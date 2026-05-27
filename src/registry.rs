//! Registry of accepted models, stored as `ArcSwap<HashMap<...>>` to
//! guarantee lock-free reads on the hot path.
//!
//! Updates (add, remove) use the `rcu` primitive which retries on
//! contention — acceptable because updates are rare (a few per hour at
//! most via admin endpoints), unlike reads which happen on every gRPC
//! request.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwap;

use crate::config::ModelSpec;

pub struct Registry {
    inner: ArcSwap<HashMap<String, ModelSpec>>,
}

impl Registry {
    pub fn new(initial: HashMap<String, ModelSpec>) -> Self {
        Self {
            inner: ArcSwap::from_pointee(initial),
        }
    }

    /// Returns an atomic snapshot. For a gRPC request, only call `load()`
    /// once and keep the result in a local variable; otherwise two lookups
    /// can fall on either side of an update and see inconsistent versions.
    pub fn snapshot(&self) -> Arc<HashMap<String, ModelSpec>> {
        self.inner.load_full()
    }

    /// O(1) lookup on the current snapshot. Returns a copy of the
    /// `ModelSpec` to avoid extending the snapshot's lifetime through the
    /// API.
    pub fn get(&self, id: &str) -> Option<ModelSpec> {
        self.inner.load().get(id).cloned()
    }

    /// Adds or replaces a model. Implicit retry on contention via rcu.
    pub fn upsert(&self, id: String, spec: ModelSpec) {
        self.inner.rcu(|cur| {
            let mut new: HashMap<String, ModelSpec> = (**cur).clone();
            new.insert(id.clone(), spec.clone());
            new
        });
    }

    /// Removes a model. Returns `true` if the model existed.
    pub fn remove(&self, id: &str) -> bool {
        let existed = AtomicBool::new(false);
        self.inner.rcu(|cur| {
            let mut new: HashMap<String, ModelSpec> = (**cur).clone();
            let was_there = new.remove(id).is_some();
            existed.store(was_there, Ordering::Relaxed);
            new
        });
        existed.load(Ordering::Relaxed)
    }

    /// List of currently registered models (copy).
    pub fn list(&self) -> Vec<(String, ModelSpec)> {
        self.inner
            .load()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// --- Synchronous tests ------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(dim: usize) -> ModelSpec {
        ModelSpec {
            dim,
            normalize: true,
            vdb_namespace: format!("ns-{dim}"),
        }
    }

    #[test]
    fn empty_registry_returns_none() {
        let r = Registry::new(HashMap::new());
        assert!(r.is_empty());
        assert!(r.get("any").is_none());
    }

    #[test]
    fn upsert_then_get() {
        let r = Registry::new(HashMap::new());
        r.upsert("m1".into(), spec(1536));
        assert_eq!(r.len(), 1);
        assert_eq!(r.get("m1"), Some(spec(1536)));
    }

    #[test]
    fn upsert_overwrites_existing() {
        let r = Registry::new(HashMap::new());
        r.upsert("m1".into(), spec(1024));
        r.upsert("m1".into(), spec(1536));
        assert_eq!(r.get("m1"), Some(spec(1536)));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn remove_existing_returns_true() {
        let r = Registry::new(HashMap::new());
        r.upsert("m1".into(), spec(512));
        assert!(r.remove("m1"));
        assert!(r.get("m1").is_none());
    }

    #[test]
    fn remove_absent_returns_false() {
        let r = Registry::new(HashMap::new());
        assert!(!r.remove("nope"));
    }

    #[test]
    fn list_returns_all_entries() {
        let r = Registry::new(HashMap::new());
        r.upsert("a".into(), spec(256));
        r.upsert("b".into(), spec(512));
        let mut l = r.list();
        l.sort_by_key(|(k, _)| k.clone());
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].0, "a");
        assert_eq!(l[1].0, "b");
    }

    #[test]
    fn snapshot_is_stable_during_update() {
        // Scenario: take a snapshot, mutate the registry; the snapshot must
        // still reflect the state at the moment of load().
        let r = Registry::new(HashMap::new());
        r.upsert("m1".into(), spec(100));

        let snap = r.snapshot();
        r.upsert("m1".into(), spec(200));
        r.upsert("m2".into(), spec(300));

        // The snapshot captured before the updates only sees m1@100.
        assert_eq!(snap.len(), 1);
        assert_eq!(snap.get("m1").unwrap().dim, 100);

        // The current state sees the updates.
        assert_eq!(r.get("m1").unwrap().dim, 200);
        assert_eq!(r.get("m2").unwrap().dim, 300);
    }

    #[test]
    fn concurrent_readers_and_writer_consistent() {
        // Synchronous load test: multiple readers vs one writer, synchronized
        // via Barrier (no sleep, per project rules).
        use std::sync::Barrier;
        use std::thread;

        let r = Arc::new(Registry::new(HashMap::new()));
        r.upsert("stable".into(), spec(42));

        const N_READERS: usize = 4;
        const N_ITER: usize = 500;

        let start = Arc::new(Barrier::new(N_READERS + 1));
        let mut handles = Vec::new();

        for _ in 0..N_READERS {
            let r = r.clone();
            let b = start.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                for _ in 0..N_ITER {
                    // The "stable" model always exists with dim == 42; the
                    // writer never touches it. Reads must consistently see
                    // this coherent value.
                    let got = r.get("stable").expect("stable always present");
                    assert_eq!(got.dim, 42);
                }
            }));
        }

        let writer = {
            let r = r.clone();
            let b = start.clone();
            thread::spawn(move || {
                b.wait();
                for i in 0..N_ITER {
                    r.upsert(format!("m{i}"), spec(i));
                    if i % 3 == 0 {
                        r.remove(&format!("m{}", i / 2));
                    }
                }
            })
        };

        for h in handles {
            h.join().expect("reader panic");
        }
        writer.join().expect("writer panic");

        // The "stable" model is still there after the full sequence.
        assert_eq!(r.get("stable").unwrap().dim, 42);
    }
}

// --- loom test --------------------------------------------------------------
//
// The loom test validates the "lock-free snapshot + atomic update" pattern on
// a simplified model that uses loom's instrumented primitives. `arc-swap`
// has its own internal loom coverage; here we validate our own logic
// (snapshot read + read-modify-write cycle on the writer side).
//
// Run with:  RUSTFLAGS='--cfg loom' cargo test --release --lib registry_loom
#[cfg(loom)]
mod registry_loom {
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    #[test]
    fn snapshot_is_consistent_across_update() {
        loom::model(|| {
            // Model: Mutex<Arc<Vec<u32>>>.
            // Reader: takes the lock briefly to clone the Arc, then reads.
            // Writer: takes the lock, clones the inner Arc, mutates, swaps.
            let state = Arc::new(Mutex::new(Arc::new(vec![1u32, 2, 3])));

            let writer = {
                let state = state.clone();
                thread::spawn(move || {
                    let mut g = state.lock().unwrap();
                    let mut new = (**g).clone();
                    new.push(4);
                    *g = Arc::new(new);
                })
            };

            // Snapshot via short lock + Arc clone.
            let snap = {
                let g = state.lock().unwrap();
                g.clone()
            };

            // Invariant: regardless of interleaving, the snapshot contains
            // either the old state (len=3) or the new one (len=4), never a
            // corrupt intermediate state.
            assert!(snap.len() == 3 || snap.len() == 4);
            assert_eq!(snap[0], 1);
            assert_eq!(snap[1], 2);
            assert_eq!(snap[2], 3);

            writer.join().unwrap();
        });
    }
}
