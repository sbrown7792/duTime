-- duTime schema v1
--
-- CORE INVARIANT — read this before touching anything below.
--
-- `size_event.d_bytes` / `d_blocks` mean EXACTLY ONE THING:
--
--     "this row's contribution to the inclusive delta of itself and every one
--      of its ancestors, at this scan"
--
-- For a normal change that equals the node's EXCLUSIVE delta (own_bytes now
-- minus own_bytes at its previous event). For a SUBTREE_GONE row it equals the
-- whole subtree's INCLUSIVE delta, because the descendants deliberately emit no
-- rows of their own (deleting node_modules/ with 30k dirs must not write 30k
-- tombstones). Both the ancestor-rollup "biggest gainers" query and the
-- checkpoint+delta "size at time T" reconstruction stay correct without
-- special-casing, because a SUBTREE_GONE row sits inside every affected
-- ancestor's subtree exactly once.
--
-- The invariant to property-test:
--     SUM(d_bytes) over subtree(A) for scans in (S1, S2]  ==  incl(A,S2) - incl(A,S1)
--
-- Sizes are stored EXCLUSIVE ("own"). Inclusive values are computed by rollup.
-- Rationale: mean directory depth is ~9.3, so storing inclusive sizes dirties
-- ~9.3 rows per single file write and rewrites the root's row on every scan.
-- Measured: 55 exclusive events/scan vs ~510 inclusive. The rollup that recovers
-- inclusive values costs churn x depth additions -- microseconds.
--
-- Every size is recorded twice: `*_bytes` is apparent (sum of st_size) and
-- `*_blocks` is allocated (sum of st_blocks * 512). They diverge on sparse files
-- and on small-file tail slack; that divergence is signal, not noise.

-- ─────────────────────────── roots ───────────────────────────
CREATE TABLE IF NOT EXISTS root (
  root_id              INTEGER PRIMARY KEY,
  path                 BLOB    NOT NULL UNIQUE,  -- raw OS bytes, absolute
  dev                  INTEGER,                  -- st_dev at registration
  enabled              INTEGER NOT NULL DEFAULT 1,
  interval_s           INTEGER NOT NULL DEFAULT 3600,
  track_file_min_bytes INTEGER NOT NULL DEFAULT 1048576,
  one_filesystem       INTEGER NOT NULL DEFAULT 1
) STRICT;

