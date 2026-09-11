/* duTime front end.
 *
 * No build step and no framework: a vendored ECharts plus this file. The
 * service has to keep building on an unattended Ubuntu box years from now,
 * and an npm toolchain is a permanent tax for no benefit at this size.
 *
 * Colour is taken from CSS custom properties rather than hardcoded, so the
 * validated palette lives in exactly one place and theme switching is a single
 * re-read.
 */
'use strict';

const $ = (s) => document.querySelector(s);
const $$ = (s) => Array.from(document.querySelectorAll(s));

const state = {
  root: null,
  metric: 'apparent',
  window: '-24h',
  path: null,
  scans: [],
  scanIdx: 0,
  mode: 'exclusive',
  listSort: 'size',
  listDesc: true,
  /// 'relative' scales each trend to its own range; 'absolute' puts them all
  /// on one scale from zero. Remembered per browser, since which question you
  /// are asking tends to be a habit rather than a per-visit decision.
  sparkScale: localStorage.getItem('dutime.sparkScale') || 'relative',
  dir: 'gainers',
  collapse: true,
  view: 'overview',
  lastChanges: [],
};

const charts = {};

// ── helpers ────────────────────────────────────────────────────────────

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/** Binary sizes, matching what the CLI prints. */
function fmtSize(n) {
  const neg = n < 0;
  let v = Math.abs(n);
  const u = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  let i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  const s = i === 0 ? `${v} B` : `${v.toFixed(1)} ${u[i]}`;
  return neg ? `−${s}` : s;
}

/** Signed size with an explicit glyph, so growth never reads by colour alone. */
function fmtDelta(n) {
  if (n === 0) return '0';
  return (n > 0 ? '+' : '−') + fmtSize(Math.abs(n));
}

/** Item counts, compactly.
 *
 * A directory with 895,930 files under it does not need nine characters
 * spent on the last three digits — nobody reads a file count to the unit,
 * and at these magnitudes the exact figure changes between one scan and the
 * next anyway. Below 10,000 the exact number is short enough to keep, so it
 * is kept. The precise value always survives in the row's tooltip.
 */
function fmtCount(n) {
  if (n < 10_000) return n.toLocaleString();
  if (n < 1_000_000) return `${Math.round(n / 1000).toLocaleString()}k`;
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  return `${(n / 1_000_000_000).toFixed(1)}B`;
}

function fmtTime(epoch) {
  const d = new Date(epoch * 1000);
  return d.toLocaleString(undefined, {
    year: 'numeric', month: 'short', day: '2-digit',
    hour: '2-digit', minute: '2-digit',
  });
}

function fmtDate(epoch) {
  return new Date(epoch * 1000).toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

function toast(msg) {
  const t = $('#toast');
  t.textContent = msg;
  t.classList.add('on');
  clearTimeout(toast._t);
  toast._t = setTimeout(() => t.classList.remove('on'), 5000);
}

/* What to do about an unreadable path, which depends on where it lives.
 *
 * On a local filesystem the kernel checks permissions, so CAP_DAC_READ_SEARCH
 * bypasses them. On NFS or SMB the *server* checks, against the numeric uid
 * duTime presents, and cannot see a client capability — so the usual advice
 * is not merely unhelpful there, it sends you to verify something that was
 * never going to work.
 */
function permissionAdvice(o) {
  if (o.server_authorized) {
    return `This root is on <b>${escapeHtml(o.fstype)}</b>, where the server enforces `
      + 'permissions against the uid duTime presents. A capability on this machine '
      + 'changes nothing, and root_squash means running as root reads less, not more. '
      + 'Make the uid match: run duTime as the user that owns the files, or grant that '
      + 'uid access on the server.';
  }
  if (o.fstype) {
    return `This root is on <b>${escapeHtml(o.fstype)}</b>, a local filesystem, so `
      + 'CAP_DAC_READ_SEARCH does grant read and traverse on everything. The system unit '
      + 'sets it; the --user unit deliberately has none.';
  }
  return 'Run duTime with CAP_DAC_READ_SEARCH (the system unit does) to read directories '
    + 'it does not own — unless this root is on a network share, where the server checks '
    + 'the uid instead and capabilities do not apply.';
}

// ── sign-in ────────────────────────────────────────────────────────────

/* The token lives in localStorage and rides on an Authorization header.
 *
 * Not a cookie: a cookie is attached by the browser to any request to this
 * origin, including one triggered by a form on someone else's page, which is
 * what CSRF is. A header has to be set deliberately by our own code, so that
 * whole class of problem does not arise and there is no need for tokens,
 * double-submit or SameSite reasoning.
 *
 * localStorage rather than sessionStorage so a reload does not sign you out,
 * which for a dashboard left open on a second monitor is the difference
 * between useful and irritating.
 */
const TOKEN_KEY = 'dutime.token';
const getToken = () => localStorage.getItem(TOKEN_KEY) || '';
const setToken = (t) => t ? localStorage.setItem(TOKEN_KEY, t) : localStorage.removeItem(TOKEN_KEY);

async function api(path, params = {}) {
  const q = new URLSearchParams();
  if (state.root != null) q.set('root', state.root);
  q.set('metric', state.metric);
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null) q.set(k, v);
  }
  const headers = {};
  const t = getToken();
  if (t) headers.Authorization = `Bearer ${t}`;
  const r = await fetch(`/api/v1/${path}?${q}`, { headers });
  const j = await r.json().catch(() => ({ error: `${r.status} ${r.statusText}` }));
  if (r.status === 401) {
    const e = new Error(j.error || 'sign in to view this');
    e.needsAuth = true;
    throw e;
  }
  if (!r.ok) throw new Error(j.error || `request failed: ${r.status}`);
  return j;
}

