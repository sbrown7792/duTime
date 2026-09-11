# Why SQLite

The obvious worry with "snapshot the filesystem every five minutes forever" is
that the database eats the disk it is supposed to be protecting. That worry is
what drove the storage design, so here are the measurements it rests on.

All figures are from a live Ubuntu 24.04 workstation: 824 GB root filesystem,
448 GiB tracked in `$HOME`, 884,071 files across 103,463 directories.

## The numbers

| | |
|---|---|
| Tracked entities (dirs + files ≥ 1 MiB) | 127,496 |
| Baseline snapshot | 20 MB |
| Events per subsequent scan | **2–10** |
| Full walk, warm cache, 4 threads | 2.1 s |
| Commit | 0.19 s |

Over a fortnight of hourly samples against a synthetic tree, 337 scans of ~99
entities produced **573 event rows** — 1.7% of the 33,264 a
snapshot-everything design would have written.

## Why it is that small

**Change-only storage.** A row means "this entity's size became X at this
scan". No row means unchanged. Most of a filesystem does not move between two
scans five minutes apart, so most of it costs nothing.

**Exclusive sizes, inclusive by rollup.** Mean directory depth here is 9.26, so
recording inclusive sizes would dirty ~9.3 rows for a single file write and
rewrite the root's row on every scan. Measured: 55 exclusive events per scan
versus ~510 inclusive. Recovering inclusive values costs `churn × depth`
additions in memory — microseconds.

**A file-size threshold.** 24,158 of 884,071 files (2.7%) are at least 1 MiB,
and they hold ~95% of all bytes. Tracking only those cuts file entities 36×
while losing almost nothing you would act on.

**Mass deletes collapse.** Removing a `node_modules` with 30k directories
writes one event carrying the whole subtree's loss, not 30,000 tombstones.

Write amplification matters as much as row count. The naive design is ~5 MB of
rows per scan, about **1.4 GB of WAL per day** at five-minute intervals.
Change-only storage is under 100 KB of WAL per scan, about 28 MB/day.

## What was rejected, and why

| Engine | Verdict |
|---|---|
| **SQLite** | **Chosen.** Zero operations, one file, in-process. Every hot query is a point lookup by `path_id` or a bounded time-range scan — never a full-table aggregation — so a row store is the right shape. `rusqlite`'s bundled build means no system SQLite is required. WAL keeps the UI readable during a commit. It is also the only candidate that cannot be "down". |
| **DuckDB** | Genuinely tempting: `ASOF JOIN` is *literally* the "value as of time T" operator, and columnar compression would shrink the event log ~5×. Rejected as the primary store for coarse single-writer locking, weak handling of frequent small commits, a large C++ build, and a less battle-tested crash story. Kept as an option: `ATTACH 'dutime.db' AS d (TYPE SQLITE)` gives DuckDB analytics over our file with zero coupling. |
| **PostgreSQL / TimescaleDB** | A server process, violating the single-binary goal, to manage a few hundred MB a year. Hypertables would be lovely at 1000× this volume. Revisit only as a fleet aggregator. |
| **Prometheus / VictoriaMetrics** | An anti-pattern here. 127k directories as label values is a textbook cardinality explosion, path churn creates unbounded series, and — decisively — a time-series database has no concept of hierarchy, so **subtree aggregation is impossible**. That is the one operation duTime exists to perform. duTime *exports* a small curated set of metrics instead; that is a feature, not the store. |
| **ClickHouse** | Excellent at this data shape and would compress the event log beautifully. It is also a multi-GB server process, for a workload measured in kilobytes per day. Reserve for a fleet of 100+ hosts. |

The store sits behind a narrow interface. If some machine turns out to churn a
thousand times harder than this one, swapping the backend is a contained change
— but building a two-backend abstraction before anyone needs one is not.

## How history is reconstructed

Two paths, deliberately:

* **`incl_at`** is production: find the most recent inclusive checkpoint at or
  before the target scan, then apply the deltas since it. Cost tracks recent
  churn, not total history.
* **`incl_at_naive`** replays last-value-per-path over everything. Far too slow
  to ship, but obviously correct — so it is the oracle the fast path is tested
  against, and `dutime doctor` runs both against the value the scanner recorded
  at the time. On the real 448 GiB tree all three agree exactly.

Checkpoints are written on the first scan unconditionally, so the slow path
never runs in production.

`current_size` materializes "the latest event per path". Without it, every
commit would have to scan all of history to learn the previous values — after a
year, millions of rows read to answer a question about 127k paths. It is
written in the same transaction as the events it summarizes, and `doctor`
re-derives it from scratch to prove it has not drifted.

## Retention

duTime currently keeps everything, because at ~231 MB/year of full-resolution
history there is nothing to reclaim. Tiering is a query-speed feature, not a
survival one, and building it before it is needed would be effort spent on a
problem that does not exist yet.

When it is needed, the delta encoding makes downsampling exact rather than
approximate: a bucket's absolute size is the last value in it, and a bucket's
delta is the *sum* of the deltas in it. Only intra-bucket spikes are lost.

The subtler half is dropping *entities* rather than timestamps as data ages.
Naively deleting a small child silently understates every ancestor forever. The
fix is to fold, not drop: roll the pruned entity's values into a synthetic
`⟨pruned⟩` sibling so inclusive totals stay exact and the loss of detail is
visible in the UI rather than being a quiet lie.
