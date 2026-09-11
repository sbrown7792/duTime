//! Shared server state: the database handle and the snapshot cache.

use crate::model::{RootId, ScanId};
use crate::store::Store;
use crate::store::snapshot::Snapshot;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// How many materialized snapshots to keep. Each is ~10 MB for a 127k-entity
/// tree, so this is a bounded tens-of-MB cache in exchange for making the
/// time slider feel instant when scrubbing back and forth.
const MAX_CACHED_SNAPSHOTS: usize = 6;

pub struct AppState {
    pub store: Mutex<Store>,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    map: HashMap<(RootId, ScanId), Arc<Snapshot>>,
    /// Insertion order, oldest first.
    order: Vec<(RootId, ScanId)>,
}

impl AppState {
    pub fn new(store: Store) -> Self {
        Self { store: Mutex::new(store), cache: Mutex::new(Cache::default()) }
    }

    /// A materialized tree for an instant, cached.
    pub fn snapshot(&self, root: RootId, scan: ScanId) -> Result<Arc<Snapshot>> {
        if let Some(s) = self.cache.lock().unwrap().map.get(&(root, scan)) {
            return Ok(s.clone());
        }
        // Deliberately built outside the cache lock: loading a historical
        // snapshot can take a moment, and holding the lock would serialize
        // every other request behind it.
        let built = {
            let store = self.store.lock().unwrap();
            Arc::new(Snapshot::load(&store, root, scan)?)
        };

        let mut c = self.cache.lock().unwrap();
        if c.map.insert((root, scan), built.clone()).is_none() {
            c.order.push((root, scan));
            while c.order.len() > MAX_CACHED_SNAPSHOTS {
                let victim = c.order.remove(0);
                c.map.remove(&victim);
            }
        }
        Ok(built)
    }

    /// Drop cached snapshots after a scan, so the live view reflects it.
    ///
    /// Only the newest entry can go stale — historical snapshots are immutable
    /// once their scan is committed — but clearing everything is cheap and
    /// leaves no room for a subtle staleness bug.
    pub fn invalidate(&self) {
        let mut c = self.cache.lock().unwrap();
        c.map.clear();
        c.order.clear();
    }
}