/** Reflect sign-in state in the header, and offer the way in. */
async function refreshAuth() {
  let a;
  try {
    a = await api('auth');
  } catch {
    return; // an older server, or one that is simply down
  }
  const btn = $('#signin');
  // Hidden entirely when nothing is gated: an affordance that cannot
  // accomplish anything is just a question the user has to answer.
  btn.hidden = !a.required && !a.authenticated;
  btn.textContent = a.authenticated ? '\u{1F513}' : '\u{1F512}';
  btn.title = a.authenticated ? 'Signed in — click to sign out' : 'Sign in to view protected roots';
  btn.classList.toggle('authed', a.authenticated);
  state.authed = a.authenticated;
}

function openSignIn(message) {
  const d = $('#authDialog');
  $('#authErr').hidden = !message;
  $('#authErr').textContent = message || '';
  $('#authToken').value = '';
  d.showModal();
  $('#authToken').focus();
}

async function trySignIn(token) {
  // Verified against the server before being stored, so a typo is reported
  // now rather than as a broken dashboard later.
  const r = await fetch('/api/v1/auth', { headers: { Authorization: `Bearer ${token}` } });
  const j = await r.json().catch(() => ({}));
  if (!j.authenticated) return false;
  setToken(token);
  return true;
}

function wireAuth() {
  $('#signin').addEventListener('click', async () => {
    if (state.authed) {
      setToken('');
      await refreshAuth();
      await boot();
      toast('Signed out.');
    } else {
      openSignIn();
    }
  });

  $('#authCancel').addEventListener('click', () => $('#authDialog').close());

  $('#authForm').addEventListener('submit', async (e) => {
    e.preventDefault();
    const t = $('#authToken').value.trim();
    if (!t) return;
    const ok = await trySignIn(t);
    if (!ok) {
      $('#authErr').hidden = false;
      $('#authErr').textContent = 'That token was not accepted.';
      $('#authToken').select();
      return;
    }
    $('#authDialog').close();
    await refreshAuth();
    await boot();
    toast('Signed in.');
  });
}

/** Choose an axis max and tick interval on binary boundaries.
 *
 * Sizes are powers of two, but a linear axis picks round *decimal* values, so
 * the ticks come out as "953.7 MiB" and "762.9 MiB" — technically correct and
 * unreadable. Stepping on 1/2/5 x a binary unit gives "256 MiB", "512 MiB".
 */
function binaryAxis(maxValue) {
  if (!(maxValue > 0)) return {};
  let unit = 1;
  while (maxValue / unit >= 1024 && unit < 1024 ** 5) unit *= 1024;
  // Drop a unit when the value only just clears it, so 1.2 GiB is stepped in
  // MiB (256/512/768/1024) rather than getting one tick at 1 GiB and a 2 GiB
  // ceiling with nothing in between.
  if (maxValue / unit < 4 && unit > 1) unit /= 1024;
  const steps = [1, 2, 4, 5, 8, 10, 16, 20, 25, 32, 50, 64, 100, 128, 200, 256, 512, 1024];
  const want = (maxValue / unit) / 4;             // aim for ~4-5 ticks
  const step = steps.find((x) => x >= want) ?? 1024;
  const interval = step * unit;
  return { max: Math.ceil(maxValue / interval) * interval, interval };
}

/** Shared ECharts chrome: hairline grid, recessive axes, muted ink. */
function baseOption() {
  return {
    backgroundColor: 'transparent',
    textStyle: { fontFamily: 'system-ui, -apple-system, "Segoe UI", sans-serif' },
    animationDuration: 260,
    grid: { left: 68, right: 22, top: 18, bottom: 34, containLabel: false },
    xAxis: {
      type: 'time',
      axisLine: { lineStyle: { color: cssVar('--axis'), width: 1 } },
      axisTick: { show: false },
      axisLabel: { color: cssVar('--text-muted'), fontSize: 11, hideOverlap: true },
      splitLine: { show: false },
    },
    yAxis: {
      type: 'value',
      axisLine: { show: false },
      axisTick: { show: false },
      axisLabel: { color: cssVar('--text-muted'), fontSize: 11, formatter: fmtSize },
      splitLine: { lineStyle: { color: cssVar('--grid'), width: 1, type: 'solid' } },
    },
    tooltip: {
      trigger: 'axis',
      axisPointer: { type: 'line', lineStyle: { color: cssVar('--axis'), width: 1 } },
      backgroundColor: cssVar('--surface-1'),
      borderColor: cssVar('--border'),
      textStyle: { color: cssVar('--text-primary'), fontSize: 12 },
      extraCssText: 'box-shadow:0 4px 16px rgba(0,0,0,.16);border-radius:8px;',
    },
  };
}

function chart(id) {
  if (!charts[id]) charts[id] = echarts.init($('#' + id), null, { renderer: 'canvas' });
  return charts[id];
}

function seriesColors() {
  return ['--s1', '--s2', '--s3', '--s4', '--s5', '--s6', '--s7', '--s8'].map(cssVar);
}

// ── overview ───────────────────────────────────────────────────────────

