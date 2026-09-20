//! A reserve of disk space, held so that a full disk stays diagnosable.
//!
//! When the filesystem under the database reaches zero bytes free, SQLite
//! cannot write — which is expected — but it also cannot *open*. In WAL mode
//! every connection has to create and size a 32 KiB `-shm` index before it can
//! read a single row, and that needs blocks. Measured on a tmpfs filled to
//! exactly zero, against a 1.7 MB database of 11,499 entities:
//!
//! ```text
//! free      dutime serve / scan / scans / doctor
//! 0         SQLITE_IOERR_SHMSIZE (4874) — cannot even open to read
//! 64 KiB    commits
//! 256 KiB   commits
//! ```
//!
//! So a process that *starts* during the incident gets nothing at all: not a
//! stale answer, an exit code. Reboot the box while the disk is full and the
//! one tool that could say what filled it will not run. A daemon that was
//! already up is fine — its `-shm` is mapped and WAL reads allocate nothing —
//! but that is luck, not a design.
//!
//! The fix is the one storage engines have used for years: keep a file of
//! useless bytes on the same filesystem, and delete it when the disk fills.
//! Freeing it buys back the few hundred kilobytes SQLite needs to commit the
//! scan that explains the fill, to open the database at all, and to let the
//! service start.
//!
//! It is deliberately *not* sized to keep duTime running indefinitely on a
//! full disk. It is sized to survive one incident: spend it, commit, and say
//! so loudly.

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Eight mebibytes: two orders of magnitude more than the largest commit
/// measured above, and small enough that reserving it is never itself the
/// reason a disk filled.
pub const DEFAULT_BALLAST_BYTES: u64 = 8 << 20;

/// Where the reserve for `db` lives.
///
/// Beside the database, because it has to be on the *same filesystem* to be
/// worth anything, and that is the only placement which guarantees it without
/// asking anyone to configure a path. `.ballast` rather than `-ballast` so it
/// cannot be mistaken for one of SQLite's own `-wal`/`-shm` sidecars.
pub fn path_for(db: &Path) -> PathBuf {
    let mut p = db.to_path_buf().into_os_string();
    p.push(".ballast");
    PathBuf::from(p)
}

pub struct Ballast {
    path: PathBuf,
    bytes: u64,
}

impl Ballast {
    pub fn beside(db: &Path, bytes: u64) -> Self {
        Self { path: path_for(db), bytes }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Is the reserve currently on disk, at the size it was asked for?
    ///
    /// A short file is reported as not held: a half-written ballast is a
    /// half-sized rescue, and an operator reading the diagnostics page needs
    /// to know the difference.
    pub fn held(&self) -> bool {
        std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= self.bytes && self.bytes > 0)
    }

    /// Create the reserve, unless it is already there or the disk has no room
    /// to spare.
    ///
    /// Declining on a nearly-full disk is the point rather than a limitation:
    /// taking the last 8 MiB from a filesystem that has 9 MiB left would be
    /// this module causing the incident it exists to survive. The reserve
    /// re-arms on its own once free space recovers, from the next successful
    /// commit.
    pub fn ensure(&self) -> Result<()> {
        if self.bytes == 0 || self.held() {
            return Ok(());
        }
        // Require room for the reserve *twice over*, so creating it always
        // leaves at least as much free as it takes.
        if let Some((_, _, avail)) = super::commit::statvfs(parent_of(&self.path))
            && (avail as u64) < self.bytes.saturating_mul(2)
        {
            tracing::debug!(
                avail,
                want = self.bytes,
                "not reserving disk space yet; too little free to spare"
            );
            return Ok(());
        }
        self.write()
            .with_context(|| format!("reserving {} bytes at {}", self.bytes, self.path.display()))
    }

