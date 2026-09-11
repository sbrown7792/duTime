//! Persistence.
//!
//! SQLite, deliberately. Measured churn on a normal workstation is ~55 changed
//! directories per 5 minutes, which with change-only storage is ~16k rows/day —
//! roughly 231 MB for a year at full 5-minute resolution. At that volume a
//! columnar or server database solves a problem that does not exist while
//! charging real operational cost, and every hot query here is either a point
//! lookup by `path_id` or a bounded time-range scan, never a full-table
//! aggregation. See `docs/storage.md`.

pub mod commit;
pub mod query;
pub mod snapshot;

use crate::model::{Kind, PathId, RootId, ScanId};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = include_str!("schema.sql");

pub struct Store {
    pub conn: Connection,
}

/// Connection settings, applied identically to the writer and every reader.
///
/// `synchronous=NORMAL` rather than FULL: with WAL that is durable against a
/// process crash and can only lose the last transaction to a power cut. For
/// disk-usage samples taken every few minutes that is the right trade — the
/// alternative is an fsync per commit forever, to protect a data point the
/// next scan reproduces anyway.
fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 10_000)?;
    conn.pragma_update(None, "wal_autocheckpoint", 2000)?;
    conn.pragma_update(None, "cache_size", -65_536)?; // 64 MiB
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    conn.pragma_update(None, "mmap_size", 1_073_741_824i64)?;
    Ok(())
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).ok();
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        Self::from_conn(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        apply_pragmas(&conn)?;
        let mut s = Self { conn };
        s.migrate()?;
        Ok(s)
    }

    /// Open an additional connection for reading.
    ///
    /// WAL lets any number of readers run concurrently with the single writer,
    /// neither blocking the other — but only across *separate connections*.
    /// Sharing one connection behind a mutex throws that away and serializes
    /// the web UI behind the scanner's commit.
    ///
    /// Deliberately not opened `SQLITE_OPEN_READ_ONLY`: a read-only connection
    /// to a WAL database still needs to write the `-shm` index, which turns a
    /// perfectly ordinary setup into a confusing "attempt to write a readonly
    /// database" at runtime. Nothing in the API layer issues a write.
    pub fn open_reader(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .with_context(|| format!("opening database {}", path.as_ref().display()))?;
        apply_pragmas(&conn)?;
        Ok(Self { conn })
    }

    /// Forward-only migrations keyed on `meta.schema_version`.
    fn migrate(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(SCHEMA)?;
        let current: Option<i64> = tx
            .query_row("SELECT v FROM meta WHERE k = 'schema_version'", [], |r| r.get(0))
            .optional()?;
        match current {
            None => {
                tx.execute(
                    "INSERT INTO meta (k, v) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION],
                )?;
            }
            Some(v) if v > SCHEMA_VERSION => {
                anyhow::bail!(
                    "database schema is version {v}, but this build only understands \
                     {SCHEMA_VERSION}. Upgrade duTime or point at a different database."
                );
            }
            Some(_) => {}
        }
        tx.commit()?;
        Ok(())
    }

    // ── roots ────────────────────────────────────────────────────────────

    /// Register a root, or return the existing one. Paths are stored as raw OS
    /// bytes because Linux paths are not required to be UTF-8.
    pub fn ensure_root(&self, path: &Path) -> Result<RootId> {
        let bytes = path.as_os_str().as_bytes();
        if let Some(id) = self
            .conn
            .query_row("SELECT root_id FROM root WHERE path = ?1", params![bytes], |r| r.get(0))
            .optional()?
        {
            return Ok(id);
        }
        self.conn
            .execute("INSERT INTO root (path) VALUES (?1)", params![bytes])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn root_path(&self, root_id: RootId) -> Result<std::path::PathBuf> {
        let b: Vec<u8> =
            self.conn
                .query_row("SELECT path FROM root WHERE root_id = ?1", params![root_id], |r| {
                    r.get(0)
                })?;
        Ok(std::path::PathBuf::from(OsString::from_vec(b)))
    }

    pub fn roots(&self) -> Result<Vec<(RootId, std::path::PathBuf)>> {
        let mut st = self.conn.prepare("SELECT root_id, path FROM root ORDER BY root_id")?;
        let rows = st.query_map([], |r| {
            let id: RootId = r.get(0)?;
            let b: Vec<u8> = r.get(1)?;
            Ok((id, std::path::PathBuf::from(OsString::from_vec(b))))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ── scans ────────────────────────────────────────────────────────────

    pub fn last_scan(&self, root_id: RootId) -> Result<Option<ScanId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT MAX(scan_id) FROM scan WHERE root_id = ?1 AND status = 'ok'",
                params![root_id],
                |r| r.get::<_, Option<ScanId>>(0),
            )
            .optional()?
            .flatten())
    }

    /// The oldest recorded scan for a root — the point before which duTime
    /// simply has no knowledge.
    pub fn first_scan(&self, root_id: RootId) -> Result<Option<(ScanId, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT scan_id, started_at FROM scan
                 WHERE root_id = ?1 AND status = 'ok' ORDER BY scan_id ASC LIMIT 1",
                params![root_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn scan_count(&self, root_id: RootId) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM scan WHERE root_id = ?1 AND status = 'ok'",
            params![root_id],
            |r| r.get(0),
        )?)
    }
}