async function loadOverview() {
  const o = await api('overview');
  state.rootPath = o.path.name;

  const used = o.history.length ? o.history[o.history.length - 1][1] : 0;
  const free = o.fs.free;
  const total = o.fs.total;
  const pct = total ? Math.round(((total - free) / total) * 100) : null;

  const tiles = [
    { k: 'Tracked size', v: fmtSize(used), m: o.path.name },
    {
      k: 'Filesystem', v: pct != null ? `${pct}% full` : '—',
      m: total ? `${fmtSize(free)} free of ${fmtSize(total)}` : 'not recorded',
      cls: pct != null && pct >= 90 ? 'warn' : '',
    },
    forecastTile(o.forecast),
    {
      k: 'Samples', v: String(o.scans),
      m: o.first_scan ? `since ${fmtTime(o.first_scan.at)}` : '—',
    },
  ];
  $('#tiles').innerHTML = tiles.map((t) => `
    <div class="tile">
      <div class="k">${t.k}</div>
      <div class="v">${t.v}</div>
      <div class="m ${t.cls || ''}">${t.m}</div>
    </div>`).join('');

  // One series, so no legend: the caption names it.
  const c = chart('usedChart');
  const pts = o.history.map(([t, b]) => [t * 1000, b]);
  const opt = baseOption();
  opt.grid.top = 26;
  Object.assign(opt.yAxis, binaryAxis(Math.max(...pts.map((p) => p[1]), 0)));
  opt.series = [{
    type: 'line', name: 'Tracked size', data: pts,
    showSymbol: false, symbolSize: 8,
    lineStyle: { width: 2, color: cssVar('--s1') },
    areaStyle: { color: cssVar('--s1'), opacity: 0.10 },
    emphasis: { focus: 'series' },
  }];
  opt.tooltip.formatter = (ps) => {
    const p = ps[0];
    return `<b>${fmtTime(p.value[0] / 1000)}</b><br>${fmtSize(p.value[1])}`;
  };
  c.setOption(opt, true);

  $('#usedSub').textContent = o.scans < 2
    ? 'Only one sample so far — a trend appears once duTime has scanned a few times.'
    : `${o.scans} samples since ${fmtTime(o.first_scan.at)}.`;

  // A scan that could not read part of the tree reports a total that is too
  // low, and a shortfall that is never mentioned is indistinguishable from a
  // real shrink — the one confusion this tool exists to prevent.
  $('#scanWarn').innerHTML = o.scan_status === 'partial'
    ? `<div class="notice" role="status">
         <strong>This scan is incomplete.</strong> Some paths could not be read, so
         every total below is lower than the truth. Growth trends are still
         meaningful when the same paths fail each time.
         ${o.scan_error ? `<div class="detail">${escapeHtml(o.scan_error)}</div>` : ''}
         <div class="detail">${permissionAdvice(o)}</div>
       </div>`
    : '';

  await loadGainers('#gainTable', 8);
}

function forecastTile(f) {
  if (!f || f.status === 'insufficient_history') {
    const n = f && f.needs;
    const m = n
      ? `needs ${n.samples} samples over ${n.span_hours}h — have ${n.have_samples} over ${n.have_span_hours.toFixed(1)}h`
      : 'needs more samples';
    return { k: 'Projected full in', v: 'not yet', m };
  }
  if (f.trend_bytes_per_day == null) {
    return { k: 'Trend', v: '—', m: 'needs more samples' };
  }
  const perDay = f.trend_bytes_per_day;
  if (f.days_to_full == null) {
    return { k: 'Free space trend', v: 'stable', m: `${fmtDelta(Math.round(perDay))}/day`, cls: 'good' };
  }
  const d = f.days_to_full;
  const v = d < 1 ? 'under a day' : d < 400 ? `~${Math.round(d)} days` : 'over a year';
  return {
    k: 'Projected full in', v,
    m: `${fmtDelta(Math.round(perDay))}/day of free space`,
    cls: d < 30 ? 'warn' : '',
  };
}

// ── gainers table ──────────────────────────────────────────────────────