    fn write(&self) -> Result<()> {
        let tmp = self.path.with_extension("ballast.new");
        let f = OpenOptions::new().create(true).truncate(true).write(true).open(&tmp)?;
        // `fallocate` commits the blocks without writing them, which matters
        // on a spinning disk and matters more on a slow SD card — but ZFS and
        // some network filesystems return EOPNOTSUPP, and a ballast that is a
        // hole in a sparse file reserves exactly nothing. Fall back to real
        // bytes rather than to a file that lies about its size.
        let allocated = rustix::fs::fallocate(&f, rustix::fs::FallocateFlags::empty(), 0, self.bytes)
            .is_ok();
        if !allocated {
            let zeros = vec![0u8; 1 << 20];
            let mut f = &f;
            let mut left = self.bytes;
            while left > 0 {
                let n = left.min(zeros.len() as u64) as usize;
                f.write_all(&zeros[..n])?;
                left -= n as u64;
            }
            f.flush()?;
        }
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &self.path)?;
        tracing::debug!(path = %self.path.display(), bytes = self.bytes, "disk reserve in place");
        Ok(())
    }

    /// Spend the reserve. Returns whether there was one to spend.
    pub fn release(&self) -> bool {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {
                tracing::warn!(
                    path = %self.path.display(),
                    "the filesystem holding the database is full; released duTime's \
                     reserved space so this write could go through. Free space on that \
                     filesystem — duTime will re-reserve it and resume on its own."
                );
                true
            }
            Err(_) => false,
        }
    }
}

fn parent_of(p: &Path) -> &Path {
    match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    }
}

/// Did this fail because the disk is full?
///
/// Deliberately broader than `SQLITE_FULL`. The failure this module was
/// written for reports `SQLITE_IOERR_SHMSIZE`, not `SQLITE_FULL`, because it
/// happens while sizing the WAL index rather than while writing a page — so
/// matching only the obvious code would miss the only case that cannot be
/// recovered any other way. The cost of the wider net is that a genuine I/O
/// error also spends the reserve: one junk file is deleted and one operation
/// is retried, which is a fair price for not being locked out of the database
/// during the incident the tool exists to explain.
pub fn is_disk_full(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        if let Some(rusqlite::Error::SqliteFailure(f, _)) = c.downcast_ref::<rusqlite::Error>() {
            return matches!(
                f.code,
                rusqlite::ErrorCode::DiskFull | rusqlite::ErrorCode::SystemIoFailure
            );
        }
        if let Some(io) = c.downcast_ref::<std::io::Error>() {
            return io.kind() == std::io::ErrorKind::StorageFull;
        }
        false
    })
}

