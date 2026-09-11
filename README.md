# duTime

**Track disk usage over time.**

`du` tells you what is big *now*. When a disk fills up, that is the wrong
question. A 40 GB directory that has been 40 GB for a year is not why the disk
filled last Tuesday. duTime answers **what grew, and when** — and, just as
usefully, what disappeared.

It runs as a small service on a Linux host, snapshots the directory tree on a
schedule, and serves a web UI you can reach from a headless server.

![The diff treemap](docs/diff-treemap.png)

*Two moments compared. Area is the larger of the two sizes, colour is the
change. The 700 MB download that was deleted is still visible, in blue, at the
size it used to be — a point-in-time `du` today cannot tell you it ever
existed.*

## Why not an existing tool?

| Tool | Gap |
|---|---|
| `du`, `ncdu`, `gdu`, `dust`, `baobab`, QDirStat | Point-in-time only |
| `duc` | Has a database and a web UI, but keeps only the latest scan per path |
| `agedu` | Tracks file *age*, not *growth* — a different question |
| diskover | Elasticsearch + PHP; heavyweight and enterprise-oriented |
| Prometheus dirsize exporters + Grafana | A time-series database has no notion of hierarchy, so subtree aggregation — the whole point — is impossible, and 100k directories as label values is a cardinality explosion |
| TreeSize (Windows, paid) | Can compare two snapshots, but is not a service and not Linux |

## Getting started

```console
$ cargo build --release
$ ./target/release/dutime serve --root "$HOME"
duTime listening on http://127.0.0.1:8471
  tracking /home/steven every 1h
```

Then open <http://127.0.0.1:8471>. On a headless box, forward it:
`ssh -L 8471:localhost:8471 yourserver`.

To install it properly:

```console
$ dutime install --user          # no root, no capabilities, tracks $HOME
$ dutime install --system        # dedicated user + CAP_DAC_READ_SEARCH
```

`install` writes a systemd unit and a starter config, then prints the two
`systemctl` commands — it never enables anything behind your back. The system
unit runs as an unprivileged `dutime` user holding exactly one capability,
never as root, and scores 1.7 on `systemd-analyze security`.

## The command line

The CLI is not an afterthought to the web UI — it is what you script against,
and it answers the question on its own.

```console
$ dutime top --since 7d
what grew in the last 7d (exclusive mode)

  +308.0 MiB  /srv/app/target/release/artifact.bin
   +89.4 MiB  /var/cache/pkg
    +5.0 MiB  /home/alice/Pictures/day13.jpg

$ dutime top --since 7d --losers
what shrank in the last 7d (exclusive mode)

  -700.0 MiB  /home/alice/Downloads/distro.iso

$ dutime du /srv --at -3d
1.1 GiB	/srv

$ dutime doctor
sqlite integrity_check       ok
current_size consistency     ok (/home/steven)
reconstruction               ok (recorded 485003831571, fast 485003831571, replay 485003831571)
in-memory snapshot           ok (127501 entities, loaded in 150 ms)
no problems found
```

`top` has two modes. **Exclusive** names the directory whose *own* files grew,
which points straight at the culprit. **Inclusive** rolls growth up the
ancestor chain, and by default hides any directory whose growth is entirely
explained by one child — otherwise a single new file reports itself nine times,
once for every directory above it.

## The web UI

**Overview** — capacity, the tracked tree over time, biggest gainers, and a
days-to-full projection that refuses to guess until it has a real window to
extrapolate from.

**Explorer** — a WinDirStat-style treemap with a time slider; drag it and the
same tree redraws as it stood at that moment. Below it, a sortable listing of
everything in the current directory with a **trend sparkline beside each row**,
so you can see which of thirty siblings is the one creeping up before deciding
which to open. Then the same directory decomposed into its largest children as
a stacked area.

![Explorer](docs/explorer.png)

**Changes** — biggest gainers and losers over any window, exclusive or
inclusive, exportable as CSV.

**Compare** — the diff treemap at the top of this page.

Every view is linkable: the URL carries the path, window and comparison, so you
can paste exactly what you are looking at into a ticket.

## How it works

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
all. See [docs/storage.md](docs/storage.md) for the full comparison.

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

## Configuration

```console
$ dutime config > /etc/dutime/config.toml
```

Scanning defaults to **hourly**, not five-minutely. Every timing here is from a
warm cache; the first walk after a reboot has to fault in a gigabyte of
dentries and inodes and will be far slower. Measure a cold scan on the machine
in question before turning the interval down.

Exclusions use gitignore syntax, but duTime deliberately never reads
`.gitignore` files off disk — `target/` and `node_modules/` are precisely what
you are trying to find.

## Status

Working: scanner, store, query layer, CLI, daemon, REST API, web UI.

Not yet: retention tiering (unnecessary at current volumes — see
[docs/storage.md](docs/storage.md)), move detection, alerting, and fleet
aggregation.

## License

MIT