async function loadGainers(sel, limit) {
  const g = await api('gainers', {
    from: state.window, mode: state.mode,
    limit, losers: state.dir === 'losers', collapse: state.collapse,
  });
  state.lastChanges = g.results;

  const el = $(sel);
  if (!g.results.length) {
    el.innerHTML = `<div class="empty">Nothing ${state.dir === 'losers' ? 'shrank' : 'grew'} in this window.</div>`;
  } else {
    const rows = g.results.map((r) => `
      <tr>
        <td class="num ${r.delta > 0 ? 'up' : 'down'}">${fmtDelta(r.delta)}</td>
        <td class="path">${escapeHtml(r.path.name)}${r.path.lossy ? ' <span title="filename is not valid UTF-8">⚠</span>' : ''}</td>
      </tr>`).join('');
    el.innerHTML = `<table><thead><tr><th class="num">Change</th><th>Path</th></tr></thead><tbody>${rows}</tbody></table>`;
  }

  const note = g.window_clamped_to_first_scan
    ? ` Tracking only began at ${fmtTime(g.from.at)}, so this covers less than the window asked for.`
    : '';
  const sub = $('#gainSub');
  if (sub && sel === '#gainTable') {
    sub.textContent = `Between ${fmtTime(g.from.at)} and ${fmtTime(g.to.at)}.${note}`;
  }
  return g;
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

// ── explorer ───────────────────────────────────────────────────────────

async function loadScans() {
  const s = await api('scans', { limit: 2000 });
  state.scans = s.scans;
  const sl = $('#timeSlider');
  sl.max = Math.max(0, state.scans.length - 1);
  if (state.scanIdx > sl.max) state.scanIdx = sl.max;
  sl.value = state.scanIdx;
  updateTimeLabel();

  const opts = state.scans.map((s, i) =>
    `<option value="${s.scan_id}">${fmtTime(s.at)}</option>`).join('');
  $('#diffFrom').innerHTML = opts;
  $('#diffTo').innerHTML = opts;
  if (state.scans.length) {
    const has = (v) => v && state.scans.some((s) => String(s.scan_id) === String(v));
    $('#diffFrom').value = has(state.diffFrom) ? state.diffFrom : state.scans[0].scan_id;
    $('#diffTo').value = has(state.diffTo)
      ? state.diffTo
      : state.scans[state.scans.length - 1].scan_id;
  }
}

function currentScan() {
  return state.scans[state.scanIdx];
}

function updateTimeLabel() {
  const s = currentScan();
  const isLast = state.scanIdx === state.scans.length - 1;
  $('#timeLabel').textContent = s ? (isLast ? `now (${fmtTime(s.at)})` : fmtTime(s.at)) : '—';
}

/** Ordinal blue ramp keyed to nesting depth.
 *
 * Depth is genuinely ordered, so an ordinal ramp is the right encoding — and
 * it keeps the treemap off categorical hues, which would be unreadable here:
 * treemap tiles touch each other arbitrarily, so every pair of colours would
 * have to clear the all-pairs CVD floors, and only three of the eight slots do.
 * Area carries magnitude; the nested rectangles carry hierarchy.
 */
function depthColors() {
  return ['--o1', '--o2', '--o3', '--o4', '--o5'].map(cssVar);
}

async function loadTreemap() {
  const s = currentScan();
  const t = await api('tree', {
    path: state.path, at: s ? `scan:${s.scan_id}` : 'now', depth: 4, limit: 300,
  });
  state.path = t.path.name;
  renderCrumbs();

  const ramp = depthColors();
  const paint = (node, d) => {
    const color = node.kind === 'other' || node.kind === 'own'
      ? cssVar('--s-other')
      : ramp[Math.min(d, ramp.length - 1)];
    const out = {
      name: node.name, value: node.value, itemStyle: { color },
      _kind: node.kind, _files: node.files, _lossy: node.lossy,
    };
    if (node.children) out.children = node.children.map((c) => paint(c, d + 1));
    return out;
  };

  const c = chart('treemap');
  c.setOption({
    backgroundColor: 'transparent',
    textStyle: { fontFamily: 'system-ui, -apple-system, "Segoe UI", sans-serif' },
    tooltip: {
      backgroundColor: cssVar('--surface-1'),
      borderColor: cssVar('--border'),
      textStyle: { color: cssVar('--text-primary'), fontSize: 12 },
      extraCssText: 'box-shadow:0 4px 16px rgba(0,0,0,.16);border-radius:8px;',
      formatter: (p) => {
        const files = p.data._files != null ? `<br><span style="color:${cssVar('--text-muted')}">${p.data._files.toLocaleString()} files</span>` : '';
        return `<b>${escapeHtml(p.name)}</b><br>${fmtSize(p.value)}${files}`;
      },
    },
    series: [{
      type: 'treemap',
      roam: false,
      // Squarified layout with a visible gap rather than a stroke: a 2px
      // surface gap separates fills without adding a border to every mark.
      squareRatio: 1.618,
      nodeClick: false,
      breadcrumb: { show: false },
      itemStyle: { borderColor: cssVar('--surface-1'), borderWidth: 2, gapWidth: 2 },
      upperLabel: {
        show: true, height: 20, color: cssVar('--text-primary'),
        fontSize: 11, fontWeight: 560, overflow: 'truncate',
      },
      label: {
        show: true, fontSize: 11, color: '#fff', overflow: 'truncate',
        formatter: (p) => (p.value > 0 ? `${p.name}\n${fmtSize(p.value)}` : p.name),
      },
      labelLayout: (p) => ({
        // Anti-pattern guard: never render a label clipped by its own tile.
        // Tiny leaves keep their colour and their tooltip; the text goes.
        fontSize: p.rect && (p.rect.width < 54 || p.rect.height < 20) ? 0 : 11,
      }),

      levels: [
        { itemStyle: { borderWidth: 0, gapWidth: 2 } },
        { itemStyle: { gapWidth: 2 } },
        { itemStyle: { gapWidth: 1 } },
        { itemStyle: { gapWidth: 1 } },
      ],
      data: (t.node.children || [t.node]).map((n) => paint(n, 1)),
    }],
  }, true);

  c.off('click');
  c.on('click', (p) => {
    if (!p.data || p.data._kind === 'other' || p.data._kind === 'own') return;
    const parts = p.treePathInfo.slice(1).map((n) => n.name);
    state.path = joinPath(t.path.name, parts);
    loadTreemap();
    loadListing();
    loadSeries();
  });

  return t;
}

function joinPath(base, parts) {
  return parts.length ? base.replace(/\/$/, '') + '/' + parts.join('/') : base;
}

function renderCrumbs() {
  const rootPath = state.rootPath || '/';
  const rel = (state.path || rootPath).slice(rootPath.length).split('/').filter(Boolean);
  const items = [{ label: rootPath, path: rootPath }];
  let acc = rootPath;
  for (const seg of rel) {
    acc = acc.replace(/\/$/, '') + '/' + seg;
    items.push({ label: seg, path: acc });
  }
  $('#crumbs').innerHTML = items.map((it, i) =>
    `${i ? '<span class="sep">/</span>' : ''}<button data-path="${escapeHtml(it.path)}">${escapeHtml(it.label)}</button>`
  ).join('');
  $$('#crumbs button').forEach((b) => b.addEventListener('click', () => {
    state.path = b.dataset.path;
    loadTreemap();
    loadListing();
    loadSeries();
  }));
}

async function loadSeries() {
  let s;
  try {
    s = await api('series', { path: state.path, from: state.window, children: 8 });
  } catch (e) {
    chart('seriesChart').clear();
    $('#seriesTable').innerHTML = `<div class="empty">${escapeHtml(e.message)}</div>`;
    return;
  }
  $('#seriesTitle').textContent = `Composition of ${s.path.name}`;

  const colors = seriesColors();
  const times = s.times.map((t) => t * 1000);
  const series = s.bands.map((b, i) => ({
    name: b.name,
    type: 'line',
    stack: 'total',
    // A 2px surface-coloured line between stacked fills is the gap spec:
    // separation without drawing a border around every mark.
    lineStyle: { width: 2, color: cssVar('--surface-1') },
    areaStyle: { color: b.synthetic ? cssVar('--s-other') : colors[i % 8], opacity: 0.92 },
    itemStyle: { color: b.synthetic ? cssVar('--s-other') : colors[i % 8] },
    showSymbol: false,
    emphasis: { focus: 'series' },
    data: b.points.map((v, k) => [times[k], v]),
  }));

  const opt = baseOption();
  opt.grid.top = 12;
  opt.grid.bottom = 62;
  const stackMax = times.length
    ? Math.max(...times.map((_, k) => s.bands.reduce((a, b) => a + (b.points[k] || 0), 0)))
    : 0;
  Object.assign(opt.yAxis, binaryAxis(stackMax));
  opt.legend = {
    bottom: 0, type: 'scroll',
    textStyle: { color: cssVar('--text-secondary'), fontSize: 11 },
    icon: 'roundRect', itemWidth: 10, itemHeight: 10,
  };
  opt.series = series;
  opt.tooltip.formatter = (ps) => {
    if (!ps.length) return '';
    const rows = ps.slice().reverse()
      .map((p) => `<div style="display:flex;gap:10px"><span style="flex:1">${p.marker} ${escapeHtml(p.seriesName)}</span><b>${fmtSize(p.value[1])}</b></div>`)
      .join('');
    return `<b>${fmtTime(ps[0].value[0] / 1000)}</b>${rows}`;
  };
  chart('seriesChart').setOption(opt, true);

  // Table view: the accessible twin, and the relief for the light-mode
  // categorical slots that sit below 3:1 against the surface.
  const last = (b) => (b.points.length ? b.points[b.points.length - 1] : 0);
  const first = (b) => (b.points.length ? b.points[0] : 0);
  $('#seriesTable').innerHTML = `<table><thead><tr>
      <th>Child</th><th class="num">At start</th><th class="num">Now</th><th class="num">Change</th>
    </tr></thead><tbody>${s.bands.map((b, i) => {
      const d = last(b) - first(b);
      const sw = b.synthetic ? cssVar('--s-other') : colors[i % 8];
      return `<tr>
        <td><span class="swatch" style="background:${sw}"></span>${escapeHtml(b.name)}</td>
        <td class="num">${fmtSize(first(b))}</td>
        <td class="num">${fmtSize(last(b))}</td>
        <td class="num ${d > 0 ? 'up' : d < 0 ? 'down' : ''}">${fmtDelta(d)}</td>
      </tr>`;
    }).join('')}</tbody></table>`;
}

// ── directory listing ──────────────────────────────────────────────────

/** An inline sparkline as plain SVG.
 *
 * One tiny SVG per row rather than a chart instance per row: a directory with
 * 200 children would otherwise mean 200 ECharts instances, each with its own
 * canvas and resize observer.
 *
 * Two scales, because they answer different questions and neither answers
 * both.
 *
 * **Per row** (`domain` omitted) scales each row to its own min and max. It
 * shows *shape*: rows here differ by orders of magnitude, so one shared scale
 * flattens every small directory to a dead line, and a directory quietly
 * doubling from 40 MB is exactly the thing you want to catch early.
 *
 * **Shared** takes an explicit domain covering every row, so a given height
 * means the same number of bytes everywhere and the biggest movers are
 * obvious at a glance. That domain starts at **zero**, not at the smallest
 * value across the rows: these marks are filled areas, and a filled area on a
 * non-zero baseline overstates every difference — 380 GB and 420 GB would
 * render as a tenfold gap. Per-row mode accepts that distortion knowingly in
 * exchange for showing shape; shared mode exists to compare magnitudes, so it
 * cannot.
 *
 * In neither mode is the magnitude left to the picture: the size and change
 * columns carry the real figures and the row title gives the range.
 */
function sparkSvg(vals, domain = null, w = 132, h = 24) {
  const pad = 2;
  if (!vals || vals.length === 0) return '';
  const min = domain ? domain.min : Math.min(...vals);
  const max = domain ? domain.max : Math.max(...vals);
  const span = max - min;
  const iw = w - pad * 2, ih = h - pad * 2;

  // A directory that never moved gets a flat rule, not a spike from noise.
  // Only when the *domain* is degenerate, though: in shared mode an unchanged
  // row still has a meaningful height, and drawing it mid-chart would put a
  // 2 MB directory level with a 400 GB one.
  if (span === 0) {
    const y = (h / 2).toFixed(1);
    return `<svg class="spark" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" aria-hidden="true">`
      + `<line x1="${pad}" y1="${y}" x2="${w - pad}" y2="${y}" `
      + `stroke="var(--text-muted)" stroke-width="1.5" stroke-linecap="round" opacity="0.55"/></svg>`;
  }

  const x = (i) => pad + (vals.length === 1 ? iw : (i * iw) / (vals.length - 1));
  const y = (v) => pad + ih - ((v - min) / span) * ih;
  const pts = vals.map((v, i) => `${x(i).toFixed(1)},${y(v).toFixed(1)}`).join(' ');
  const area = `${pad},${(h - pad).toFixed(1)} ${pts} ${(w - pad).toFixed(1)},${(h - pad).toFixed(1)}`;
  const lx = x(vals.length - 1).toFixed(1), ly = y(vals[vals.length - 1]).toFixed(1);

  return `<svg class="spark" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" aria-hidden="true">`
    + `<polygon points="${area}" fill="var(--s1)" opacity="0.12"/>`
    + `<polyline points="${pts}" fill="none" stroke="var(--s1)" stroke-width="1.5" `
    + `stroke-linejoin="round" stroke-linecap="round"/>`
    + `<circle cx="${lx}" cy="${ly}" r="2" fill="var(--s1)"/></svg>`;
}

const KIND_ICON = { dir: '\u{1F4C1}', file: '\u{1F4C4}', symlink: '\u{21B3}', other: '\u{2022}' };

async function loadListing() {
  let d;
  try {
    d = await api('listing', { path: state.path, from: state.window, points: 40, limit: 400 });
  } catch (e) {
    $('#listing').innerHTML = `<div class="empty">${escapeHtml(e.message)}</div>`;
    return;
  }

  const rows = d.rows.slice();
  const key = state.listSort;
  rows.sort((a, b) => {
    const v = key === 'name'
      ? String(a.name).localeCompare(String(b.name))
      : (a[key] ?? 0) - (b[key] ?? 0);
    return state.listDesc ? -v : v;
  });

  const sortAttr = (k) =>
    state.listSort === k ? ` aria-sort="${state.listDesc ? 'descending' : 'ascending'}"` : '';

  // One domain for every row, from zero to the largest value any row reaches.
  // Computed over the rows actually shown, so hiding or truncating entries
  // cannot leave the scale pinned to something that is not on screen.
  const shared = state.sparkScale === 'absolute';
  const ceiling = shared
    ? Math.max(0, ...rows.flatMap((r) => r.spark || []))
    : 0;
  const domain = shared ? { min: 0, max: ceiling } : null;

  const body = rows.map((r) => {
    const dirish = r.kind === 'dir';
    const range = r.spark && r.spark.length
      ? `${fmtSize(r.spark[0])} → ${fmtSize(r.spark[r.spark.length - 1])}`
        + (shared ? ` (of ${fmtSize(ceiling)} full height)` : '')
      : '';
    const count = dirish ? `${fmtCount(r.dirs)} dirs, ${fmtCount(r.files)} files` : '';
    const countExact = dirish
      ? `${r.dirs.toLocaleString()} directories, ${r.files.toLocaleString()} files`
      : '';
    return `<tr class="${dirish ? 'clickable' : ''}" data-name="${escapeHtml(r.name)}" data-dir="${dirish}"${dirish ? ' tabindex="0"' : ''}>
      <td class="name" title="${escapeHtml(r.name)}">
        <span class="ico">${KIND_ICON[r.kind] || ''}</span>${escapeHtml(r.name)}${r.lossy ? ' <span title="filename is not valid UTF-8">⚠</span>' : ''}
      </td>
      <td class="num sz">${fmtSize(r.size)}</td>
      <td class="num dl ${r.delta > 0 ? 'up' : r.delta < 0 ? 'down' : ''}">${r.delta ? fmtDelta(r.delta) : '—'}</td>
      <td class="trend" title="${range}">${sparkSvg(r.spark, domain)}</td>
      <td class="num ct" title="${countExact}">${count}</td>
    </tr>`;
  }).join('');

  // Pinned above the sorted rows, never sorted into them: it is navigation,
  // not data. The breadcrumbs can do this too, but the eye is already in the
  // table when you decide you went the wrong way.
  // Deliberately no size or change: a figure in a size column that is sorted
  // descending, sitting above a smaller child, reads as a broken sort. The
  // parent's numbers go in the tooltip, where they inform without competing.
  const up = d.parent
    ? `<tr class="updir clickable" data-path="${escapeHtml(d.parent.path.name)}" tabindex="0"
           title="Up to ${escapeHtml(d.parent.path.name)} \u2014 ${fmtSize(d.parent.size)}${d.parent.delta ? `, ${fmtDelta(d.parent.delta)}` : ''}">
         <td class="name">
           <span class="ico">\u{21B0}</span>..<span class="upname">${escapeHtml(d.parent.name)}</span>
         </td>
         <td class="num sz"></td><td class="num dl"></td>
         <td class="trend"></td><td class="num ct"></td>
       </tr>`
    : '';

  const own = d.own && d.own.size > 0
    ? `<tr class="own">
         <td class="name">files in this directory${d.own.files ? ` (${fmtCount(d.own.files)})` : ''}</td>
         <td class="num sz">${fmtSize(d.own.size)}</td>
         <td class="num dl ${d.own.delta > 0 ? 'up' : d.own.delta < 0 ? 'down' : ''}">${d.own.delta ? fmtDelta(d.own.delta) : '—'}</td>
         <td class="trend"></td><td class="num ct"></td>
       </tr>`
    : '';

  const more = d.truncated
    ? `<div class="more">${d.truncated.toLocaleString()} smaller entries not shown</div>`
    : '';

  $('#listing').innerHTML = rows.length || own || up
    ? `<div class="listing"><table>
        <thead><tr>
          <th class="name sortable"${sortAttr('name')} data-sort="name">Name</th>
          <th class="num sz sortable"${sortAttr('size')} data-sort="size">Size</th>
          <th class="num dl sortable"${sortAttr('delta')} data-sort="delta">Change</th>
          <th class="trend">Trend</th>
          <th class="num ct">Contents</th>
        </tr></thead>
        <tbody>${up}${body}${own}</tbody>
      </table>${more}</div>`
    : '<div class="empty">This directory is empty, or everything in it is below the tracking threshold.</div>';

  $('#listSub').textContent =
    `${rows.length.toLocaleString()} entries · trend covers ${fmtTime(d.from.at)} to ${fmtTime(d.to.at)}`
    + (d.window_clamped_to_first_scan ? ' (all the history there is)' : '')
    // A shared scale is only readable if you are told what it is.
    + (shared
      ? ` · trends share one scale, 0 to ${fmtSize(ceiling)}`
      : ' · each trend scaled to its own range');

  $$('#listing tr.clickable').forEach((tr) => {
    const go = () => {
      state.path = tr.dataset.path
        ?? joinPath(state.path || state.rootPath, [tr.dataset.name]);
      loadTreemap();
      loadListing();
      loadSeries();
    };
    tr.addEventListener('click', go);
    tr.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); go(); }
    });
  });

  $$('[data-spark]').forEach((b) => b.classList.toggle('on', b.dataset.spark === state.sparkScale));

  $$('#listing th.sortable').forEach((th) => th.addEventListener('click', () => {
    const k = th.dataset.sort;
    if (state.listSort === k) state.listDesc = !state.listDesc;
    else { state.listSort = k; state.listDesc = k !== 'name'; }
    loadListing();
  }));
}

