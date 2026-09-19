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
