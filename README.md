# duTime

**Track disk usage over time.**

`du` tells you what is big *now*. When a disk fills up, that is the wrong
question. A 40 GB directory that has been 40 GB for a year is not why the disk
filled last Tuesday. duTime answers **what grew, and when.**

It runs as a small service on a Linux host, snapshots the directory tree on a
schedule, and serves a web UI you can reach from a headless server.

## Status

Early development. The scanner and its correctness gate are in place.

- [x] Parallel filesystem walker, byte-exact against `du`
- [x] Mount-aware one-filesystem restriction (incl. same-device bind mounts)
- [x] Deterministic hardlink de-duplication
- [x] Complete SQLite schema
- [ ] Change-only event store
- [ ] Query layer: as-of-T sizes, biggest gainers
- [ ] CLI
- [ ] Daemon + REST API
- [ ] Web UI: treemap, time slider, diff treemap

## Why not an existing tool?

| Tool | Gap |
|---|---|
| `du`, `ncdu`, `gdu`, `dust`, `baobab`, QDirStat | Point-in-time only |
| `duc` | Has a database and a web UI, but keeps only the latest scan per path |
| `agedu` | Tracks file *age*, not *growth* — a different question |
| diskover | Elasticsearch + PHP; heavyweight and enterprise-oriented |
| Prometheus dirsize exporters + Grafana | A TSDB has no notion of hierarchy, so subtree aggregation — the whole point — is impossible, and 100k directories as label values is a cardinality explosion |

## Design notes

**Exclusive storage, inclusive by rollup.** A directory's inclusive size changes
whenever anything beneath it changes. At a mean depth of ~9.3, storing inclusive
sizes dirties ~9.3 rows per single file write. duTime stores *exclusive* ("own")
sizes — one row — and recovers inclusive values with an in-memory rollup.

**Change-only events.** The absence of a row means "unchanged". Measured churn
on a normal workstation is ~55 directories per 5 minutes, so a full-resolution
year of history is a few hundred MB rather than the ~1.4 GB *per day* a naive
snapshot-everything design would write.

**SQLite, deliberately.** At this volume the analytical engines solve a problem
that does not exist while charging real operational cost. See
`docs/storage.md`.

**Both size metrics, always.** Apparent (`st_size`) and allocated
(`st_blocks * 512`) are tracked separately everywhere. Their divergence is
signal: sparse VM images have far fewer blocks than bytes, and a pile of tiny
files has more blocks than bytes from per-file tail slack.

## Correctness

duTime's numbers are only worth anything if you can check them, so the test
suite pins them to `du` byte for byte — including hardlinks, sparse files,
symlinks, non-UTF-8 filenames, and files straddling the tracking threshold.

```
cargo test
```

Verified against a real 11 GB `/usr` tree: apparent and allocated totals both
match `du` exactly.

## Building

```
cargo build --release
./target/release/dutime scan /usr
```

## License

MIT
