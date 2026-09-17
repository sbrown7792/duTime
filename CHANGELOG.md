# Changelog

Versions are bumped per batch of user-visible change, not per commit. While
duTime is pre-1.0 the minor number moves for features and notable fixes, the
patch number for fixes and cosmetic additions — minting a minor release for a
link in a footer would make the numbers mean less, not more.

Between releases, `dutime --version` identifies a build exactly: it carries
the commit and commit date, and `dutime doctor` adds the binary's own mtime.
That is what answers "is the thing running on that server the thing I just
built?", which a semver cannot, since it is identical across every build
between releases.

## 0.5.1 — 2026-09-16

- **A name deleted and recreated is one row, not one row per generation.**
  0.5.0 started listing deleted entries, and a SQLite write-ahead log
  checkpointed away daily promptly produced *nineteen* rows for one filename
  in a thirty-day window — burying the directory it was in. The store still
  records each generation separately, which is right: a location that is
  emptied and refilled is not the same bytes. But the listing is about
  locations, which is duTime's premise, so the generations are added together
  there and the gaps appear as the zero-byte periods they were. The row says
  how many times the name has been recreated.

  Merged in the query rather than by reviving the dictionary row, which was
  the obvious alternative and is wrong: `incl_files` counts file *nodes*
  regardless of size, so a revived row would have inflated historical file
  counts for every scan in the gap.

- **A favicon**, drawn as the treemap the app is built around, with the gaps
  sized so the four blocks stay separate at 16px.

## 0.5.0 — 2026-09-16

### Added

- **Entries deleted inside the window are listed**, struck through, with the
  trend that explains them.

  This is the case that sends people looking: a directory's trend spikes and
  returns to baseline, and nothing inside it shows the same shape — because
  whatever caused it is no longer there to be listed. Found on a real server
  as a 230 MiB SQLite write-ahead log that was checkpointed away between two
  scans; the recreated file was listed, flat and 1.9 MiB, while the 230 MiB
  one it replaced was invisible.

  A deleted row shows no size, because it has none, and says the peak it
  reached instead — for something created *and* deleted inside the window that
  is the only number describing how much space it was taking. Its trend ends
  at zero. It is not clickable, there being nothing to descend into.

### Changed

- Entries are ranked for a listing slot by their **peak-to-trough movement**
  rather than their net change. Something created and deleted inside the
  window nets to zero however large it got, so ranking on net change buried
  exactly the row that explained the parent's spike.

### Fixed

- **Permalinks naming a directory opened at the root instead.** On the first
  load there is no previously selected root to compare against, and the
  comparison that clears the path when you switch roots was firing anyway —
  discarding the path that had just been read out of the URL.

## 0.4.2 — 2026-09-15

- **The Overview's window control now moves the chart.** It only ever moved
  the gainers table; the graph above was built from the entire recorded
  history regardless, so picking "Last hour" left two cards on one page
  disagreeing about what the window meant.
- The projection deliberately does *not* follow it. It is fitted to every
  sample there is, because days-to-full is a property of the disk and should
  not swing with the control you are using to look at it.
- The tracked-size tile is now stated by the server rather than read off the
  end of the chart's series, which a narrow window can empty. The chart's
  caption counts what is plotted, and says how many samples exist in total
  when those differ.

## 0.4.1 — 2026-09-13

- **An exclude pattern starting with `#` now excludes something.** Gitignore
  syntax reads a leading `#` as a comment, so `exclude = ["#recycle/"]` parsed
  as a comment and quietly excluded nothing — no error, and a scan that looked
  fine while still walking the directory it had been told to skip. Synology
  puts a `#recycle` at the top of every shared folder, so this is the first
  thing a NAS owner tries. In a list where every entry *is* a pattern there is
  no such thing as a comment. `!` still negates.
- `/api/v1/scans` reports each scan's `status` and `err`. Both columns were
  being read from the database and then discarded, which made the one endpoint
  you would check for a failed scan the one that could not tell you about it.

## 0.4.0 — 2026-09-13

### Fixed

- **The Contents listing hid the directory you were looking for.** It can only
  show so many entries, and it chose them by size. In a directory of
  thousands that hid exactly the wrong thing: a small folder that doubled sat
  below every large folder that did nothing, fell outside the limit, and was
  reported only as part of a count of entries "not shown".

  Entries are now chosen by what moved — anything with a change over the
  window is kept first, largest change first, and the remaining slots go to
  the largest static entries. Size still decides the order among things that
  did not move, and the browser still sorts the displayed table however you
  ask; this only decides who makes the cut.

  Doing that required measuring every child before dropping any, so the window
  deltas are computed for the whole directory and the selection happens
  afterwards. Measured no slower on a 1.3M-entity volume.

- The "not shown" note now says whether the hidden entries changed. Normally
  none did — the movers are taken first — and "1,204 unchanged entries not
  shown" is a much more useful thing to be told than "1,204 smaller entries".

## 0.3.1 — 2026-09-12

- The status line links to the repository. The URL comes from `Cargo.toml`, so
  it is not a copy pasted into the page, and the link carries `rel="noreferrer"`
  — duTime is usually served from an internal host, and the default would hand
  that hostname to github.com on every click.
- **The Explorer is reordered**: Contents, then Composition over time, then
  Blocks. The breadcrumbs stay at the top, since they steer every pane; the
  time slider moved down with the treemap, because it only ever applied to
  that one.
- Dragging the slider no longer re-fetches the Contents listing. The slider
  does not appear in that request's parameters, so it was a request per drag
  that could not change anything on screen.