// ── diff treemap ───────────────────────────────────────────────────────

/** Map a relative change to the diverging ramp.
 *
 * Blue <-> red with a neutral grey midpoint, not red/green: red and green are
 * the classic dichromat trap, while this pair separates by Delta E 20-26 under
 * both protan and deutan simulation. Lightness also steps monotonically out
 * from the middle, and every tile carries a signed label, so the reading
 * survives greyscale printing and colour blindness alike.
 */
function divergingColor(delta, before) {
  if (delta === 0) return cssVar('--d0');
  const base = Math.max(before, Math.abs(delta), 1);
  const r = Math.abs(delta) / base;
  const step = r < 0.05 ? 1 : r < 0.4 ? 2 : 3;
  return cssVar(delta > 0 ? `--u${step}` : `--d${step}`);
}

async function loadDiff() {
  const from = $('#diffFrom').value, to = $('#diffTo').value;
  if (!from || !to) return;
  let d;
  try {
    d = await api('diff', { from: `scan:${from}`, to: `scan:${to}`, depth: 4, limit: 300 });
  } catch (e) { toast(e.message); return; }

  const paint = (n) => {
    const o = {
      // Area is max(before, after) so a deleted directory still occupies the
      // space it used to, instead of silently vanishing from the picture.
      name: n.gone ? `${n.name} (deleted)` : n.name,
      value: n.value,
      itemStyle: { color: divergingColor(n.delta, n.before) },
      _delta: n.delta, _before: n.before, _after: n.after, _gone: n.gone,
    };
    if (n.children) o.children = n.children.map(paint);
    return o;
  };

  chart('diffmap').setOption({
    backgroundColor: 'transparent',
    textStyle: { fontFamily: 'system-ui, -apple-system, "Segoe UI", sans-serif' },
    tooltip: {
      backgroundColor: cssVar('--surface-1'),
      borderColor: cssVar('--border'),
      textStyle: { color: cssVar('--text-primary'), fontSize: 12 },
      extraCssText: 'box-shadow:0 4px 16px rgba(0,0,0,.16);border-radius:8px;',
      formatter: (p) => `<b>${escapeHtml(p.name)}</b>`
        + (p.data._gone ? ' <span style="opacity:.7">(deleted)</span>' : '')
        + `<br>${fmtSize(p.data._before)} → ${fmtSize(p.data._after)}<br>`
        + `<b>${fmtDelta(p.data._delta)}</b>`,
    },
    series: [{
      type: 'treemap',
      roam: false, nodeClick: false, squareRatio: 1.618,
      breadcrumb: { show: false },
      itemStyle: { borderColor: cssVar('--surface-1'), borderWidth: 2, gapWidth: 2 },
      upperLabel: { show: true, height: 20, color: cssVar('--text-primary'), fontSize: 11, overflow: 'truncate' },
      label: {
        show: true, fontSize: 11, color: cssVar('--text-primary'), overflow: 'truncate',
        // The signed figure is the secondary encoding: colour is never the
        // only thing saying which way a tile moved.
        formatter: (p) => (p.data._delta ? `${p.name}\n${fmtDelta(p.data._delta)}` : p.name),
      },
      labelLayout: (p) => ({
        // Anti-pattern guard: never render a label clipped by its own tile.
        // Tiny leaves keep their colour and their tooltip; the text goes.
        fontSize: p.rect && (p.rect.width < 54 || p.rect.height < 20) ? 0 : 11,
      }),

      data: (d.node.children || [d.node]).map(paint),
    }],
  }, true);
}

