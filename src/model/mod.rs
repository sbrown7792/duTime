//! Core domain types shared by the scanner, the store and the query layer.

pub mod tree;

use std::ffi::OsString;

pub type PathId = i64;
pub type ScanId = i64;
pub type RootId = i64;

/// What kind of filesystem object a dictionary entry describes.
///
/// Only `Dir` and `File` are ever *tracked entities* (things that carry sizes).
/// Symlinks and specials are recorded so the tree is complete and so their own
/// inode size is accounted, but they are never recursed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i64)]
pub enum Kind {
    Dir = 0,
    File = 1,
    Symlink = 2,
    Other = 3,
}

impl Kind {
    pub fn from_i64(v: i64) -> Option<Self> {
        match v {
            0 => Some(Kind::Dir),
            1 => Some(Kind::File),
            2 => Some(Kind::Symlink),
            3 => Some(Kind::Other),
            _ => None,
        }
    }
}

/// Bit flags on a [`SizeEvent`].
///
/// `SUBTREE_GONE` is the load-bearing one: see the CORE INVARIANT comment at the
/// top of `schema.sql`.
pub mod flags {
    pub const BORN: i64 = 1;
    /// This entity was deleted; `own_*` are zero.
    pub const GONE: i64 = 2;
    /// This entity *and its whole subtree* were deleted. `d_bytes` carries the
    /// entire subtree's inclusive delta and descendants emit no rows at all.
    pub const SUBTREE_GONE: i64 = 4;
    /// `st_blocks * 512` is materially below `st_size`.
    pub const SPARSE: i64 = 8;
    /// A hardlink whose bytes were credited to another path (du semantics).
    pub const HARDLINK_DEDUPED: i64 = 16;
}

/// One measured entity produced by a single walk.
///
/// `own_bytes` for a directory is its own inode size plus the sizes of all
/// non-directory children that are *not themselves tracked entities* (i.e. files
/// below `track_file_min_bytes`). For a promoted file entity it is just that
/// file's size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    /// Path components from (and excluding) the root, as raw OS bytes.
    pub rel: Vec<OsString>,
    pub kind: Kind,
    pub own_bytes: i64,
    pub own_blocks: i64,
    /// Direct children that are files below the tracking threshold.
    pub own_files: i64,
    pub ino: Option<i64>,
    pub dev: Option<i64>,
    pub flags: i64,
}

/// A change-only measurement row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeEvent {
    pub path_id: PathId,
    pub scan_id: ScanId,
    pub own_bytes: i64,
    pub own_blocks: i64,
    pub own_files: i64,
    /// Contribution to the inclusive delta of self + every ancestor.
    pub d_bytes: i64,
    pub d_blocks: i64,
    pub flags: i64,
}

/// Which of the two size metrics a query is asking about.
///
/// These are tracked separately everywhere because their divergence is useful
/// signal: sparse files have `blocks << bytes`, and a pile of tiny files has
/// `blocks > bytes` from per-file tail slack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Sum of `st_size` — what the files claim to contain.
    Apparent,
    /// Sum of `st_blocks * 512` — what the filesystem actually spent.
    Allocated,
}

/// Whether a size includes descendants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// This node's own files only. Pinpoints *which* directory grew.
    Exclusive,
    /// This node plus its entire subtree.
    Inclusive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanSummary {
    pub scan_id: ScanId,
    pub root_id: RootId,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub n_dirs: i64,
    pub n_files: i64,
    pub n_entities: i64,
    pub n_events: i64,
    pub incl_bytes: i64,
    pub incl_blocks: i64,
    pub fs_total: Option<i64>,
    pub fs_free: Option<i64>,
    pub fs_avail: Option<i64>,
    pub status: String,
}
