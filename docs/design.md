# How duTime works

The short version is in the [README](../README.md#how-it-works). This is the
reasoning behind it: why the store is shaped the way it is, and how the
numbers are checked against `du`.

## The design

A scheduled walk, a change-only event log, and an in-memory rollup.

**Change-only storage.** A row means "this entity's size became X at this
scan"; no row means unchanged. On a live 448 GiB home directory with 884k
files, the baseline snapshot is 127,496 entities in a 20 MB database and every
scan after it records **2–10 events**.

**Exclusive sizes, inclusive by rollup.** Mean directory depth is ~9.3, so
storing inclusive sizes would dirty ~9.3 rows per single file write. Storing
exclusive sizes dirties one, and the rollup that recovers inclusive values is
an in-memory pointer walk.

**SQLite, deliberately.** At a few hundred MB a year, the columnar and
server-based options solve a problem that does not exist while charging real
operational cost — and a time-series database cannot do subtree aggregation at
all. See [storage.md](storage.md) for the full comparison.

**Both size metrics, always.** Apparent (`st_size`) and allocated
(`st_blocks × 512`) are tracked separately everywhere. Their divergence is
signal: sparse VM images have far fewer blocks than bytes, and a pile of tiny
files has more blocks than bytes from per-file tail slack.

## Correctness

duTime's numbers are only worth anything if you can check them, so the test
suite pins them to `du` byte for byte — including hardlinks, sparse files,
symlinks, non-UTF-8 filenames, and files straddling the tracking threshold.

Verified against a real 11 GB `/usr`: apparent and allocated totals both match
`du` exactly. `dutime doctor` cross-checks three independent implementations of
history reconstruction — the fast checkpoint+delta path, a naive full replay,
and the in-memory rollup — against the value recorded at scan time.

```console
$ cargo test
```

Two things `du` does that are surprising, and that duTime replicates because
being checkable matters more than being tidy:

- `du --apparent-size` does **not** count a directory's own `st_size`, while
  allocated `du` **does** count its `st_blocks`.
- `du -a` omits de-duplicated hardlinks from its listing entirely.

Where duTime deviates, it does so on purpose: hardlink de-duplication credits
the lowest-sorting path rather than whichever link the walk reached first, so
two scans of an unchanged tree agree instead of inventing growth.

## Surviving the disk it is watching

duTime exists to answer "what filled this disk", which means the disk being
full is its working condition, not an edge case. SQLite does not take that
view. On a filesystem with zero bytes free it cannot open a database at all,
because WAL mode has to create and size a 32 KiB `-shm` index before it can
read a row. Measured against a 1.7 MB database of 11,499 entities, on a
filesystem filled to exactly zero:

| free | `dutime serve` / `scan` / `scans` / `doctor` |
|---|---|
| 0 | `SQLITE_IOERR_SHMSIZE` — cannot open, even read-only |
| 64 KiB | commits |
| 256 KiB | commits |

The split that matters is between a process that is already running and one
that is not. A live daemon is unaffected: its `-shm` is mapped, WAL reads
allocate nothing, and every API endpoint keeps answering with the disk at zero.
Its commits fail with `SQLITE_FULL`, the scan is discarded rather than recorded
as a cliff, and it recovers on its own the moment space returns. A process that
*starts* during the incident gets an exit code.

That asymmetry is luck, not design, so duTime does not rely on it. It reserves
8 MiB in `dutime.db.ballast` beside the database — the same filesystem, by
construction — and frees it on the first operation that fails for want of
space, whether that is a commit or an open. One retry, never a loop: if the
write fails again the disk is full in a way 8 MiB was never going to fix, and
the caller needs the error rather than another attempt.

Three details are load-bearing:

- **The reserve must hold real blocks.** `fallocate`, falling back to writing
  zeros where the filesystem does not support it. A sparse file of the right
  length reserves nothing, and would fail silently in exactly the situation it
  was created for.
- **"Disk full" is matched more broadly than `SQLITE_FULL`.** The failure this
  was built for reports `SQLITE_IOERR_SHMSIZE`, because it happens while sizing
  the index rather than while writing a page. Matching only the obvious code
  would miss the only case that cannot be recovered any other way.
- **Re-reserving is refused while the disk is still nearly full.** Taking the
  last 8 MiB back from a filesystem with 9 MiB free would be this mechanism
  causing the incident it exists to survive. It re-arms once there is room for
  the reserve twice over.

The history itself is rarely the casualty. A disk fills *across* the preceding
scans, every one of which committed while there was still space, so what
explains the fill is already recorded before anything fails. What the reserve
protects is the ability to go and read it.