// ── wiring ─────────────────────────────────────────────────────────────

async function refresh() {
  try {
    if (state.view === 'overview') await loadOverview();
    else if (state.view === 'explorer') { await loadTreemap(); await loadListing(); await loadSeries(); }
    else if (state.view === 'changes') await loadGainers('#changesTable', 100);
    else if (state.view === 'compare') await loadDiff();
  } catch (e) {
    // A 401 has an answer, so offer it rather than reporting a dead end.
    if (e.needsAuth) openSignIn(e.message);
    else toast(e.message);
  }
}

/** Encode the current view in the URL so it can be shared or reloaded.
 *
 * Pasting a link to exactly what you are looking at is most of what makes a
 * diagnostic tool usable in a ticket or a chat thread.
 */
function syncHash() {
  const p = new URLSearchParams();
  p.set('view', state.view);
  if (state.path && state.path !== state.rootPath) p.set('path', state.path);
  if (state.window !== '-24h') p.set('window', state.window);
  if (state.metric !== 'apparent') p.set('metric', state.metric);
  // Travels in the link: a shared-scale listing is the thing worth pasting
  // into a ticket, and it does not read the same at the default.
  if (state.view === 'explorer' && state.sparkScale !== 'relative') {
    p.set('spark', state.sparkScale);
  }
  if (state.view === 'compare') {
    // A comparison is the thing most worth sharing: "look at what happened
    // between these two moments" is the whole point of the view.
    const f = $('#diffFrom').value, t = $('#diffTo').value;
    if (f) p.set('from', f);
    if (t) p.set('to', t);
  }
  const h = '#' + p.toString();
  if (location.hash !== h) history.replaceState(null, '', h);
}

