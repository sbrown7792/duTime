//! Shared server state: the writer, a pool of readers, and the snapshot cache.
//!
//! The scanner and the web UI must not wait on each other. A scan on a large
//! server can take minutes to walk and seconds to commit, and a dashboard that
//! freezes for the duration is a dashboard nobody trusts.
//!
//! Two things make that work:
//!
//! * **WAL plus separate connections.** SQLite allows any number of concurrent
//!   readers alongside the single writer — but only across distinct
//!   connections. One `Mutex<Connection>` shared by everything throws that
//!   guarantee away and serializes every request behind the commit.
//! * **Reads happen on the blocking pool.** SQLite calls are synchronous; run
//!   them on async worker threads and a handful of slow queries starve the
//!   runtime, at which point even the health check stops answering.

use crate::model::{RootId, ScanId};
use crate::store::Store;
use crate::store::snapshot::Snapshot;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

/// How many materialized snapshots to keep. Each is roughly 10 MB for a
/// 127k-entity tree, so this is a bounded tens-of-MB cache in exchange for
/// making the time slider feel instant while scrubbing.
const MAX_CACHED_SNAPSHOTS: usize = 8;

/// Read connections. Enough for the handful of parallel requests a dashboard
/// makes, few enough that a burst cannot exhaust the blocking pool.
const READERS: usize = 4;

pub struct AppState {
    /// The scanner's connection. Held only for the duration of a commit.
    pub store: Mutex<Store>,
    readers: ReadPool,
    cache: Mutex<Cache>,
}

/// A tiny checkout pool. Not worth a dependency: this is the whole thing.
struct ReadPool {
    idle: Mutex<Vec<Store>>,
    available: Condvar,
}

/// A reader borrowed from the pool, returned on drop.
pub struct Reader<'a> {
    pool: &'a ReadPool,
    conn: Option<Store>,
}

impl std::ops::Deref for Reader<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        self.conn.as_ref().expect("reader used after return")
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            self.pool.idle.lock().unwrap().push(c);
            self.pool.available.notify_one();
        }
    }
}

impl ReadPool {
    fn get(&self) -> Reader<'_> {
        let mut idle = self.idle.lock().unwrap();
        // Only ever called from the blocking pool, so parking here costs an
        // OS thread rather than an async worker.
        while idle.is_empty() {
            idle = self.available.wait(idle).unwrap();
        }
        let conn = idle.pop();
        drop(idle);
        Reader { pool: self, conn }
    }
}

#[derive(Default)]
struct Cache {
    map: HashMap<(RootId, ScanId), Arc<Snapshot>>,
    /// Insertion order, oldest first.
    order: Vec<(RootId, ScanId)>,
}

impl AppState {
    /// In-memory state, for tests. Reads and writes share the one connection,
    /// which is fine when there is no scanner running against it.
    pub fn new(store: Store) -> Self {
        Self {
            store: Mutex::new(store),
            readers: ReadPool { idle: Mutex::new(Vec::new()), available: Condvar::new() },
            cache: Mutex::new(Cache::default()),
        }
    }

    /// Open a writer plus a pool of readers against a database on disk.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path: PathBuf = path.as_ref().to_path_buf();
        // The writer opens first so the WAL and its `-shm` index exist before
        // any reader attaches.
        let writer = Store::open(&path)?;
        let mut idle = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            idle.push(Store::open_reader(&path)?);
        }
        Ok(Self {
            store: Mutex::new(writer),
            readers: ReadPool { idle: Mutex::new(idle), available: Condvar::new() },
            cache: Mutex::new(Cache::default()),
        })
    }

    /// Borrow a connection for reading.
    ///
    /// Falls back to the writer when no pool exists (the in-memory test case),
    /// where there is no scanner to contend with.
    pub fn read(&self) -> ReadGuard<'_> {
        if self.readers.idle.lock().unwrap().is_empty()
            && self.readers.idle.lock().unwrap().capacity() == 0
        {
            return ReadGuard::Writer(self.store.lock().unwrap());
        }
        ReadGuard::Pooled(self.readers.get())
    }

    /// A materialized tree for an instant, cached.
    pub fn snapshot(&self, root: RootId, scan: ScanId) -> Result<Arc<Snapshot>> {
        if let Some(s) = self.cache.lock().unwrap().map.get(&(root, scan)) {
            return Ok(s.clone());
        }
        // Built outside the cache lock: a cold load takes long enough that
        // holding it would serialize every other request behind this one.
        let built = {
            let r = self.read();
            Arc::new(Snapshot::load(&r, root, scan)?)
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
}

/// Either a pooled reader or the writer, depending on how state was built.
pub enum ReadGuard<'a> {
    Pooled(Reader<'a>),
    Writer(std::sync::MutexGuard<'a, Store>),
}

impl std::ops::Deref for ReadGuard<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        match self {
            ReadGuard::Pooled(r) => r,
            ReadGuard::Writer(w) => w,
        }
    }
}