/// One live entry in the on-disk path dictionary.
#[derive(Debug, Clone)]
pub struct LivePath {
    pub path_id: PathId,
    pub parent_id: Option<PathId>,
    pub name: OsString,
    pub kind: Kind,
    pub own_bytes: i64,
    pub own_blocks: i64,
    pub own_files: i64,
}

/// The live dictionary for one root, loaded into memory.
///
/// This is what makes the commit path cheap: reconciling a fresh walk against
/// the stored tree is a hash lookup per node rather than a query per node.
#[derive(Debug, Default)]
pub struct LiveDict {
    pub by_id: HashMap<PathId, LivePath>,
    /// `(parent_id, name)` → `path_id`. `parent_id` is `-1` for the root.
    pub by_parent_name: HashMap<(i64, OsString), PathId>,
    pub children: HashMap<i64, Vec<PathId>>,
    pub root_path_id: Option<PathId>,
}

impl LiveDict {
    pub fn load(store: &Store, root_id: RootId) -> Result<Self> {
        let mut d = LiveDict::default();
        let mut st = store.conn.prepare(
            "SELECT p.path_id, p.parent_id, p.name, p.kind,
                    COALESCE(c.own_bytes, 0), COALESCE(c.own_blocks, 0), COALESCE(c.own_files, 0)
             FROM path p
             LEFT JOIN current_size c ON c.path_id = p.path_id
             WHERE p.root_id = ?1 AND p.died_scan IS NULL",
        )?;
        let rows = st.query_map(params![root_id], |r| {
            let name: Vec<u8> = r.get(2)?;
            Ok(LivePath {
                path_id: r.get(0)?,
                parent_id: r.get(1)?,
                name: OsString::from_vec(name),
                kind: Kind::from_i64(r.get(3)?).unwrap_or(Kind::Other),
                own_bytes: r.get(4)?,
                own_blocks: r.get(5)?,
                own_files: r.get(6)?,
            })
        })?;
        for row in rows {
            let lp = row?;
            let pkey = lp.parent_id.unwrap_or(-1);
            if lp.parent_id.is_none() {
                d.root_path_id = Some(lp.path_id);
            }
            d.by_parent_name.insert((pkey, lp.name.clone()), lp.path_id);
            d.children.entry(pkey).or_default().push(lp.path_id);
            d.by_id.insert(lp.path_id, lp);
        }
        Ok(d)
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Every descendant of `id`, including `id` itself.
    pub fn subtree(&self, id: PathId) -> Vec<PathId> {
        let mut out = vec![id];
        let mut i = 0;
        while i < out.len() {
            if let Some(kids) = self.children.get(&out[i]) {
                out.extend_from_slice(kids);
            }
            i += 1;
        }
        out
    }
}
