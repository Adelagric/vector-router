//! Aligned buffer pool for cases where the protobuf `bytes` arrives
//! misaligned for `f32`. The pool avoids hot-path allocations.
//!
//! Strategy when the pool is empty: one-off allocation with an
//! `exhausted_count` increment. The pool **never blocks**: a load-correlated
//! block would degrade p99 exactly when it shouldn't.
//!
//! Alignment: buffers are stored as `Box<[u32]>`, which guarantees 4-byte
//! alignment (the natural size and alignment of `u32` and `f32`). Conversion
//! to `&[u8]` / `&mut [u8]` goes through `bytemuck::cast_slice`, which is
//! safe.

use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_queue::ArrayQueue;

use crate::error::Error;

/// 4-byte aligned buffer, sized to hold up to `capacity` bytes. The useful
/// length (`len`) is independent and managed via `copy_from_slice`.
pub struct AlignedBuffer {
    // Stored as `u32` to guarantee 4-byte alignment. Byte capacity is
    // `storage.len() * 4`.
    storage: Box<[u32]>,
    // Number of bytes actually filled (always ≤ capacity).
    len: usize,
}

impl AlignedBuffer {
    pub fn new(capacity_bytes: usize) -> Self {
        let cap_u32 = capacity_bytes.div_ceil(4);
        Self {
            storage: vec![0u32; cap_u32].into_boxed_slice(),
            len: 0,
        }
    }

    /// Maximum buffer capacity in bytes.
    pub fn capacity_bytes(&self) -> usize {
        self.storage.len() * 4
    }

    /// Length of useful data in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Copies the source bytes into the buffer, overwriting prior content.
    /// Returns an error if the source exceeds capacity.
    pub fn copy_from_slice(&mut self, src: &[u8]) -> Result<(), Error> {
        if src.len() > self.capacity_bytes() {
            return Err(Error::Validation(format!(
                "payload ({} octets) dépasse la capacité du buffer ({} octets)",
                src.len(),
                self.capacity_bytes(),
            )));
        }
        let dst = bytemuck::cast_slice_mut::<u32, u8>(&mut self.storage);
        dst[..src.len()].copy_from_slice(src);
        self.len = src.len();
        Ok(())
    }

    /// `&[f32]` view over the filled bytes. Storage being 4-aligned,
    /// `try_cast_slice` will never return an alignment error; the possible
    /// failure is if `len` is not a multiple of 4 (which the caller must
    /// guarantee via upstream dimension validation).
    pub fn as_f32(&self) -> Result<&[f32], Error> {
        let all_bytes = bytemuck::cast_slice::<u32, u8>(&self.storage);
        let used = &all_bytes[..self.len];
        bytemuck::try_cast_slice::<u8, f32>(used)
            .map_err(|e| Error::Validation(format!("buffer non convertible en &[f32] : {e:?}")))
    }

    /// Resets the useful length to zero. Memory is not freed.
    pub fn clear(&mut self) {
        self.len = 0;
    }
}

/// Default = empty buffer, used as a sentinel for `mem::take` in
/// `PooledBuffer`'s Drop. Zero allocation thanks to `Vec::new()`.
impl Default for AlignedBuffer {
    fn default() -> Self {
        Self {
            storage: Vec::new().into_boxed_slice(),
            len: 0,
        }
    }
}

// --- Pool -------------------------------------------------------------------

pub struct BufferPool {
    queue: ArrayQueue<AlignedBuffer>,
    capacity_bytes: usize,
    exhausted_count: AtomicU64,
}

impl BufferPool {
    /// Creates a pool pre-filled with `size` buffers, each of `capacity_bytes`.
    pub fn new(size: usize, capacity_bytes: usize) -> Self {
        let queue = ArrayQueue::new(size.max(1));
        for _ in 0..size {
            let _ = queue.push(AlignedBuffer::new(capacity_bytes));
        }
        Self {
            queue,
            capacity_bytes,
            exhausted_count: AtomicU64::new(0),
        }
    }

    /// Borrows a buffer. Never blocks: if the pool is empty, allocates a
    /// new buffer and increments `exhausted_count` + emits the Prometheus
    /// metric `pool_exhausted_total`.
    pub fn take(&self) -> PooledBuffer<'_> {
        let buffer = match self.queue.pop() {
            Some(b) => b,
            None => {
                self.exhausted_count.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("pool_exhausted_total").increment(1);
                AlignedBuffer::new(self.capacity_bytes)
            }
        };
        PooledBuffer { buffer, pool: self }
    }

    /// Total number of times the pool had to allocate a fallback buffer.
    pub fn exhausted_count(&self) -> u64 {
        self.exhausted_count.load(Ordering::Relaxed)
    }

    /// Number of buffers currently available in the pool.
    pub fn available(&self) -> usize {
        self.queue.len()
    }

    pub fn capacity_bytes(&self) -> usize {
        self.capacity_bytes
    }
}

