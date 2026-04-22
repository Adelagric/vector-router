// Extrait public du projet vector-router — fichier fourni à titre de vitrine
// technique. Ce fichier ne se compile pas seul ; il dépend du crate complet.
// Code complet distribué commercialement (Tier 3). Contact : kaleche@gmail.com
//
// (C) 2026 Adel Kaleche. All rights reserved. Voir LICENSE à la racine du repo.

//! Pool de buffers alignés pour les cas où le `bytes` protobuf arrive
//! désaligné pour `f32`. Le pool évite les allocations sur le chemin chaud.
//!
//! Stratégie si le pool est vide : allocation ponctuelle avec incrément d'un
//! compteur `exhausted_count`. Le pool **ne bloque jamais** : un blocage
//! corrélé à la charge dégraderait le p99 exactement quand il ne faut pas.
//!
//! Alignement : les buffers sont stockés en `Box<[u32]>`, ce qui garantit un
//! alignement sur 4 octets (taille et alignement naturels de `u32` et `f32`).
//! La conversion vers `&[u8]` / `&mut [u8]` passe par `bytemuck::cast_slice`,
//! qui est safe.

use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_queue::ArrayQueue;

use crate::error::Error;

/// Buffer aligné sur 4 octets, dimensionné pour contenir jusqu'à `capacity`
/// octets. La longueur utile (`len`) est indépendante et se gère via `copy_from_slice`.
pub struct AlignedBuffer {
    // Stockage en `u32` pour garantir l'alignement 4. La capacité en octets
    // est `storage.len() * 4`.
    storage: Box<[u32]>,
    // Nombre d'octets réellement remplis (toujours ≤ capacité).
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

    /// Capacité maximale du buffer en octets.
    pub fn capacity_bytes(&self) -> usize {
        self.storage.len() * 4
    }

    /// Longueur de données utiles en octets.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Copie les octets source dans le buffer, écrase le contenu précédent.
    /// Retourne une erreur si la source dépasse la capacité.
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

    /// Vue `&[f32]` sur les octets remplis. Le stockage étant aligné 4,
    /// `try_cast_slice` ne retournera jamais d'erreur d'alignement ; l'échec
    /// possible est si `len` n'est pas multiple de 4 (ce que l'appelant doit
    /// garantir via la validation de dimension en amont).
    pub fn as_f32(&self) -> Result<&[f32], Error> {
        let all_bytes = bytemuck::cast_slice::<u32, u8>(&self.storage);
        let used = &all_bytes[..self.len];
        bytemuck::try_cast_slice::<u8, f32>(used)
            .map_err(|e| Error::Validation(format!("buffer non convertible en &[f32] : {e:?}")))
    }

    /// Remet la longueur utile à zéro. La mémoire n'est pas libérée.
    pub fn clear(&mut self) {
        self.len = 0;
    }
}

/// Default = buffer vide, utilisé comme sentinelle pour `mem::take` dans
/// le Drop de `PooledBuffer`. Zéro allocation grâce à `Vec::new()`.
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
    /// Crée un pool pré-rempli de `size` buffers, chacun de `capacity_bytes`.
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

    /// Emprunte un buffer. Ne bloque jamais : si le pool est vide, alloue un
    /// nouveau buffer et incrémente `exhausted_count` + émet la métrique
    /// Prometheus `pool_exhausted_total`.
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

    /// Nombre total de fois où le pool a dû allouer un buffer de secours.
    pub fn exhausted_count(&self) -> u64 {
        self.exhausted_count.load(Ordering::Relaxed)
    }

    /// Nombre de buffers actuellement disponibles dans le pool.
    pub fn available(&self) -> usize {
        self.queue.len()
    }

    pub fn capacity_bytes(&self) -> usize {
        self.capacity_bytes
    }
}

// --- PooledBuffer : RAII ----------------------------------------------------

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
        // `mem::take` laisse un AlignedBuffer::default() (vide, zéro alloc)
        // à la place pour pouvoir redonner le buffer utile au pool.
        let mut buf = mem::take(&mut self.buffer);
        buf.clear();
        // Si le buffer ne correspond pas à la capacité attendue (cas rare : un
        // buffer pré-alloué avant un resize de config), on ne le remet pas.
        if buf.capacity_bytes() == self.pool.capacity_bytes {
            // Si la queue est pleine (allocation fallback précédente pas encore
            // consommée par un autre preneur), le buffer est simplement droppé.
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
        // 17 octets demandés → arrondi à 20 (5 × u32).
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
        } // drop → retour en pool
        assert_eq!(pool.available(), 2);
        assert_eq!(pool.exhausted_count(), 0);
    }

    #[test]
    fn pool_exhaustion_falls_back_to_allocation() {
        let pool = BufferPool::new(1, 16);
        let b1 = pool.take();
        let b2 = pool.take(); // pool vide → fallback
        assert_eq!(pool.exhausted_count(), 1);
        drop(b1);
        drop(b2);
        // Les deux buffers tentent de revenir en pool (capacité 1), l'un est droppé.
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
        // Reprise : le buffer doit être clear-é.
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
                    // Scope implicite, drop → retour en pool (ou drop si plein).
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Après la rafale, le pool doit être plein (tous les buffers revenus).
        assert_eq!(pool.available(), 4);
    }
}