/// Run `op`; if the disk is full, spend the reserve beside `db` and try once.
///
/// One retry, never a loop: if the write still fails with the reserve gone,
/// the disk is full in a way 8 MiB was never going to fix, and the caller
/// needs the error rather than another attempt.
pub fn with_rescue<T>(db: &Path, what: &str, mut op: impl FnMut() -> Result<T>) -> Result<T> {
    match op() {
        Ok(v) => Ok(v),
        Err(e) if is_disk_full(&e) => {
            if !Ballast::beside(db, 0).release() {
                return Err(e);
            }
            tracing::warn!(what, "retrying after releasing reserved disk space");
            op()
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn full_err() -> anyhow::Error {
        anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(13), // SQLITE_FULL
            Some("database or disk is full".into()),
        ))
    }

    /// The error that actually locked duTime out of its own database on a
    /// full disk: raised while sizing the WAL index, not while writing a
    /// page, so it arrives as an I/O error rather than as SQLITE_FULL.
    fn shmsize_err() -> anyhow::Error {
        anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(4874), // SQLITE_IOERR_SHMSIZE
            Some("disk I/O error".into()),
        ))
    }

    #[test]
    fn a_reserve_is_real_blocks_and_can_be_spent_once() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("dutime.db");
        let b = Ballast::beside(&db, 1 << 20);

        assert!(!b.held());
        b.ensure().unwrap();
        assert!(b.held());
        let m = std::fs::metadata(b.path()).unwrap();
        assert_eq!(m.len(), 1 << 20);
        // Sparse would reserve nothing, which is the one way this feature can
        // fail silently: a file that claims a megabyte and holds no blocks.
        assert!(
            std::os::unix::fs::MetadataExt::blocks(&m) * 512 >= (1 << 20),
            "the reserve must occupy real blocks, not be a hole"
        );

        // Already held: ensure is idempotent, not a rewrite.
        b.ensure().unwrap();
        assert!(b.held());

        assert!(b.release());
        assert!(!b.held());
        assert!(!b.release(), "spending a reserve twice must report the second as a no-op");
    }

    #[test]
    fn a_reserve_of_zero_is_no_reserve() {
        let d = tempfile::tempdir().unwrap();
        let b = Ballast::beside(&d.path().join("dutime.db"), 0);
        b.ensure().unwrap();
        assert!(!b.held());
        assert!(!b.path().exists());
    }

    /// A half-written reserve is a half-sized rescue; report it as absent so
    /// the diagnostics page cannot claim protection that is not there.
    #[test]
    fn a_short_reserve_does_not_count_as_held() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("dutime.db");
        let b = Ballast::beside(&db, 1 << 20);
        std::fs::write(b.path(), b"too small").unwrap();
        assert!(!b.held());
    }

    #[test]
    fn disk_full_is_recognised_however_sqlite_phrases_it() {
        assert!(is_disk_full(&full_err()));
        assert!(is_disk_full(&shmsize_err()));
        assert!(is_disk_full(&anyhow::Error::new(std::io::Error::from_raw_os_error(28))));
        // Context wrapping must not hide it: every caller adds some.
        assert!(is_disk_full(&full_err().context("committing a scan")));

        assert!(!is_disk_full(&anyhow::anyhow!("no such table: scan")));
        assert!(!is_disk_full(&anyhow::Error::new(rusqlite::Error::QueryReturnedNoRows)));
    }

    #[test]
    fn a_full_disk_spends_the_reserve_and_the_operation_is_retried() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("dutime.db");
        let b = Ballast::beside(&db, 1 << 20);
        b.ensure().unwrap();

        let tries = Cell::new(0);
        let got = with_rescue(&db, "test", || {
            tries.set(tries.get() + 1);
            if tries.get() == 1 { Err(shmsize_err()) } else { Ok(7) }
        })
        .unwrap();

        assert_eq!(got, 7);
        assert_eq!(tries.get(), 2, "the operation must be retried exactly once");
        assert!(!b.held(), "the reserve must have been spent");
    }

    /// One retry, never a loop: with the reserve gone, another attempt just
    /// delays the error the caller needs.
    #[test]
    fn it_gives_up_rather_than_retrying_forever() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("dutime.db");
        Ballast::beside(&db, 1 << 20).ensure().unwrap();

        let tries = Cell::new(0);
        let e = with_rescue::<()>(&db, "test", || {
            tries.set(tries.get() + 1);
            Err(full_err())
        })
        .unwrap_err();

        assert_eq!(tries.get(), 2);
        assert!(is_disk_full(&e), "the original error must reach the caller");
    }

    /// With no reserve to spend there is nothing to retry with, so the error
    /// goes straight back rather than the work being done twice.
    #[test]
    fn with_no_reserve_the_error_is_returned_unretried() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("dutime.db");
        let tries = Cell::new(0);
        let _ = with_rescue::<()>(&db, "test", || {
            tries.set(tries.get() + 1);
            Err(full_err())
        });
        assert_eq!(tries.get(), 1);
    }

    /// An error that is not about space must not cost the reserve.
    #[test]
    fn an_ordinary_error_leaves_the_reserve_alone() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("dutime.db");
        let b = Ballast::beside(&db, 1 << 20);
        b.ensure().unwrap();

        let tries = Cell::new(0);
        let _ = with_rescue::<()>(&db, "test", || {
            tries.set(tries.get() + 1);
            Err(anyhow::anyhow!("no such column: nope"))
        });
        assert_eq!(tries.get(), 1);
        assert!(b.held());
    }
}