function readHash() {
  const p = new URLSearchParams(location.hash.slice(1));
  const v = p.get('view');
  if (v && ['overview', 'explorer', 'changes', 'compare'].includes(v)) state.view = v;
  if (p.get('path')) state.path = p.get('path');
  if (p.get('window')) state.window = p.get('window');
  if (p.get('metric')) state.metric = p.get('metric');
  if (['relative', 'absolute'].includes(p.get('spark'))) state.sparkScale = p.get('spark');
  state.diffFrom = p.get('from');
  state.diffTo = p.get('to');
}

function switchView(v) {
  state.view = v;
  $$('.tabs button').forEach((b) => b.classList.toggle('on', b.dataset.view === v));
  $$('.view').forEach((s) => s.classList.toggle('on', s.id === 'view-' + v));
  syncHash();
  // ECharts cannot size a hidden container, so resize once it is visible.
  requestAnimationFrame(() => Object.values(charts).forEach((c) => c.resize()));
  refresh();
}

function applyTheme(t) {
  document.documentElement.dataset.theme = t;
  localStorage.setItem('dutime-theme', t);
  // Re-read the custom properties and rebuild, rather than tinting: the dark
  // steps are their own validated set, not an automatic flip of the light ones.
  refresh();
}

async function init() {
  const saved = localStorage.getItem('dutime-theme');
  if (saved) document.documentElement.dataset.theme = saved;
  readHash();

  $('#theme').addEventListener('click', () => {
    const cur = document.documentElement.dataset.theme;
    const isDark = cur === 'dark'
      || (cur !== 'light' && matchMedia('(prefers-color-scheme: dark)').matches);
    applyTheme(isDark ? 'light' : 'dark');
  });

  $$('.tabs button').forEach((b) =>
    b.addEventListener('click', () => switchView(b.dataset.view)));

  $$('.seg [data-metric]').forEach((b) => b.addEventListener('click', () => {
    state.metric = b.dataset.metric;
    $$('.seg [data-metric]').forEach((x) => x.classList.toggle('on', x === b));
    refresh();
  }));

  $$('.seg [data-spark]').forEach((b) => b.addEventListener('click', () => {
    state.sparkScale = b.dataset.spark;
    try { localStorage.setItem('dutime.sparkScale', state.sparkScale); } catch { /* private mode */ }
    $$('.seg [data-spark]').forEach((x) => x.classList.toggle('on', x === b));
    // The scale is a drawing decision, not a different question for the
    // server — but the listing is built in one pass, so re-render it. Only
    // when it is on screen: the control lives inside the Explorer.
    if (state.view === 'explorer') loadListing();
    syncHash();
  }));

  $$('.seg [data-mode]').forEach((b) => b.addEventListener('click', () => {
    state.mode = b.dataset.mode;
    $$('.seg [data-mode]').forEach((x) => x.classList.toggle('on', x === b));
    $('#collapseWrap').style.visibility = state.mode === 'inclusive' ? 'visible' : 'hidden';
    refresh();
  }));
  $('#collapseWrap').style.visibility = 'hidden';

  $$('.seg [data-dir]').forEach((b) => b.addEventListener('click', () => {
    state.dir = b.dataset.dir;
    $$('.seg [data-dir]').forEach((x) => x.classList.toggle('on', x === b));
    refresh();
  }));

  $('#collapse').addEventListener('change', (e) => {
    state.collapse = e.target.checked;
    refresh();
  });

  $('#window').addEventListener('change', (e) => { state.window = e.target.value; refresh(); });
  $('#root').addEventListener('change', async (e) => {
    state.root = Number(e.target.value);
    state.path = null;
    await loadScans();
    refresh();
  });

  $('#timeSlider').addEventListener('input', (e) => {
    state.scanIdx = Number(e.target.value);
    updateTimeLabel();
  });
  $('#timeSlider').addEventListener('change', () => { loadTreemap(); loadListing(); });

  $('#diffFrom').addEventListener('change', () => { loadDiff(); syncHash(); });
  $('#diffTo').addEventListener('change', () => { loadDiff(); syncHash(); });

  $('#csv').addEventListener('click', () => {
    const rows = [['delta_bytes', 'path']].concat(
      state.lastChanges.map((r) => [r.delta, r.path.name]));
    const csv = rows.map((r) => r.map((c) => `"${String(c).replace(/"/g, '""')}"`).join(',')).join('\n');
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([csv], { type: 'text/csv' }));
    a.download = 'dutime-changes.csv';
    a.click();
    URL.revokeObjectURL(a.href);
  });

  addEventListener('resize', () => Object.values(charts).forEach((c) => c.resize()));

  wireAuth();
  await refreshAuth();
  await boot();
}