-- ──────────────────── scans: the discrete time axis ────────────────────
-- scan_id is the TRUE ordering axis. Never sort history by wall clock: NTP
-- steps and DST make started_at non-monotonic.
CREATE TABLE IF NOT EXISTS scan (
  scan_id     INTEGER PRIMARY KEY,
  root_id     INTEGER NOT NULL REFERENCES root(root_id),
  started_at  INTEGER NOT NULL,          -- unix epoch seconds, UTC
  ended_at    INTEGER,
  duration_ms INTEGER,
  tier        INTEGER NOT NULL DEFAULT 0, -- 0=raw 1=hourly 2=daily 3=monthly
  n_dirs      INTEGER,
  n_files     INTEGER,
  n_entities  INTEGER,
  n_events    INTEGER,
  incl_bytes  INTEGER,                    -- root totals: free, drives the overview sparkline
  incl_blocks INTEGER,
  fs_total    INTEGER,                    -- statvfs at scan time -> "unaccounted" bucket
  fs_free     INTEGER,
  fs_avail    INTEGER,
  status      TEXT    NOT NULL,           -- ok|partial|aborted|overrun|error
  err         TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS scan_time ON scan(root_id, started_at);

-- ──────────────── path dictionary (parent_id + name) ────────────────
-- Paths are stored once as a tree, not as strings: average full path here is
-- 79 chars but the average single name component is ~12.
--
-- `name` is BLOB, not TEXT, and that is deliberate: Linux filenames are
-- arbitrary byte strings and are NOT required to be valid UTF-8. Storing them
-- as TEXT corrupts them and makes the system 500 on one user's oddly-named file
-- two years in.
--
-- A location that is deleted and later recreated gets a NEW path_id (a new
-- "incarnation"), so history never conflates two different things that happened
-- to share a name.
CREATE TABLE IF NOT EXISTS path (
  path_id   INTEGER PRIMARY KEY,
  parent_id INTEGER REFERENCES path(path_id),  -- NULL only for a configured root
  name      BLOB    NOT NULL,
  root_id   INTEGER NOT NULL REFERENCES root(root_id),
  depth     INTEGER NOT NULL,
  kind      INTEGER NOT NULL,  -- 0=dir 1=file 2=symlink 3=other
  born_scan INTEGER NOT NULL,
  died_scan INTEGER,           -- NULL = currently live
  ino       INTEGER,           -- rename/move detection + hardlink dedup
  dev       INTEGER
) STRICT;
-- Uniqueness applies only among LIVE rows, so recreation is legal.
CREATE UNIQUE INDEX IF NOT EXISTS path_live   ON path(parent_id, name) WHERE died_scan IS NULL;
CREATE        INDEX IF NOT EXISTS path_parent ON path(parent_id);
CREATE        INDEX IF NOT EXISTS path_ino    ON path(dev, ino) WHERE died_scan IS NULL;
CREATE        INDEX IF NOT EXISTS path_root   ON path(root_id) WHERE died_scan IS NULL;

-- ─────────────── change-only measurements ───────────────
-- The absence of a row for a scan means "unchanged since this path's previous
-- row" (carry-forward semantics). This is what makes the whole thing cheap:
-- ~55 rows/scan instead of ~126,000.
CREATE TABLE IF NOT EXISTS size_event (
  path_id    INTEGER NOT NULL,
  scan_id    INTEGER NOT NULL,
  own_bytes  INTEGER NOT NULL,  -- absolute, apparent
  own_blocks INTEGER NOT NULL,  -- absolute, allocated
  own_files  INTEGER NOT NULL,  -- count of direct untracked-small children
  d_bytes    INTEGER NOT NULL,  -- see CORE INVARIANT at top of file
  d_blocks   INTEGER NOT NULL,
  flags      INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (path_id, scan_id)
) WITHOUT ROWID, STRICT;
-- Clustered by (path_id, scan_id): "full history of path P" is one contiguous
-- range scan. The secondary index serves the time-window scans that drive
-- gainers/diff.
CREATE INDEX IF NOT EXISTS size_event_by_scan ON size_event(scan_id, path_id);

-- ─────────── materialized current state ───────────
-- The absolute exclusive values as of the most recent scan, for every live
-- entity. One row per live path; ~126k rows, a few MB.
--
-- This is a materialization of "the latest size_event per path", and it exists
-- because computing that honestly is the one genuinely expensive query in the
-- design. `size_event` is clustered by (path_id, scan_id), so "last row per
-- path" still has to touch every historical row -- after a year that is
-- millions of rows scanned to answer a question about 126k paths, on the hot
-- path of every single scan.
--
-- Kept in the same transaction as the events it summarizes, so the two cannot
-- diverge. `dutime doctor` re-derives it from scratch and compares.
CREATE TABLE IF NOT EXISTS current_size (
  path_id    INTEGER PRIMARY KEY,
  own_bytes  INTEGER NOT NULL,
  own_blocks INTEGER NOT NULL,
  own_files  INTEGER NOT NULL
) WITHOUT ROWID, STRICT;

-- ─────────── materialized inclusive checkpoints (keyframes) ───────────
-- Bounds "as of T" reconstruction: without these, reconstructing a size means
-- replaying every event since the beginning of time. Written every
-- `checkpoint_every_scans` (default 288 = daily at 5min) and only for entities
-- over `checkpoint_min_bytes`, so ~6k rows/day.
--
-- A checkpoint is ALWAYS written on a root's first scan, so the slow cold path
-- (last-value-per-path over all history) never runs in production.
CREATE TABLE IF NOT EXISTS checkpoint (
  path_id     INTEGER NOT NULL,
  scan_id     INTEGER NOT NULL,
  incl_bytes  INTEGER NOT NULL,
  incl_blocks INTEGER NOT NULL,
  incl_files  INTEGER NOT NULL,
  incl_dirs   INTEGER NOT NULL,
  PRIMARY KEY (path_id, scan_id)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS checkpoint_by_scan ON checkpoint(scan_id);

-- ─────────── detected moves (heuristic, annotation layer) ───────────
-- duTime tracks LOCATIONS, not inodes -- "/var/log/big.log is growing" is the
-- question users actually ask, so a move is honestly reported as a drop here
-- and a rise there. On top of that we join vanished<->appeared entities on
-- (dev, ino) within a single scan to explain it: "-4.2 GB (likely moved to
-- /mnt/archive/...)". confidence < 1.0 when inode reuse is suspected.
CREATE TABLE IF NOT EXISTS move_event (
  scan_id      INTEGER NOT NULL,
  from_path_id INTEGER NOT NULL,
  to_path_id   INTEGER NOT NULL,
  bytes        INTEGER NOT NULL,
  confidence   REAL    NOT NULL,
  PRIMARY KEY (scan_id, from_path_id)
) WITHOUT ROWID, STRICT;

-- ─────────── timeline annotations (apt history, boots, manual) ───────────
CREATE TABLE IF NOT EXISTS annotation (
  id     INTEGER PRIMARY KEY,
  at     INTEGER NOT NULL,
  kind   TEXT,
  label  TEXT,
  detail TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS annotation_at ON annotation(at);

-- ─────────── bookkeeping ───────────
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v ANY) STRICT;
