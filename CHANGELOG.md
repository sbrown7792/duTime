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

## 0.5.10 — 2026-09-18

- **Releases now ship a binary.** Tagging builds a statically linked
  `x86_64-unknown-linux-musl` executable and attaches it to the GitHub
  release, so deploying is a download rather than installing a Rust toolchain
  on the machine whose disk is filling up. The asset name carries no version,
  so `releases/latest/download/…` is a URL that keeps working.

  One target, on purpose. A glibc-linked binary inherits the build host's
  glibc version as a floor and simply fails to exec on anything older —
  Debian 12, an older Ubuntu LTS, most NAS firmware. For a tool whose
  distribution story is "copy it to your server", that is a hard failure
  rather than a slow path, and no second target avoids it.

- **duTime brings its own allocator.** Rust calls whatever `malloc` the
  platform provides, so the parallel walk — a path buffer per directory
  entry, 211k of them on a 10 GiB `/usr`, across four threads — ran at the
  mercy of the libc it happened to be linked against. Median of five
  interleaved runs on that tree:

  | build | scan |
  |---|---|
  | glibc + system allocator | 0.78 s |
  | glibc + mimalloc | 0.68 s |
  | musl + system allocator | 1.65 s |
  | musl + mimalloc | 0.85 s |

  musl's malloc is written for size and simplicity rather than concurrent
  throughput, and that 2.1× was the price of a portable binary. It is not any
  more. glibc gains 13% from the same change, so a build you make yourself is
  faster too.

  The binary is not stripped: symbols cost 458 KB compressed and buy 12,889
  named functions in a panic backtrace, which is the difference between
  diagnosing a crash on someone's NAS and not.

## 0.5.9 — 2026-09-18

- **The Changes tab's controls no longer re-aim the Overview.** Choosing
  "Shrank" there left the Overview's summary listing what shrank, underneath
  a heading reading **Biggest gainers** and a caption promising what grew the
  most — a chart contradicting its own title, which is the one thing a
  diagnostic must never do.

  The gainers table is drawn on both views and was reading the direction out
  of shared state, so a control belonging to one view silently changed the
  other. The same leak applied to the other two controls in that row:
  *Inclusive* attribution and the ancestor-collapsing checkbox both reached
  the Overview's pane, the first of them contradicting a caption that says it
  counts a directory's own files. The caller now states what it wants: the
  Overview asks for the fixed summary it advertises, the Changes tab passes
  its controls, and each keeps its own settings.

- The Overview's eight-row summary no longer overwrites what the Changes
  tab's *Export CSV* hands over.

## 0.5.8 — 2026-09-18

- **Hovering a block names the whole path.** Both treemaps — Compare and the
  Explorer's Blocks — showed only the leaf name, which in a tree of media or
  of per-application config directories is the one thing that cannot identify
  anything: `movie.mkv` and `config` occur hundreds of times, and telling
  which one grew is the only reason to hover in the first place.

  The path is threaded down as the tiles are built rather than reassembled
  from the hovered tile's ancestry, because the displayed name of a deleted
  entry carries a `(deleted)` suffix that reassembly would splice into the
  middle of the path. The synthesised buckets — `⟨other⟩` and the own-files
  row — keep their own label and are given no path, since inventing one would
  name a directory that does not exist. Long paths wrap inside a capped
  tooltip instead of stretching it off the side of the chart.

- **The container tile stops answering `NaN B → NaN B`.** A treemap's
  outermost node is drawn behind the tiles and is hoverable wherever they do
  not reach — 7 of 59 points along one horizontal sweep of a real chart. It
  holds none of a tile's fields, so it produced a tooltip with a blank name
  and NaN for every figure. It now produces nothing.

## 0.5.7 — 2026-09-18