- A relative window given alongside an explicit `at` is now measured from that
  moment rather than from the wall clock. Only reachable through the API —
  `?at=scan:5&from=-24h` previously meant the day before *now*, which could end
  before it began.

## 0.3.0 — 2026-09-12

### Added

- **A status line** at the foot of the page: which build is answering, when
  that binary was built, and how much disk the database is using. The build
  stamp is there because duTime is deployed by copying a binary to a server,
  and a version number alone cannot tell you whether the copy landed.
- **Per-pane progress.** Each card shows its own bar and dims its own body,
  because the panes finish at different times and one page-wide bar that
  clears when the last lands says nothing about which is still working.
  Descending into a directory previously showed no feedback at all.

### Fixed

- **A window containing a baseline scan took 12 seconds.** The first full
  scan of a volume emits one event per entity — 1,463,512 on a real Nextcloud
  volume — and every window containing it had to resolve each one by climbing
  its ancestors with a database lookup per level. Bands are now assigned by
  walking down the subtree once, so each event is an array index: **12 s →
  0.62 s**, and 12 ms once the baseline ages out of the window.
- The per-pane progress bars pushed the document sideways and flashed a
  horizontal scrollbar. Fixing that turned up three more sources of the same
  thing at phone widths — the time slider, the tab row, and two tables — all
  of which predated the bars.

### Changed

- **Schema version 2**: `size_event_by_scan` now covers the delta columns, so
  reading a window is one sequential index scan instead of a seek per row.
  1.34 s → 0.88 s on 1.3M events, for 4% more disk. Existing databases are
  migrated on first start, which takes a few seconds on a large one.
- "Trend" in the Contents pane now reads "Trend scale".

## 0.2.0 — 2026-09-11

Everything here came out of running 0.1.0 on a real server.

### Added

- **Explorer directory listing.** Every entry in the current directory with
  its size, its change over the window, and a **trend sparkline** per row, so
  you can see which of thirty siblings is the one creeping up before deciding
  which to open. Sortable; rows descend into directories.
- **Two trend scales.** *Per row* gives each row its own height and shows
  shape. *Shared* sets the height from the largest movement on the page, so
  the biggest mover fills its cell and everything else is drawn to the same
  ruler. Remembered per browser, carried in the permalink.
- **A row to walk back up** (`..`), so navigating out does not mean moving to
  the breadcrumbs. Rows are keyboard-reachable.
- **Per-root access control.** Mark a root `protected = true` and it needs a
  bearer token; unprotected roots stay open, so a shared dashboard can show
  the system disk while a Nextcloud volume stays shut. Anonymous callers are
  not told a protected root exists. Sign in from the UI; `dutime token`
  generates one.
- **`dutime config --check`**, which prints what a config resolves to rather
  than only whether it parses, and warns about nested roots.
- **`dutime doctor` network section**: bound address, systemd's IP filter, a
  reachability probe, and every URL the host answers on.
- **`dutime token`**, and `dutime install --listen`, which writes the config
  and the unit's IP filter together.
- **Access logging** (`access_log = true`), a loading indicator on root
  changes, and `/api/v1/cache` reporting snapshot cache occupancy.

### Fixed

- **Default excludes silently dropped 71,516 files / 18.9 GiB.** `/snap/` as
  a gitignore pattern anchors to the scan root, so it matched `$HOME/snap`.
  Absolute excludes are now separate from relative ones.
- **An unreadable directory was reported as a smaller number.** 11 MB behind
  a `chmod 000` directory came back as 1.9 MiB with the scan marked `ok` —
  indistinguishable from a real shrink, which is the one confusion duTime
  exists to prevent. Such scans are `partial`, name the paths, and show a
  banner.
- **A filesystem mounted under a root was skipped and recorded nowhere**, so
  duTime could report "no mount was skipped" while a whole volume sat
  unscanned beneath it.
- **NFS advice was wrong.** `CAP_DAC_READ_SEARCH` does nothing on a network
  share, where the server checks the uid. duTime now names the filesystem
  type and both uids.
- **The service bound `0.0.0.0` and silently dropped every packet**, because
  the shipped unit carries `IPAddressAllow=localhost`.
- The UI froze during a scan commit, and the Contents column ran off the card
  on directories with millions of files.

### Performance

Measured on a synthetic 1.3M-entity volume
(`cargo run --release --example bench_large`):

| | 0.1.0 | 0.2.0 |
|---|---|---|
| Contents, ordinary window | 2.3 s | 0.012 s |
| Contents, window containing a baseline scan | 12 s | 0.62 s |
| Stacked area, ditto | 7.5 s | 0.64 s |
| Worst-case API latency during a scan | 680 ms | 164 ms |

The snapshot cache is bounded in bytes rather than in snapshots — eight
snapshots of a large root is 1.8 GB against a unit that capped the service at
1 GB. Resident memory on that fixture went from 1.1 GB to ~470 MB, and the
unit now allows 2 GB.

### Changed

- `MemoryHigh`/`MemoryMax` raised to 1G/2G. Memory scales with entity count,
  and being OOM-killed mid-scan is worse than using the memory.
- The up row's tooltip no longer shows the parent's change; deriving it meant
  reading the whole subtree for a tooltip.

## 0.1.0 — 2026-09-10

Initial build; never tagged. Scanner with `du`-exact accounting, the
change-only event store, the query layer, the CLI, the daemon, the REST API
and the web UI.