/* Load the root list and settle on one.
 *
 * Separate from init because signing in or out changes which roots exist as
 * far as this browser is concerned, and everything downstream — the picker,
 * the scan list, the time slider — has to be rebuilt from the new list
 * rather than left pointing at a root that is no longer visible.
 */
async function boot() {
  try {
    const r = await api('roots');
    if (!r.roots.length) {
      document.querySelector('main').innerHTML = state.authed === false && !$('#signin').hidden
        ? '<div class="card"><div class="empty">Every tracked root is protected.<br><br>'
          + 'Sign in with the lock button above to view them.</div></div>'
        : '<div class="card"><div class="empty">No roots tracked yet.<br><br>'
          + 'Run <code>dutime scan /some/path</code> and reload.</div></div>';
      return;
    }
    $('#root').innerHTML = r.roots.map((x) =>
      `<option value="${x.root_id}">${escapeHtml(x.path.name)}</option>`).join('');
    // Stay where we are if that root is still visible; signing out of a
    // protected root has to land somewhere rather than erroring.
    const keep = r.roots.find((x) => x.root_id === state.root) || r.roots[0];
    if (keep.root_id !== state.root) state.path = null;
    state.root = keep.root_id;
    state.rootPath = keep.path.name;
    $('#root').value = state.root;
    await loadScans();
    state.scanIdx = Math.max(0, state.scans.length - 1);
    $('#timeSlider').value = state.scanIdx;
    updateTimeLabel();

    // Reflect anything the hash selected back into the controls.
    // A window from the hash that is not one of the presets would blank the
    // control; add it rather than showing an empty box.
    const wsel = $('#window');
    if (![...wsel.options].some((o) => o.value === state.window)) {
      wsel.add(new Option(`Last ${state.window.replace('-', '')}`, state.window));
    }
    wsel.value = state.window;
    $$('.seg [data-metric]').forEach((b) =>
      b.classList.toggle('on', b.dataset.metric === state.metric));
    $$('.seg [data-spark]').forEach((b) =>
      b.classList.toggle('on', b.dataset.spark === state.sparkScale));
    switchView(state.view);
  } catch (e) {
    toast(e.message);
  }
}

init();