- **The used-space chart's vertical scale is now a choice.** 0.5.6 fitted the
  axis to the range in the window, which answers "what did this window do"
  and is the better default — but it is not the only question, and the other
  one ("how much room is left") wants the opposite axis. A **Scale** control
  offers both: *Changes* fits to the range, *Filesystem* runs zero to the
  size of the disk.

  Under *Filesystem* the gridlines are quarters of the disk rather than
  binary boundaries: the ceiling has to be the real size for height to mean
  fullness, and rounding it up to the next power of two would put the top
  gridline somewhere the disk does not reach. Left to itself the chart
  divided the range and then added the ceiling as a stub, printing "745.1
  GiB" directly beneath "823.9 GiB".

  The caption states which scale is in force, the filled area returns under
  *Filesystem* because that axis really does start at zero, and the choice
  is remembered per browser and travels in the permalink — "the disk is
  filling" and "here is what moved this week" are different claims, and a
  link should arrive making the one that was sent. A root whose filesystem
  size was never recorded falls back to fitting rather than drawing an axis
  to an unknown ceiling.

## 0.5.6 — 2026-09-18

- **"Space used over time" is scaled to what it is showing.** The axis ran
  from zero to a rounded maximum, which for a large and slowly-moving total
  draws a flat line pinned near the top of the frame — saying neither how
  full the disk is nor what the window did.

  A fixed ceiling does not fix it either. Measured across four real roots,
  the window span was 1.2%, 0.2%, 100% and 0.0% of the maximum, and the
  tracked size was 50%, 91%, 31% and 20% of its filesystem — so scaling to
  the filesystem leaves the 91% root just as flat, only higher up, and a
  min-to-max scale is degenerate on the root that never moves at all. The
  axis is now fitted to the range in the window, on binary tick boundaries;
  a series that never moves gets a band invented around it so it sits mid
  height and reads as flat; and a range that reaches zero on its own stays
  zero-based. On those four roots the line went from occupying 0.2–1.2% of
  the chart height to 58–64%.

  The filled area is dropped whenever the axis does not start at zero, since
  a fill measures from the baseline and would overstate every value by
  whatever was cut off the bottom, and the caption says the scale is fitted.
  The stacked composition chart is untouched: stacked areas must start at
  zero.

- Finding the range no longer spreads the whole history into `Math.min`.
  `history` is every scan in the window, uncapped, so a long window of
  frequent scans passed one argument per scan and would overflow the call
  stack.

## 0.5.5 — 2026-09-18

- **The deepest colour means the most movement in both themes.** Dark mode
  re-stepped the diverging ramp so its *lightest* steps sat at the extremes,
  on the reasoning that light carries furthest on a dark ground. It also
  meant the same reading had to be learned twice, and inverted between them.
  The dark ramp now runs the same way as the light one — deepest and most
  saturated at the ends, neutral receding into the page — using hues already
  tuned for a dark background rather than importing near-black navy. The
  glyph inks were recomputed against the new steps; every pair clears 4.5:1.

- **A comparison no longer draws the parts of the tree that did not change.**
  A seven-day diff of a media library returned **41,620 rectangles, of which
  41,486 — 99.7% — had a delta of exactly zero**, in a 7.5 MB response. The
  hundred-odd tiles that had actually moved were lost in it. A subtree with
  no movement anywhere inside it holds no diff information by definition, so
  it is now drawn as a single tile at its own size and says how much detail
  it is standing in for. On that library: 41,620 tiles to 1,232, and 7.5 MB
  to 239 KB.

- **The change outranks the bulk.** Children were kept by size and cut at the
  limit, so a directory whose largest children are all static hid its
  movement behind them: `Movies` had moved 48 GB while its 300 largest
  children had moved nothing between them — the answer was off the end of the
  list, and collapsing then reported it as "nothing moved here", which was
  never checked. Anything that moved now sorts ahead of everything that did
  not.

- **A tile is named only when its whole name fits, and never when it did not
  move.** The old rule was a size threshold, which cannot know how long a
  name is: every tile past 54px was labelled and then cut to whatever fitted,
  down to a single letter — the field of `H`, `T`, `Jo` stubs that made a
  deep treemap look like static. Labels are now placed by measuring the text
  against the tile. The bands above parent tiles are measured too: ECharts
  does not route them through the hook the fit test lives in, so they are
  checked against the drawn layout in a second pass.

  Unchanged tiles are no longer labelled at all. They are there to give the
  moved ones something to be read against, and which of them got a name came
  down to nothing but its length — `FBI` fitted where `The_Blacklist` did
  not. The result reads like WinDirStat with a diff over it: blocks for the
  bulk, names on the parents and on what actually moved.

