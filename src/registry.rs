//! Registre des modèles acceptés, stocké en `ArcSwap<HashMap<...>>` pour
//! garantir des lectures lock-free sur le chemin chaud.
//!
//! Les mises à jour (ajout, suppression) utilisent la primitive `rcu` qui
//! retry en cas de contention — acceptable car les mises à jour sont rares
//! (quelques par heure au plus via les endpoints admin), contrairement aux
//! lectures qui se font à chaque requête gRPC.

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

    /// Retourne un snapshot atomique. Pour une requête gRPC, n'appeler `load()`
    /// qu'une seule fois et conserver le résultat dans une variable locale,
    /// sinon deux lookups peuvent tomber de part et d'autre d'une mise à jour
    /// et voir des versions incohérentes.
    pub fn snapshot(&self) -> Arc<HashMap<String, ModelSpec>> {
        self.inner.load_full()
    }

    /// Lookup O(1) sur le snapshot courant. Renvoie une copie du `ModelSpec`
    /// pour éviter d'allonger la durée de vie du snapshot à travers l'API.
    pub fn get(&self, id: &str) -> Option<ModelSpec> {
        self.inner.load().get(id).cloned()
    }

    /// Ajoute ou remplace un modèle. Retry implicite sur contention via rcu.
    pub fn upsert(&self, id: String, spec: ModelSpec) {
        self.inner.rcu(|cur| {
            let mut new: HashMap<String, ModelSpec> = (**cur).clone();
            new.insert(id.clone(), spec.clone());
            new
        });
    }

    /// Supprime un modèle. Retourne `true` si le modèle existait.
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

    /// Liste des modèles actuellement enregistrés (copie).
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

// --- Tests synchrones -------------------------------------------------------

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
        // Scénario : on prend un snapshot, on mute le registre, le snapshot
        // doit toujours refléter l'état au moment du load().
        let r = Registry::new(HashMap::new());
        r.upsert("m1".into(), spec(100));

        let snap = r.snapshot();
        r.upsert("m1".into(), spec(200));
        r.upsert("m2".into(), spec(300));

        // Le snapshot capturé avant les updates ne voit que m1@100.
        assert_eq!(snap.len(), 1);
        assert_eq!(snap.get("m1").unwrap().dim, 100);

        // L'état courant voit les mises à jour.
        assert_eq!(r.get("m1").unwrap().dim, 200);
        assert_eq!(r.get("m2").unwrap().dim, 300);
    }

    #[test]
    fn concurrent_readers_and_writer_consistent() {
        // Test de charge synchrone : plusieurs lecteurs vs un writer,
        // synchronisés via Barrier (pas de sleep, conformément aux règles).
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
                    // Le modèle "stable" existe toujours avec dim == 42 ;
                    // le writer n'y touche jamais. Les lectures doivent
                    // systématiquement voir cette valeur cohérente.
                    let got = r.get("stable").expect("stable toujours présent");
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

        // Le modèle "stable" est toujours là après toute la séquence.
        assert_eq!(r.get("stable").unwrap().dim, 42);
    }
}

// --- Test loom --------------------------------------------------------------
//
// Le test loom vérifie le pattern "snapshot lock-free + update atomique" sur
// un modèle simplifié qui utilise les primitives instrumentées de loom.
// `arc-swap` a sa propre couverture loom interne ; ici on valide notre logique
// (lecture de snapshot + cycle read-modify-write côté écrivain).
//
// Exécution :  RUSTFLAGS='--cfg loom' cargo test --release --lib registry_loom
#[cfg(loom)]
mod registry_loom {
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    #[test]
    fn snapshot_is_consistent_across_update() {
        loom::model(|| {
            // Modèle : Mutex<Arc<Vec<u32>>>.
            // Lecteur : prend le lock brièvement pour cloner l'Arc, puis lit.
            // Écrivain : prend le lock, clone l'Arc intérieur, modifie, remplace.
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

            // Snapshot via lock court + clone de l'Arc.
            let snap = {
                let g = state.lock().unwrap();
                g.clone()
            };

            // Invariant : peu importe l'entrelacement, le snapshot contient
            // soit l'ancien état (len=3), soit le nouveau (len=4),
            // jamais un état intermédiaire corrompu.
            assert!(snap.len() == 3 || snap.len() == 4);
            assert_eq!(snap[0], 1);
            assert_eq!(snap[1], 2);
            assert_eq!(snap[2], 3);

            writer.join().unwrap();
        });
    }
}