// --- PooledBuffer: RAII -----------------------------------------------------

pub struct PooledBuffer<'p> {
    buffer: AlignedBuffer,
    pool: &'p BufferPool,
}

impl<'p> std::ops::Deref for PooledBuffer<'p> {
    type Target = AlignedBuffer;
    fn deref(&self) -> &AlignedBuffer {
        &self.buffer
    }
}

impl<'p> std::ops::DerefMut for PooledBuffer<'p> {
    fn deref_mut(&mut self) -> &mut AlignedBuffer {
        &mut self.buffer
    }
}

impl<'p> Drop for PooledBuffer<'p> {
    fn drop(&mut self) {
        // `mem::take` leaves an AlignedBuffer::default() (empty, zero alloc)
        // in place so we can hand the useful buffer back to the pool.
        let mut buf = mem::take(&mut self.buffer);
        buf.clear();
        // If the buffer no longer matches the expected capacity (rare case:
        // a buffer pre-allocated before a config resize), don't return it.
        if buf.capacity_bytes() == self.pool.capacity_bytes {
            // If the queue is full (a previous fallback allocation not yet
            // consumed by another taker), the buffer is simply dropped.
            let _ = self.pool.queue.push(buf);
        }
    }
}

// --- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_is_4_byte_aligned() {
        let buf = AlignedBuffer::new(16);
        let ptr = buf.storage.as_ptr() as usize;
        assert_eq!(ptr % 4, 0, "pointeur de stockage non aligné sur 4 : {ptr}");
    }

    #[test]
    fn buffer_capacity_rounded_up_to_4() {
        // 17 bytes requested → rounded to 20 (5 × u32).
        let buf = AlignedBuffer::new(17);
        assert_eq!(buf.capacity_bytes(), 20);
    }

    #[test]
    fn copy_and_as_f32_roundtrip() {
        let mut buf = AlignedBuffer::new(16);
        let raw: [u8; 8] = [0x00, 0x00, 0x80, 0x3f, 0x00, 0x00, 0x00, 0x40];
        buf.copy_from_slice(&raw).expect("copie");
        let floats = buf.as_f32().expect("cast");
        assert_eq!(floats, &[1.0f32, 2.0f32]);
    }

    #[test]
    fn copy_rejects_oversize() {
        let mut buf = AlignedBuffer::new(8);
        let too_big = vec![0u8; 16];
        assert!(buf.copy_from_slice(&too_big).is_err());
    }

    #[test]
    fn clear_resets_len() {
        let mut buf = AlignedBuffer::new(16);
        buf.copy_from_slice(&[0u8; 8]).unwrap();
        assert_eq!(buf.len(), 8);
        buf.clear();
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn pool_returns_buffer_on_drop() {
        let pool = BufferPool::new(2, 16);
        assert_eq!(pool.available(), 2);
        {
            let _b = pool.take();
            assert_eq!(pool.available(), 1);
        } // drop → returned to pool
        assert_eq!(pool.available(), 2);
        assert_eq!(pool.exhausted_count(), 0);
    }

    #[test]
    fn pool_exhaustion_falls_back_to_allocation() {
        let pool = BufferPool::new(1, 16);
        let b1 = pool.take();
        let b2 = pool.take(); // pool empty → fallback
        assert_eq!(pool.exhausted_count(), 1);
        drop(b1);
        drop(b2);
        // Both buffers try to return to the pool (capacity 1), one is dropped.
        assert_eq!(pool.available(), 1);
    }

    #[test]
    fn buffer_can_be_reused_via_pool() {
        let pool = BufferPool::new(1, 8);
        {
            let mut b = pool.take();
            b.copy_from_slice(&[1u8, 2, 3, 4]).unwrap();
            assert_eq!(b.len(), 4);
        }
        // Reuse: the buffer must have been cleared.
        let b = pool.take();
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn take_under_concurrent_load() {
        use std::sync::Arc;
        use std::sync::Barrier;
        use std::thread;

        let pool = Arc::new(BufferPool::new(4, 64));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = pool.clone();
            let b = barrier.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                for _ in 0..100 {
                    let _buf = p.take();
                    // Implicit scope, drop → return to pool (or drop if full).
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // After the burst, the pool must be full (all buffers returned).
        assert_eq!(pool.available(), 4);
    }
}