## 0.5.4 — 2026-09-17

- **A comparison can no longer run backwards.** Nothing stopped "to" being
  earlier than "from", and the result was a picture that is internally
  correct and reads as a lie: every growth shown as a shrink, in the one
  view whose entire content is which way things moved. Nothing in the output
  announced the inversion either.

  The impossible options are now disabled rather than the pair being
  silently swapped, so the control states its constraint instead of
  correcting a choice after it is made: "to" offers only scans after the
  selected "from", and a scan with nothing after it cannot be a "from" at
  all. Moving "from" past "to" carries "to" forward with it, since a
  disabled option stays selected in every browser and would otherwise be
  stranded behind. A shared permalink that arrives inverted is corrected on
  load and the URL rewritten to the pair actually shown.

## 0.5.3 — 2026-09-17

- **The Compare legend shows the half of its scale that was invisible.** The
  legend markup named its three "shrank" chips `d-3 d-2 d-1` while the
  stylesheet painted `.sw.d3 .sw.d2 .sw.d1`, so those three rendered fully
  transparent: bare minus signs floating beside the grey and red chips. The
  legend said "shrank → grew" while showing no colour for shrank at all —
  which is the one thing it exists to say, since blue is otherwise unexplained.

- **Legend glyphs gradate, and stay legible in both themes.** The chips read
  `−− − −` and `+ + ++`, repeating two glyphs and so failing as the
  non-colour encoding they are there to provide; they now step `−−− −− −` and
  `+ ++ +++`. Their ink was picked by a rule that assumed step 3 is the dark
  end of the ramp — true in light, false in dark, where the ramp is re-stepped
  rather than flipped, so the darkest chips are `d1`/`u1`. White on the light
  `#e89a9a` chip measured 2.2:1. Each step now carries its own ink token per
  theme, and every one of the fourteen pairs was measured at 4.5:1 or better;
  two light-mode pairs that had always been below it are fixed in passing.

  A test pins both halves: every swatch class must have a rule that paints it,
  and a step defined in three themes must have an ink defined in three themes.
  The bug was a mistyped class name, which reading cannot catch — both halves
  look correct on their own.

## 0.5.2 — 2026-09-17

- **One deleted directory no longer blanks the whole Explorer tab.** The
  listing omits item counts for an entry that is gone, deliberately: they come
  from the resident tree, and sending zero would read as "it was empty" rather
  than "it is not there any more". The web UI decided whether to read those
  counts from `kind` alone, and a deleted directory is still a directory — so
  it read `undefined`, threw, and the error propagated up through the one
  handler that loads all three Explorer panes. The result was an error toast
  reading `Cannot read properties of undefined` and an empty tab: no contents
  listing, and no composition chart either, since the throw happened before it
  was reached. Any root where a directory was merely removed was affected, and
  0.5.0 made it reachable by listing deleted entries in the first place.

- **An absolute exclude no longer disappears when the root is reached through
  a symlink.** `scan()` canonicalizes its root, then kept each configured
  `exclude_paths` entry in whatever form it was written and filtered them with
  `starts_with(root)`. A root behind a link failed every one of those
  comparisons, so the prefixes were discarded in silence and the scan ran with
  no absolute excludes at all while reporting nothing unusual. The default
  configuration ships system paths in exactly that field, so on such a host
  the result was a walk into `/proc` and its neighbours, or simply a wrong
  total, with no signal that anything had been ignored. Prefixes are now
  resolved the same way the root is. One that does not exist keeps its literal
  form, since excluding a path that is not there yet is legitimate.

  Found because the build directory was moved out of a file-sync folder and
  symlinked back, which put a link in the path of the `du` oracle's own
  fixtures and turned the silent failure into a failing test.

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
