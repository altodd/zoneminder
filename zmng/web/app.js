// ZoneMinder NG web UI. No build step, no framework: ES modules + fetch + MSE.
//
// Views
//   #/live            tile wall of every camera (snapshots or go2rtc WebRTC)
//   #/camera/<id>[/t] one camera: full-res live, its timeline, scrub previews, Live button
//   #/review          multi-camera synchronized playback with date/group/camera filters
//   #/events          motion events, instant paging, thumbnails
//   #/status          health: recording/detector state per camera, storage headroom, issues
//   #/admin           cameras, users, storage/tiering

const $ = (sel, el = document) => el.querySelector(sel);
const h = (tag, attrs = {}, ...kids) => {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    // the attribute alone does not mute a video made by script (only the
    // parser copies it to .muted), and an unmuted video may not autoplay
    else if (k === 'muted') { el.muted = !!v; if (v) el.setAttribute('muted', ''); }
    else if (v !== null && v !== undefined && v !== false) el.setAttribute(k, v === true ? '' : v);
  }
  for (const k of kids.flat()) if (k !== null && k !== undefined && k !== false) el.append(k.nodeType ? k : document.createTextNode(k));
  return el;
};
const fmtTime = (ms) => new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
const fmtDate = (ms) => new Date(ms).toLocaleDateString([], { month: 'short', day: 'numeric' });
const fmtDT = (ms) => `${fmtDate(ms)} ${fmtTime(ms)}`;
const fmtClock = (ms) => new Date(ms).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
// replaceChildren() turns null into the text "null"
const fill = (el, ...kids) => el.replaceChildren(...kids.flat().filter((k) => k !== null && k !== undefined && k !== false));
const debounce = (fn, ms) => { let t = null; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); }; };
// object kinds as the event filters and timeline colours group them
const VEHICLES = ['car', 'truck', 'bus', 'motorcycle', 'bicycle'];
const ANIMALS = ['dog', 'cat'];
const KINDS = [['person', 'People', 'person'], ['vehicle', 'Vehicles', VEHICLES.join(',')], ['animal', 'Animals', ANIMALS.join(',')]];
const labelsOf = (ev) => [ev.kind, ...(ev.objects || []).map((o) => o.label)];
const kindClass = (ev) => { const l = labelsOf(ev); return l.includes('person') ? 'person' : l.some((x) => VEHICLES.includes(x)) ? 'vehicle' : l.some((x) => ANIMALS.includes(x)) ? 'animal' : 'motion'; };
const KIND_COLOR = { person: '#3ddc84', vehicle: '#c38bff', animal: '#4fd1c5', motion: 'rgba(79,156,249,.85)' };
const matchesKinds = (ev, kinds) => !kinds || labelsOf(ev).some((l) => kinds.includes(l));
const fmtDur = (ms) => { const s = Math.round(ms / 1000); return s >= 3600 ? `${Math.floor(s / 3600)}h ${Math.floor(s % 3600 / 60)}m` : s >= 60 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${s}s`; };
const toLocalInput = (ms) => { const d = new Date(ms); const p = (n) => String(n).padStart(2, '0'); return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`; };
const WINDOWS = [[900000, '15 min'], [3600000, '1 h'], [3 * 3600000, '3 h'], [12 * 3600000, '12 h'], [86400000, '24 h']];

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------
const api = {
  async req(method, url, body) {
    const r = await fetch(url, { method, headers: body ? { 'content-type': 'application/json' } : {}, body: body ? JSON.stringify(body) : undefined });
    if (r.status === 401) { showLogin(); throw new Error('unauthorized'); }
    const ct = r.headers.get('content-type') || '';
    const data = ct.includes('json') ? await r.json() : await r.text();
    if (!r.ok) throw new Error(data.error || data || r.statusText);
    return data;
  },
  get: (u) => api.req('GET', u),
  post: (u, b) => api.req('POST', u, b),
  patch: (u, b) => api.req('PATCH', u, b),
  del: (u) => api.req('DELETE', u),
};

const state = { me: null, cameras: [], cleanup: null };
// Federation: cameras from peers carry `peer`; their key is "peer:id" and their API lives under /api/peers/<peer>/.
const keyOf = (c) => (c.peer ? `${c.peer}:${c.id}` : String(c.id));
const camById = (key) => { const k = String(key); return state.cameras.find((c) => keyOf(c) === k) || state.cameras.find((c) => !c.peer && String(c.id) === k); };
const apiBase = (peer) => (peer ? `/api/peers/${peer}/api` : '/api');
const camApi = (c) => `${apiBase(c.peer)}/cameras/${c.id}`;
const evApi = (ev) => `${apiBase(ev.peer)}/events/${ev.id}`;
const evCam = (ev) => camById(ev.peer ? `${ev.peer}:${ev.camera_id}` : ev.camera_id);
const camLabel = (c) => (c.peer ? `${c.name} @${c.peer}` : c.name);
const groupsOf = (c) => (c.tags || '').split(',').map((s) => s.trim()).filter(Boolean);
const allGroups = () => [...new Set(state.cameras.flatMap(groupsOf))].sort();

// ---------------------------------------------------------------------------
// codec support probe
// ---------------------------------------------------------------------------
const support = {
  mse: !!window.MediaSource,
  hevcMse: !!window.MediaSource && ['video/mp4; codecs="hvc1.1.6.L153.B0"', 'video/mp4; codecs="hev1.1.6.L153.B0"'].some((c) => MediaSource.isTypeSupported(c)),
  h264Mse: !!window.MediaSource && MediaSource.isTypeSupported('video/mp4; codecs="avc1.640028"'),
  nativeHls: (() => { const v = document.createElement('video'); return v.canPlayType('application/vnd.apple.mpegurl') !== ''; })(),
};
const canPlayMain = (c) => support.mse && (c.status?.codec?.startsWith('avc') ? support.h264Mse : support.hevcMse);
// full resolution through the server-side H.264 transcode (HEVC camera, no HEVC decoder here, [transcode] configured)
const canTranscode = (c) => support.mse && support.h264Mse && !canPlayMain(c) && !!state.me?.transcode;
// query suffix that picks the best full-resolution main-stream representation for this browser
const mainQuery = (c) => (canPlayMain(c) ? '' : canTranscode(c) ? '&codec=h264' : null);

// ---------------------------------------------------------------------------
// MSE player for our fMP4 streams (live or range). Absolute tfdt: we shift
// with timestampOffset so the media timeline starts at 0.
// ---------------------------------------------------------------------------
class MsePlayer {
  constructor(video) { this.video = video; this.abort = null; this.ms = null; this.sb = null; this.queue = []; this.mime = null; this.baseSec = null; this.ended = false; this.onLiveEnded = null; }
  async play(url, { live = false, startMs = null } = {}) {
    this.stop();
    const ac = new AbortController(); this.abort = ac;
    const res = await fetch(url, { signal: ac.signal });
    if (!res.ok) throw new Error(`video: ${res.status} ${await res.text()}`);
    this.mime = res.headers.get('content-type') || 'video/mp4';
    if (!MediaSource.isTypeSupported(this.mime)) throw new Error(`browser cannot decode ${this.mime}`);
    const first = Number(res.headers.get('X-First-Dts-Ms') || startMs || 0);
    this.baseSec = first / 1000;
    const ms = new MediaSource(); this.ms = ms;
    this.video.src = URL.createObjectURL(ms);
    await new Promise((r) => ms.addEventListener('sourceopen', r, { once: true }));
    if (this.ms !== ms) return; // stopped meanwhile
    const sb = ms.addSourceBuffer(this.mime); this.sb = sb;
    sb.mode = 'segments';
    sb.timestampOffset = -this.baseSec;
    sb.addEventListener('updateend', () => this.pump());
    const reader = res.body.getReader();
    (async () => {
      try {
        for (;;) {
          const { value, done } = await reader.read();
          if (done) { if (live && this.onLiveEnded && this.ms === ms) this.onLiveEnded(); this.ended = true; this.pump(); break; }
          this.queue.push(value); this.pump();
          if (live) this.trimLive();
        }
      } catch (e) { if (e.name !== 'AbortError') console.warn('stream', e); }
    })();
    this.video.play().catch(() => {});
  }
  pump() {
    const sb = this.sb; if (!sb || sb.updating || !this.ms || this.ms.readyState !== 'open') return;
    if (this.queue.length) {
      const chunk = this.queue[0];
      try { sb.appendBuffer(chunk); this.queue.shift(); }
      catch (e) {
        if (e.name === 'QuotaExceededError') {
          const b = sb.buffered; const t = this.video.currentTime;
          if (b.length && t - b.start(0) > 25) { try { sb.remove(b.start(0), t - 20); return; } catch {} }
          setTimeout(() => this.pump(), 200);
        } else { console.warn('append', e); this.queue.shift(); }
      }
    } else if (this.ended) { try { this.ms.endOfStream(); } catch {} this.ended = false; }
  }
  trimLive() {
    const v = this.video, sb = this.sb; if (!sb || sb.updating) return;
    const b = sb.buffered; if (!b.length) return;
    const end = b.end(b.length - 1);
    if (end - v.currentTime > 4) v.currentTime = end - 0.5;
    if (b.start(0) < end - 30) { try { sb.remove(0, end - 20); } catch {} }
  }
  wallMs() { return this.baseSec === null ? null : (this.baseSec + this.video.currentTime) * 1000; }
  stop() {
    if (this.abort) this.abort.abort(); this.abort = null;
    this.queue = []; this.ended = false;
    if (this.ms && this.ms.readyState === 'open') { try { this.ms.endOfStream(); } catch {} }
    this.ms = null; this.sb = null; this.baseSec = null;
    if (this.video.src) { URL.revokeObjectURL(this.video.src); this.video.removeAttribute('src'); this.video.load(); }
  }
}

// ---------------------------------------------------------------------------
// Timeline widget: coverage + motion + events per camera row, hover/drag
// scrubbing with snapshot previews (substream keyframes, cached), wheel zoom.
// ---------------------------------------------------------------------------
// `onScrub(t)` is called while the cursor is dragged (once per frame) so the
// page can show that moment at once; `onSeek(t)` when it is let go.
// `highlight(ev)` decides which events are drawn at full strength.
function makeTimeline({ rows, range, onSeek, onScrub, onScrubStart, onRangeChange, fixedRange = false, tall = false, highlight = () => true }) {
  const canvas = h('canvas');
  const cursor = h('div', { class: 'cursor' });
  const hover = h('div', { class: 'hover' });
  const tip = h('div', { class: 'tip' });
  const preview = h('div', { class: 'preview', hidden: true }, h('img'), h('div', { class: 'ptime' }));
  const el = h('div', { class: 'timeline' + (tall ? ' tall' : '') }, canvas, hover, tip, cursor, preview);
  let data = new Map(); // camId -> timeline json
  let playhead = range.start;
  let dragging = false;
  const cache = new Map();
  let previewTimer = null;
  const xOf = (ms) => (ms - range.start) / (range.end - range.start) * canvas.clientWidth;
  const msAt = (ev) => { const r = el.getBoundingClientRect(); return range.start + Math.min(1, Math.max(0, (ev.clientX - r.left) / r.width)) * (range.end - range.start); };
  const rowAt = (ev) => { const r = el.getBoundingClientRect(); const y = ev.clientY - r.top - 14; const rowH = (canvas.clientHeight - 14) / Math.max(1, rows.length); return rows[Math.max(0, Math.min(rows.length - 1, Math.floor(y / rowH)))]; };

  function draw() {
    const dpr = window.devicePixelRatio || 1; const W = canvas.clientWidth, H = canvas.clientHeight;
    if (!W) return;
    if (canvas.width !== W * dpr) { canvas.width = W * dpr; canvas.height = H * dpr; }
    const ctx = canvas.getContext('2d'); ctx.setTransform(dpr, 0, 0, dpr, 0, 0); ctx.clearRect(0, 0, W, H);
    const rowH = Math.max(6, (H - 14) / Math.max(1, rows.length));
    rows.forEach((row, i) => {
      const tl = data.get(row.key); const y = 14 + i * rowH;
      ctx.fillStyle = '#2b3442'; if (tl) for (const [a, b] of tl.coverage) ctx.fillRect(xOf(a), y, Math.max(1, xOf(b) - xOf(a)), rowH - 1);
      if (tl) {
        const bw = Math.max(1, W / ((range.end - range.start) / tl.bucket_ms));
        for (let k = 0; k < tl.motion.length; k++) { const m = tl.motion[k]; if (!m) continue; const hh = rows.length === 1 ? (rowH - 1) * m / 255 : rowH - 1; ctx.fillStyle = m > 120 ? '#ff5a5f' : '#f9b84f'; ctx.fillRect(xOf(range.start + k * tl.bucket_ms), y + (rowH - 1 - hh), bw, hh); }
        for (const ev of tl.events) { const k = kindClass(ev); ctx.globalAlpha = highlight(ev) ? 1 : 0.25; ctx.fillStyle = KIND_COLOR[k]; ctx.fillRect(xOf(ev.start), y, Math.max(2, xOf(ev.end || ev.start) - xOf(ev.start)), k === 'motion' ? 3 : 5); }
        ctx.globalAlpha = 1;
      }
      if (rows.length > 1) { ctx.fillStyle = '#c8ced8'; ctx.font = '10px system-ui'; ctx.fillText(row.name, 3, y + Math.min(rowH - 2, 11)); }
    });
    ctx.fillStyle = '#8b93a1'; ctx.font = '11px system-ui';
    // ticks at least ~80 px apart, on local-time boundaries, labelled "5:15 PM" (a date at midnight)
    const span = range.end - range.start;
    const step = [60000, 300000, 900000, 1800000, 3600000, 3 * 3600000, 6 * 3600000, 12 * 3600000, 86400000].find((st) => W * st / span >= 80) || 86400000;
    const tz = new Date(range.start).getTimezoneOffset() * 60000;
    for (let t = Math.ceil((range.start - tz) / step) * step + tz; t < range.end; t += step) {
      const x = xOf(t); const d = new Date(t);
      ctx.fillRect(x, 0, 1, 10);
      ctx.fillText(step >= 86400000 || (d.getHours() === 0 && d.getMinutes() === 0) ? fmtDate(t) : fmtClock(t), x + 3, 10);
    }
    cursor.style.left = `${xOf(playhead)}px`;
  }
  async function load() {
    const res = await Promise.all(rows.map((r) => api.get(`${r.api}/timeline?start=${Math.round(range.start)}&end=${Math.round(range.end)}`).catch(() => null)));
    data = new Map(rows.map((r, i) => [r.key, res[i]]));
    draw();
  }
  function showPreview(ev) {
    const t = msAt(ev); const row = rowAt(ev);
    hover.style.left = `${xOf(t)}px`; tip.style.left = `${xOf(t)}px`; tip.textContent = fmtDT(t);
    preview.hidden = false; preview.style.left = `${Math.min(canvas.clientWidth - 170, Math.max(0, xOf(t) - 80))}px`;
    $('.ptime', preview).textContent = (rows.length > 1 ? row.name + ' · ' : '') + fmtTime(t);
    if (t > Date.now() - 3000) return;
    const key = `${row.key}:${Math.round(t / 2000)}`;
    clearTimeout(previewTimer);
    if (cache.has(key)) { $('img', preview).src = cache.get(key); return; }
    previewTimer = setTimeout(async () => {
      try {
        // preview tiles (no decode) first; keyframe decode of the substream as the fallback
        let r = await fetch(`${row.api}/preview.jpg?t=${Math.round(t)}`);
        if (r.status === 404) r = await fetch(`${row.api}/frame.jpg?t=${Math.round(t)}&width=320&stream=sub`);
        if (!r.ok) return;
        const url = URL.createObjectURL(await r.blob());
        cache.set(key, url); if (cache.size > 400) { const k = cache.keys().next().value; URL.revokeObjectURL(cache.get(k)); cache.delete(k); }
        if (!preview.hidden) $('img', preview).src = url;
      } catch {}
    }, dragging ? 60 : 120);
  }
  // while dragging, the page shows the moment itself (tiles follow the cursor); the popup is for hovering
  let scrubT = null, scrubTimer = 0;
  const scrub = (t) => { if (!onScrub) return; scrubT = t; if (!scrubTimer) scrubTimer = setTimeout(() => { scrubTimer = 0; onScrub(scrubT); }, 30); };
  el.addEventListener('pointermove', (ev) => {
    if (dragging) { playhead = msAt(ev); cursor.style.left = `${xOf(playhead)}px`; tip.style.left = cursor.style.left; tip.textContent = fmtDT(playhead); if (onScrub) { preview.hidden = true; scrub(playhead); return; } }
    showPreview(ev);
  });
  el.addEventListener('pointerleave', () => { if (!dragging) { preview.hidden = true; tip.textContent = ''; } });
  el.addEventListener('pointerdown', (ev) => { dragging = true; el.setPointerCapture(ev.pointerId); playhead = msAt(ev); cursor.style.left = `${xOf(playhead)}px`; onScrubStart?.(); if (onScrub) scrub(playhead); else showPreview(ev); });
  const release = (ev) => { if (!dragging) return; dragging = false; preview.hidden = true; tip.textContent = ''; onSeek(msAt(ev), rowAt(ev)); };
  el.addEventListener('pointerup', release);
  el.addEventListener('pointercancel', release);
  el.addEventListener('wheel', (ev) => {
    if (fixedRange) return; ev.preventDefault();
    const t = msAt(ev); const f = ev.deltaY > 0 ? 1.25 : 0.8; let span = Math.min(7 * 86400000, Math.max(300000, (range.end - range.start) * f));
    const frac = (t - range.start) / (range.end - range.start); range.start = t - frac * span; range.end = range.start + span;
    if (range.end > Date.now()) { range.end = Date.now(); range.start = range.end - span; }
    onRangeChange?.(); load();
  }, { passive: false });
  window.addEventListener('resize', draw);
  return {
    el, load, draw,
    events() { return [...data.values()].flatMap((tl) => tl?.events || []).sort((a, b) => a.start - b.start); },
    // playback must not pull the cursor away from a drag in progress
    setPlayhead(ms) { if (dragging) return; playhead = ms; cursor.style.left = `${xOf(ms)}px`; },
    get dragging() { return dragging; },
    setRange(s, e) { range.start = s; range.end = e; return load(); },
    get range() { return range; },
    destroy() { window.removeEventListener('resize', draw); for (const u of cache.values()) URL.revokeObjectURL(u); cache.clear(); },
  };
}

// ---------------------------------------------------------------------------
// Scrubbing: show a moment on a player while the timeline is dragged. Inside
// the chunk already loaded that is a plain seek (a real frame, instant);
// outside it a preview tile (no decode), sharpened to the substream keyframe
// once the cursor rests.
// ---------------------------------------------------------------------------
function makeScrubber() {
  const cache = new Map();
  let rest = null;
  const fetchImg = async (url) => {
    if (cache.has(url)) return cache.get(url);
    const r = await fetch(url); if (!r.ok) return null;
    const u = URL.createObjectURL(await r.blob());
    cache.set(url, u); if (cache.size > 300) { const k = cache.keys().next().value; URL.revokeObjectURL(cache.get(k)); cache.delete(k); }
    return u;
  };
  const overlay = (p) => { if (!p.scrubImg) { p.scrubImg = h('img', { class: 'scrubimg', alt: '' }); p.video.after(p.scrubImg); } return p.scrubImg; };
  const hide = (p) => { if (p.scrubImg) p.scrubImg.hidden = true; };
  function inBuffer(p, t) {
    if (p.mse.baseSec === null) return null;
    const off = t / 1000 - p.mse.baseSec; const b = p.video.buffered;
    for (let i = 0; i < b.length; i++) if (off >= b.start(i) && off <= b.end(i) - 0.05) return off;
    return null;
  }
  return {
    show(players, t) {
      clearTimeout(rest);
      for (const p of players) {
        const off = inBuffer(p, t);
        if (off !== null) { p.video.currentTime = off; hide(p); continue; }
        const img = overlay(p); img.hidden = false; img.dataset.t = t;
        const bucket = Math.round(t / 2000) * 2000;
        fetchImg(`${camApi(p.cam)}/preview.jpg?t=${bucket}`).then((u) => { if (u && img.dataset.t == t && !img.dataset.sharp) img.src = u; }).catch(() => {});
        img.dataset.sharp = '';
      }
      rest = setTimeout(() => players.forEach((p) => {
        const img = p.scrubImg; if (!img || img.hidden) return;
        fetchImg(`${camApi(p.cam)}/frame.jpg?t=${Math.round(t)}&width=640&stream=sub`).then((u) => { if (u && img.dataset.t == t) { img.src = u; img.dataset.sharp = '1'; } }).catch(() => {});
      }), 250);
    },
    hide(p) { hide(p); },
    destroy() { clearTimeout(rest); for (const u of cache.values()) URL.revokeObjectURL(u); cache.clear(); },
  };
}

// Size a tile grid so its tiles fill the space left in the window: two
// cameras get two big tiles, twenty get a wall (scrolling only below
// `min` px per tile). `below()` is the height of what follows the grid.
function fitGrid(grid, count, { aspect = 16 / 9, below = () => 0, min = 260 } = {}) {
  const gap = 8;
  const fit = () => {
    const n = Math.max(1, count());
    const W = grid.clientWidth; if (!W) return;
    const top = grid.getBoundingClientRect().top + window.scrollY;
    const H = Math.max(220, window.innerHeight - top - below() - 12);
    let best = { w: 0, cols: 1 };
    for (let cols = 1; cols <= n; cols++) {
      const rows = Math.ceil(n / cols);
      const w = Math.min((W - (cols - 1) * gap) / cols, ((H - (rows - 1) * gap) / rows) * aspect);
      if (w > best.w + 0.5) best = { w, cols };
    }
    let { w, cols } = best;
    if (w < min) { cols = Math.max(1, Math.floor((W + gap) / (min + gap))); w = (W - (cols - 1) * gap) / cols; }
    grid.style.gridTemplateColumns = `repeat(${cols}, ${Math.floor(w)}px)`;
  };
  const ro = new ResizeObserver(() => fit());
  ro.observe(grid);
  window.addEventListener('resize', fit);
  fit(); // reads layout synchronously; again once the page has settled
  requestAnimationFrame(fit);
  setTimeout(fit, 300);
  return { fit, destroy() { ro.disconnect(); window.removeEventListener('resize', fit); } };
}

// ---------------------------------------------------------------------------
// Live: tile wall. Click a tile for the camera page.
// ---------------------------------------------------------------------------
async function viewLive(main) {
  const cams = state.cameras;
  const mode = localStorage.getItem('liveMode') || 'snapshot';
  const groupSel = h('select', { onchange: (e) => { localStorage.setItem('liveGroup', e.target.value); route(); } },
    h('option', { value: '' }, 'All groups'), ...allGroups().map((g) => h('option', { value: g, selected: g === localStorage.getItem('liveGroup') }, g)));
  const group = localStorage.getItem('liveGroup') || '';
  const shown = group ? cams.filter((c) => groupsOf(c).includes(group)) : cams;
  const toolbar = h('div', { class: 'toolbar' },
    h('span', { class: 'muted' }, `${shown.length} cameras`), groupSel, h('span', { class: 'grow' }),
    h('label', { class: 'row' }, 'Tiles ', h('select', { onchange: (e) => { localStorage.setItem('liveMode', e.target.value); route(); } },
      h('option', { value: 'snapshot', selected: mode === 'snapshot' }, 'Snapshots every 2 s'),
      h('option', { value: 'sub', selected: mode === 'sub' }, 'Substream video (MSE)'),
      h('option', { value: 'webrtc', selected: mode === 'webrtc', disabled: !state.me.go2rtc_url }, 'go2rtc WebRTC (low latency)'))));
  const grid = h('div', { class: 'grid fit' });
  main.replaceChildren(toolbar, grid);
  const sizer = fitGrid(grid, () => shown.length, { aspect: aspectOf(shown) });
  const players = [];
  for (const c of shown) {
    const tile = h('div', { class: 'tile', onclick: () => { location.hash = `#/camera/${keyOf(c)}`; } },
      h('div', { class: 'label' }, camLabel(c)), h('div', { class: 'stat' }, statusText(c)), h('div', { class: 'motion' }));
    grid.append(tile);
    if (mode === 'sub' && support.h264Mse && c.sub_status?.connected) {
      const v = h('video', { muted: true, playsinline: true, autoplay: true }); tile.prepend(v);
      const p = new MsePlayer(v); const start = () => p.play(`${camApi(c)}/live.mp4?stream=sub`, { live: true }).catch(() => fallbackSnapshot(tile, c));
      p.onLiveEnded = () => setTimeout(start, 2000); start(); players.push(p);
    } else if (mode === 'webrtc' && state.me.go2rtc_url && !c.peer) {
      tile.prepend(h('iframe', { src: `${state.me.go2rtc_url}/stream.html?src=${encodeURIComponent(c.id + '_CameraDirectSecondary')}&mode=webrtc`, style: 'width:100%;height:100%;border:0;background:#000;pointer-events:none', allow: 'autoplay' }));
    } else fallbackSnapshot(tile, c);
  }
  const poll = setInterval(async () => {
    try {
      const peers = [...new Set(shown.map((c) => c.peer || ''))];
      const all = await Promise.all(peers.map((pr) => api.get(`${apiBase(pr)}/stats`).then((st) => st.cameras.map((s) => ({ ...s, peer: pr || undefined }))).catch(() => [])));
      for (const s of all.flat()) {
        const i = shown.findIndex((c) => c.id === s.id && (c.peer || '') === (s.peer || '')); if (i < 0) continue;
        shown[i].status = s.status; shown[i].detect = s.detect;
        const tile = grid.children[i]; if (!tile) continue;
        $('.stat', tile).textContent = statusText(shown[i]);
        $('.motion', tile).style.transform = `scaleX(${(s.detect?.score || 0) / 255})`;
      }
    } catch {}
  }, 3000);
  state.cleanup = () => { clearInterval(poll); sizer.destroy(); players.forEach((p) => p.stop()); grid.querySelectorAll('img[data-snap]').forEach((i) => clearInterval(i._t)); };
}
// the cameras' picture shape (most are 16:9; a 4:3 or 32:9 camera changes it)
const aspectOf = (cams) => { const c = cams.find((x) => x.status?.width && x.status?.height); return c ? c.status.width / c.status.height : 16 / 9; };
function statusText(c) {
  if (!c.status?.connected) return 'offline';
  const s = c.status;
  return `${s.width}×${s.height} ${s.fps.toFixed(0)} fps ${(s.kbps / 1000).toFixed(1)} Mb/s${c.detect?.in_event ? ' • MOTION' : ''}`;
}
function fallbackSnapshot(tile, c, every = 2000, width = 640) {
  tile.querySelector('video')?.remove();
  const img = h('img', { 'data-snap': '1', alt: c.name });
  let busy = false;
  const load = () => {
    if (busy) return; busy = true;
    const next = new Image();
    next.onload = () => { img.src = next.src; busy = false; };
    next.onerror = () => { busy = false; };
    next.src = `${camApi(c)}/snapshot.jpg?width=${width}&t=${Date.now()}`;
  };
  load(); img._t = setInterval(load, every);
  tile.prepend(img);
}

// ---------------------------------------------------------------------------
// Camera page: live (full-res MSE) + this camera's timeline + scrub previews
// ---------------------------------------------------------------------------
async function viewCamera(main, camId, atMs) {
  const cam = camById(camId);
  if (!cam) { main.replaceChildren(h('p', { class: 'err' }, 'No such camera.')); return; }
  const now = Date.now();
  let windowMs = Number(localStorage.getItem('camWindow')) || 3 * 3600 * 1000;
  const range = { start: now - windowMs, end: now };
  let mode = atMs ? 'playback' : 'live';
  let playhead = atMs || now;
  const CHUNK = 2 * 60 * 1000;

  const video = h('video', { controls: true, playsinline: true, muted: true });
  const overlay = h('div', { class: 'overlay' }, '');
  const liveBadge = h('div', { class: 'livebadge' }, '● LIVE');
  const player = h('div', { class: 'player' }, video, overlay, liveBadge);
  const liveBtn = h('button', { onclick: () => goLive() }, '● Live');
  const winSel = h('select', { onchange: (e) => { windowMs = Number(e.target.value); localStorage.setItem('camWindow', windowMs); range.start = range.end - windowMs; tl.load(); } },
    ...WINDOWS.map(([v, l]) => h('option', { value: v, selected: v === windowMs }, l)));
  const timeLabel = h('span', { class: 'time' });
  const head = h('div', { class: 'toolbar' },
    h('a', { href: '#/live', class: 'muted' }, '◀ all cameras'), h('h2', { style: 'margin:0' }, camLabel(cam)),
    h('span', { class: 'pill ' + (cam.status?.connected ? 'ok' : 'bad') }, statusText(cam)), h('span', { class: 'grow' }), timeLabel, liveBtn);
  const scrubber = makeScrubber();
  const pl = { cam, video, mse: null };
  let wasPaused = false;
  const tl = makeTimeline({ rows: [{ id: cam.id, key: keyOf(cam), name: cam.name, api: camApi(cam) }], range,
    onScrubStart: () => { wasPaused = video.paused; video.pause(); },
    onScrub: (t) => { timeLabel.textContent = fmtDT(t); scrubber.show([pl], t); },
    onSeek: (t) => play(t, { resume: !wasPaused || mode === 'live' }), onRangeChange: () => {} });
  // playback speed for recorded video (live always plays in real time)
  const SPEEDS = [0.5, 1, 2, 4, 8, 16];
  const speed = () => Number(speedSel.value) || 1;
  const applySpeed = () => { const r = mode === 'playback' ? speed() : 1; video.defaultPlaybackRate = r; video.playbackRate = r; };
  const speedSel = h('select', { title: 'playback speed (recorded video)', onchange: () => { localStorage.setItem('camSpeed', speedSel.value); applySpeed(); } },
    ...SPEEDS.map((r) => h('option', { value: r, selected: r === (Number(localStorage.getItem('camSpeed')) || 1) }, `${r}×`)));
  const controls = h('div', { class: 'controls' }, winSel, speedSel,
    h('button', { class: 'ghost', onclick: () => shift(-windowMs / 2) }, '◀'), h('button', { class: 'ghost', onclick: () => shift(windowMs / 2) }, '▶'),
    h('button', { class: 'ghost', onclick: () => play(playhead - 10000) }, '−10 s'), h('button', { class: 'ghost', onclick: () => play(playhead + 10000) }, '+10 s'),
    h('span', { class: 'grow' }),
    h('button', { class: 'ghost', onclick: () => window.open(`${camApi(cam)}/video.mp4?start=${Math.round(playhead - 30000)}&end=${Math.round(playhead + 30000)}`) }, 'Export ±30 s'),
    h('button', { class: 'ghost', onclick: () => window.open(`${camApi(cam)}/frame.jpg?t=${Math.round(playhead)}&width=3840`) }, 'Full-res frame'),
    h('button', { class: 'ghost', onclick: () => { location.hash = '#/events'; localStorage.setItem('evCam', keyOf(cam)); } }, 'Events'),
    h('span', { class: 'muted small' }, 'drag the timeline to scrub · wheel to zoom · space pause · ←/→ 10 s · [ ] speed'));
  main.replaceChildren(head, player, tl.el, controls);
  if (cam.ptz && state.me.user.role === 'admin' && !cam.peer) player.append(ptzPad(cam));

  const mse = new MsePlayer(video);
  pl.mse = mse;
  let playRange = null;
  function shift(d) { range.end = Math.min(Date.now(), range.end + d); range.start = range.end - windowMs; tl.load(); }
  async function goLive() {
    mode = 'live'; liveBadge.hidden = false; liveBtn.disabled = true; overlay.textContent = 'connecting…'; speedSel.disabled = true; applySpeed(); scrubber.hide(pl);
    const mq = mainQuery(cam);
    const url = mq !== null ? `${camApi(cam)}/live.mp4?${mq.slice(1)}` : (support.h264Mse && cam.sub_status ? `${camApi(cam)}/live.mp4?stream=sub` : null);
    if (!url) { overlay.textContent = 'browser cannot decode this stream — snapshots'; snapshots(); return; }
    mse.onLiveEnded = () => { if (mode === 'live') setTimeout(goLive, 2000); };
    try { await mse.play(url, { live: true }); overlay.textContent = url.includes('stream=sub') ? 'live (substream)' : url.includes('codec=h264') ? 'live (transcoded)' : 'live'; }
    catch (e) { overlay.textContent = `${e.message} — snapshots`; snapshots(); }
  }
  let snapTimer = null;
  function snapshots() { const img = h('img', { style: 'width:100%;display:block' }); video.replaceWith(img); const load = () => { img.src = `${camApi(cam)}/snapshot.jpg?width=1920&t=${Date.now()}`; }; load(); snapTimer = setInterval(load, 1000); }
  async function play(ms, { resume = true } = {}) {
    mode = 'playback'; liveBadge.hidden = true; liveBtn.disabled = false; mse.onLiveEnded = null; speedSel.disabled = false;
    playhead = Math.min(ms, Date.now() - 3000); tl.setPlayhead(playhead);
    const s = Math.max(playhead - 1500, 0), e = Math.min(Date.now(), s + CHUNK); playRange = [s, e];
    overlay.textContent = 'loading…';
    try {
      const mq = mainQuery(cam);
      const url = mq !== null ? `${camApi(cam)}/video.mp4?start=${Math.round(s)}&end=${Math.round(e)}${mq}` : `${camApi(cam)}/video.mp4?stream=sub&start=${Math.round(s)}&end=${Math.round(e)}`;
      if (support.mse) await mse.play(url, { startMs: s });
      else if (support.nativeHls) { video.src = `${camApi(cam)}/playlist.m3u8?start=${Math.round(s)}&end=${Math.round(e)}`; video.play(); }
      applySpeed();
      if (!resume) video.pause();
      video.addEventListener('loadeddata', () => scrubber.hide(pl), { once: true });
    } catch (err) { overlay.textContent = err.message; scrubber.hide(pl); }
  }
  video.addEventListener('timeupdate', () => {
    if (mode !== 'playback') return;
    const w = mse.wallMs(); if (w === null) return;
    playhead = w; overlay.textContent = fmtDT(w); timeLabel.textContent = fmtDT(w); tl.setPlayhead(w);
    if (w > range.end - 5000 && range.end < Date.now()) shift(windowMs / 4);
  });
  video.addEventListener('ended', () => { if (mode === 'playback' && playRange && playRange[1] < Date.now() - 5000) play(playRange[1]); });
  const keys = (e) => {
    if (['INPUT', 'SELECT', 'TEXTAREA'].includes(e.target.tagName)) return;
    if (e.key === ' ') { e.preventDefault(); video.paused ? video.play() : video.pause(); }
    if (e.key === 'ArrowRight') play(playhead + (e.shiftKey ? 60000 : 10000));
    if (e.key === 'ArrowLeft') play(playhead - (e.shiftKey ? 60000 : 10000));
    if (e.key === ']' || e.key === '[') { const i = SPEEDS.indexOf(speed()) + (e.key === ']' ? 1 : -1); if (i >= 0 && i < SPEEDS.length) { speedSel.value = SPEEDS[i]; speedSel.dispatchEvent(new Event('change')); } }
  };
  document.addEventListener('keydown', keys);
  await tl.load();
  if (mode === 'live') goLive(); else play(playhead);
  const tick = setInterval(() => {
    if (mode === 'live') { playhead = Date.now(); tl.setPlayhead(playhead); timeLabel.textContent = fmtDT(playhead); }
    if (range.end >= Date.now() - 20000) { range.end = Date.now(); range.start = range.end - windowMs; tl.load(); }
  }, 15000);
  state.cleanup = () => { clearInterval(tick); clearInterval(snapTimer); mse.stop(); tl.destroy(); scrubber.destroy(); document.removeEventListener('keydown', keys); };
}

// PTZ pad: hold an arrow to move, release to stop; zoom; presets; home.
function ptzPad(cam) {
  const send = (body) => api.post(`/api/cameras/${cam.id}/ptz`, body).catch((e) => toast(`PTZ: ${e.message}`, { kind: 'bad' }));
  const hold = (pan, tilt, zoom) => ({
    onpointerdown: (e) => { e.preventDefault(); e.target.setPointerCapture(e.pointerId); send({ action: 'move', pan, tilt, zoom, seconds: 10 }); },
    onpointerup: () => send({ action: 'stop' }), onpointercancel: () => send({ action: 'stop' }),
  });
  const btn = (label, pan, tilt, zoom, cls = '') => h('button', { class: 'ghost small ' + cls, ...hold(pan, tilt, zoom) }, label);
  const presets = h('select', { onchange: (e) => { if (e.target.value) send({ action: 'preset', preset: e.target.value }); e.target.value = ''; } }, h('option', { value: '' }, 'Preset…'));
  api.get(`/api/cameras/${cam.id}/ptz/presets`).then((ps) => ps.forEach((p) => presets.append(h('option', { value: p.token }, p.name)))).catch(() => {});
  return h('div', { class: 'ptz' },
    h('div', { class: 'pad' }, btn('↖', -1, 1, 0), btn('↑', 0, 1, 0), btn('↗', 1, 1, 0), btn('←', -1, 0, 0), h('button', { class: 'ghost small', onclick: () => send({ action: 'home' }) }, '⌂'), btn('→', 1, 0, 0), btn('↙', -1, -1, 0), btn('↓', 0, -1, 0), btn('↘', 1, -1, 0)),
    h('div', { class: 'row' }, btn('－', 0, 0, -1), btn('＋', 0, 0, 1), presets));
}

// ---------------------------------------------------------------------------
// Review: filters (date range, groups, cameras, what to show) + synchronized
// multi-camera playback of the substreams on a shared timeline.
// Picking cameras, groups or a saved view updates the view at once (keeping
// the playhead); dates, "Show" and "Length" wait for Apply so the page does
// not reload while they are being edited.
// ---------------------------------------------------------------------------

// Named time ranges, relative to now (local time).
function presetRange(key, now = Date.now()) {
  const at = (d, h, m = 0) => { const x = new Date(d); x.setHours(h, m, 0, 0); return x.getTime(); };
  const day = 86400000;
  switch (key) {
    case 'night': { let end = at(now, 6); if (end > now) end -= day; return [end - 8 * 3600000, end]; } // 10 PM – 6 AM
    case 'morning': { let s0 = at(now, 6); if (s0 > now) s0 -= day; return [s0, Math.min(now, s0 + 6 * 3600000)]; }
    case 'today': return [at(now, 0), now];
    case 'yesterday': { const s0 = at(now, 0) - day; return [s0, s0 + day]; }
    case 'sunday': { // the latest Sunday 7 AM – 1 PM that has started
      const d = new Date(now); d.setDate(d.getDate() - d.getDay());
      let s0 = at(d, 7); if (s0 > now) s0 -= 7 * day;
      return [s0, Math.min(now, s0 + 6 * 3600000)];
    }
    default: { const m = Number(key); return m ? [now - m, now] : null; }
  }
}
const RANGES = [['900000', 'Last 15 min'], ['3600000', 'Last hour'], ['21600000', 'Last 6 h'], ['86400000', 'Last 24 h'], ['604800000', 'Last 7 days'],
  ['night', 'Last night (10 PM–6 AM)'], ['morning', 'This morning (6 AM–noon)'], ['today', 'Today'], ['yesterday', 'Yesterday'], ['sunday', 'Sunday services (7 AM–1 PM)']];
const LENGTHS = [[0, 'Any length'], [3, '3 s+ of motion'], [10, '10 s+ of motion'], [30, '30 s+ of motion']];

// Which events count for a "Show" choice and minimum motion length.
function eventFilter(show, minMotion) {
  const kinds = KINDS.find(([k]) => k === show)?.[2].split(',') || null;
  return (ev) => (!kinds || matchesKinds(ev, kinds)) && (show !== 'starred' || ev.archived) && (!minMotion || ev.motion_secs == null || ev.motion_secs >= minMotion);
}

async function viewReview(main, atMs) {
  const cams = state.cameras.filter((c) => c.sub_status || c.status?.connected);
  const now = Date.now();
  const saved = JSON.parse(localStorage.getItem('reviewFilter') || '{}');
  const f = { from: saved.from || now - 3600000, to: saved.to || now, groups: saved.groups || [], cameras: saved.cameras || [], show: saved.show ?? (saved.motionOnly ? 'motion' : ''), minMotion: saved.minMotion || 0 };
  if (f.to > now) f.to = now;
  if (atMs) { f.from = atMs - 1800000; f.to = Math.min(now, atMs + 1800000); }
  const SHOW = [['', 'All selected cameras'], ['motion', 'Cameras with motion'], ...(state.me.objects ? KINDS.map(([k, l]) => [k, `Cameras with ${l.toLowerCase()}`]) : []), ['starred', 'Cameras with starred events']];
  if (!SHOW.some(([v]) => v === f.show)) f.show = '';
  const applied = { from: f.from, to: f.to, show: f.show, minMotion: f.minMotion };

  const applyBtn = h('button', { onclick: () => apply() }, 'Apply');
  const dirty = () => applyBtn.classList.add('pending');
  const fromIn = h('input', { type: 'datetime-local', value: toLocalInput(f.from), oninput: dirty });
  const toIn = h('input', { type: 'datetime-local', value: toLocalInput(f.to), oninput: dirty });
  const quick = h('select', { onchange: (e) => { const r = presetRange(e.target.value); e.target.value = ''; if (!r) return; fromIn.value = toLocalInput(r[0]); toIn.value = toLocalInput(r[1]); dirty(); } },
    h('option', { value: '' }, 'Pick a range…'), ...RANGES.map(([v, l]) => h('option', { value: v }, l)));
  const showSel = h('select', { onchange: dirty }, ...SHOW.map(([v, l]) => h('option', { value: v, selected: v === f.show }, l)));
  const lenSel = h('select', { onchange: dirty, title: 'hide short blips (headlights, leaves): seconds of actual motion' }, ...LENGTHS.map(([v, l]) => h('option', { value: v, selected: v === f.minMotion }, l)));
  const soon = debounce(() => apply({ camerasOnly: true }), 350);
  const groupBoxes = allGroups().map((g) => h('label', { class: 'chip' }, h('input', { type: 'checkbox', value: g, checked: f.groups.includes(g), onchange: () => { syncCamsFromGroups(); soon(); } }), g));
  const camBoxes = cams.map((c) => h('label', { class: 'chip' }, h('input', { type: 'checkbox', value: keyOf(c), checked: f.cameras.length ? f.cameras.includes(keyOf(c)) : true, onchange: soon }), camLabel(c)));
  const setAll = (on) => { camBoxes.forEach((b) => { $('input', b).checked = on; }); groupBoxes.forEach((b) => { $('input', b).checked = false; }); soon(); };
  function syncCamsFromGroups() {
    const gs = groupBoxes.filter((b) => $('input', b).checked).map((b) => $('input', b).value);
    // no group ticked: every camera again
    camBoxes.forEach((b) => { const c = camById($('input', b).value); $('input', b).checked = !gs.length || groupsOf(c).some((g) => gs.includes(g)); });
  }
  // saved views: a named set of cameras/groups + Show + Length (this browser)
  const loadViews = () => { try { return JSON.parse(localStorage.getItem('reviewViews') || '[]'); } catch { return []; } };
  const viewsRow = h('div', { class: 'row' });
  const current = () => ({ groups: groupBoxes.filter((b) => $('input', b).checked).map((b) => $('input', b).value), cameras: camBoxes.filter((b) => $('input', b).checked).map((b) => $('input', b).value), show: showSel.value, minMotion: Number(lenSel.value) });
  function useView(v) {
    groupBoxes.forEach((b) => { $('input', b).checked = v.groups.includes($('input', b).value); });
    camBoxes.forEach((b) => { $('input', b).checked = v.cameras.includes($('input', b).value); });
    showSel.value = SHOW.some(([x]) => x === v.show) ? v.show : ''; lenSel.value = v.minMotion || 0;
    apply({ keepPlayhead: true });
  }
  function drawViews() {
    const views = loadViews();
    fill(viewsRow, h('span', { class: 'muted' }, 'Views:'),
      ...views.map((v, i) => h('span', { class: 'chip view' }, h('button', { class: 'linkish', onclick: () => useView(v), title: 'show these cameras with these filters' }, v.name),
        h('button', { class: 'linkish x', title: `forget "${v.name}"`, onclick: () => { const all = loadViews(); all.splice(i, 1); localStorage.setItem('reviewViews', JSON.stringify(all)); drawViews(); } }, '✕'))),
      h('button', { class: 'ghost small', onclick: () => {
        const name = (prompt('Name this view (cameras, groups, Show and Length are saved; dates are not):', '') || '').trim();
        if (!name) return;
        const all = loadViews().filter((v) => v.name !== name); all.push({ name, ...current() });
        localStorage.setItem('reviewViews', JSON.stringify(all)); drawViews();
      } }, views.length ? '+ save view' : '+ save these cameras and filters as a view'));
  }
  drawViews();
  const manage = state.me.user.role === 'admin' ? h('a', { href: '#/admin', class: 'small', onclick: () => sessionStorage.setItem('adminFocus', 'groups') }, groupBoxes.length ? 'manage groups' : '+ create camera groups') : null;
  // the filters fold to one line (phones start folded): more room for the tiles
  const body = h('div', { class: 'filters-body' },
    h('div', { class: 'row' }, h('label', {}, 'From', fromIn), h('label', {}, 'To', toIn), h('label', {}, 'Range', quick), h('label', {}, 'Show', showSel), h('label', {}, 'Length', lenSel),
      h('span', { class: 'grow' }), applyBtn),
    viewsRow,
    groupBoxes.length || manage ? h('div', { class: 'row' }, h('span', { class: 'muted' }, 'Groups:'), ...groupBoxes, manage) : null,
    h('div', { class: 'row' }, h('span', { class: 'muted' }, 'Cameras:'), h('button', { class: 'ghost small', onclick: () => setAll(true) }, 'all'), h('button', { class: 'ghost small', onclick: () => setAll(false) }, 'none'), ...camBoxes));
  const summaryText = h('span', { class: 'muted small' });
  const foldBtn = h('button', { class: 'ghost small', onclick: () => setFolded(!body.hidden) });
  const setFolded = (on) => { body.hidden = on; foldBtn.textContent = on ? 'Filters ▾' : 'Hide filters ▴'; try { localStorage.setItem('reviewFolded', on ? '1' : '0'); } catch {} session?.fit?.(); };
  const filters = h('div', { class: 'card filters' }, h('div', { class: 'row filters-head' }, summaryText, h('span', { class: 'grow' }), foldBtn), body);
  const stage = h('div');
  fill(main, filters, stage);
  let session = null;
  let gen = 0;
  setFolded((localStorage.getItem('reviewFolded') ?? (window.innerWidth < 700 ? '1' : '0')) === '1');
  async function apply({ camerasOnly = false, keepPlayhead = camerasOnly } = {}) {
    if (!camerasOnly) {
      const from = new Date(fromIn.value).getTime(), to = new Date(toIn.value).getTime();
      if (!(from < to)) { toast('"From" must be before "To"', { kind: 'bad' }); return; }
      Object.assign(applied, { from, to, show: showSel.value, minMotion: Number(lenSel.value) });
      applyBtn.classList.remove('pending');
    }
    const my = ++gen;
    let chosen = camBoxes.filter((b) => $('input', b).checked).map((b) => camById($('input', b).value)).filter(Boolean);
    localStorage.setItem('reviewFilter', JSON.stringify({ from: applied.from, to: applied.to, show: applied.show, minMotion: applied.minMotion, groups: groupBoxes.filter((b) => $('input', b).checked).map((b) => $('input', b).value), cameras: chosen.map(keyOf) }));
    const range = { start: applied.from, end: Math.min(applied.to, Date.now()) };
    const counts = eventFilter(applied.show, applied.minMotion);
    if (applied.show && chosen.length) {
      const res = await Promise.all(chosen.map((c) => api.get(`${camApi(c)}/timeline?start=${Math.round(range.start)}&end=${Math.round(range.end)}`).catch(() => null)));
      if (my !== gen) return;
      chosen = chosen.filter((c, i) => res[i] && (applied.show === 'motion' ? res[i].events.some(counts) || (!applied.minMotion && res[i].motion.some((m) => m > 0)) : res[i].events.some(counts)));
    }
    // a camera change keeps watching the same moment
    const keep = keepPlayhead && session?.playhead ? session.playhead() : null;
    const startAt = keep ?? (atMs && atMs > range.start && atMs < range.end ? atMs : range.start);
    session?.destroy();
    const what = SHOW.find(([v]) => v === applied.show)?.[1].replace('Cameras with ', '');
    summaryText.textContent = `${fmtDT(range.start)} → ${fmtDT(range.end)} · ${SHOW.find(([v]) => v === applied.show)?.[1]}${applied.minMotion ? ` · ${applied.minMotion} s+ motion` : ''} · ${chosen.length} camera${chosen.length === 1 ? '' : 's'}`;
    session = reviewSession(stage, chosen, range, startAt, { counts, empty: applied.show ? `No selected camera has ${what} in this range.` : 'No cameras selected.' });
    atMs = null;
  }
  await apply();
  state.cleanup = () => session?.destroy();
}

function reviewSession(stage, cams, range, startAt, { counts = () => true, empty = 'No cameras selected.' } = {}) {
  if (!cams.length) { fill(stage, h('p', { class: 'muted' }, empty)); return { destroy() {} }; }
  const CHUNK = 2 * 60 * 1000;
  let playhead = startAt, paused = false, wasPaused = false;
  const grid = h('div', { class: 'grid montage fit' });
  const TILE_SPEEDS = [0.5, 1, 2, 4, 8, 16];
  const players = cams.map((c) => {
    const v = h('video', { muted: true, playsinline: true });
    const open = () => { location.hash = `#/camera/${keyOf(c)}/${Math.round(playhead)}`; };
    // per-camera speed: this tile leaves the group sync until set back to "sync"
    const own = h('select', { class: 'tilespeed', title: 'speed of this camera only', onclick: (e) => e.stopPropagation(), ondblclick: (e) => e.stopPropagation(),
      onchange: (e) => { const r = Number(e.target.value); p.own = r || null; tile.classList.toggle('unsynced', !!p.own); if (p.own) { p.video.playbackRate = p.own; if (!paused) p.video.play().catch(() => {}); } else resync(p); } },
      h('option', { value: '' }, 'sync'), ...TILE_SPEEDS.map((r) => h('option', { value: r }, `${r}×`)));
    const tile = h('div', { class: 'tile', ondblclick: open }, v, h('div', { class: 'label', onclick: open, title: 'open this camera at this moment' }, camLabel(c), ' ⤢'), h('div', { class: 'stat' }, ''), own);
    grid.append(tile);
    const p = { cam: c, video: v, mse: new MsePlayer(v), stat: $('.stat', tile), own: null, tile };
    return p;
  });
  const synced = () => players.filter((p) => !p.own);
  const scrubber = makeScrubber();
  const tl = makeTimeline({ rows: cams.map((c) => ({ id: c.id, key: keyOf(c), name: camLabel(c), api: camApi(c) })), range, fixedRange: true, tall: cams.length > 3, highlight: counts,
    onScrubStart: () => { wasPaused = paused; players.forEach((p) => p.video.pause()); },
    onScrub: (t) => { timeLabel.textContent = fmtDT(t); scrubber.show(players, t); },
    onSeek: (t) => seekAll(t, { resume: !wasPaused }) });
  const timeLabel = h('span', { class: 'time' });
  const togglePause = () => { paused = !paused; playBtn.textContent = paused ? '▶' : '⏸'; players.forEach((p) => paused ? p.video.pause() : p.video.play().catch(() => {})); };
  const playBtn = h('button', { class: 'ghost', title: 'play / pause (space)', onclick: togglePause }, '⏸');
  const rate = () => Number(speedSel.value);
  const speedSel = h('select', { title: 'speed of all synced cameras', onchange: () => synced().forEach((p) => { p.video.playbackRate = rate(); }) }, ...[1, 2, 4, 8, 16].map((r) => h('option', { value: r }, `${r}×`)));
  // quiet time: what to do between the events that count (Show / Length)
  const quietSel = h('select', { title: 'between events: play normally, play fast, or jump to the next event', onchange: () => localStorage.setItem('reviewQuiet', quietSel.value) },
    ...[['play', 'Quiet: normal'], ['fast', 'Quiet: fast (8×)'], ['skip', 'Quiet: skip']].map(([v, l]) => h('option', { value: v, selected: v === (localStorage.getItem('reviewQuiet') || 'play') }, l)));
  // previous / next event that counts for the current Show / Length
  const events = () => tl.events().filter(counts);
  const jump = (dir) => {
    const evs = events();
    const ev = dir > 0 ? evs.find((e) => e.start - 2000 > playhead + 1000) : [...evs].reverse().find((e) => e.start - 2000 < playhead - 3000);
    if (ev) seekAll(ev.start - 2000); else toast(dir > 0 ? 'No later event in this range' : 'No earlier event in this range');
  };
  const legend = state.me.objects ? h('span', { class: 'legend small' }, ...['motion', 'person', 'vehicle', 'animal'].map((k) => h('span', {}, h('i', { style: `background:${KIND_COLOR[k]}` }), k))) : null;
  const quietBadge = h('span', { class: 'pill', hidden: true }, '⏩ quiet');
  const controls = h('div', { class: 'controls' }, playBtn, speedSel, quietSel,
    h('button', { class: 'ghost', title: 'previous event (P)', onclick: () => jump(-1) }, '⏮ event'), h('button', { class: 'ghost', title: 'next event (N)', onclick: () => jump(1) }, 'event ⏭'),
    h('button', { class: 'ghost', onclick: () => seekAll(playhead - 60000) }, '−1 min'), h('button', { class: 'ghost', onclick: () => seekAll(playhead + 60000) }, '+1 min'),
    timeLabel, quietBadge, h('span', { class: 'grow' }), legend,
    h('span', { class: 'muted small' }, `${cams.length} camera${cams.length === 1 ? '' : 's'} · ${fmtDT(range.start)} → ${fmtDT(range.end)} · drag the timeline to scrub · N/P events · each tile has its own speed`));
  fill(stage, grid, tl.el, controls);
  const sizer = fitGrid(grid, () => players.length, { aspect: aspectOf(cams), below: () => tl.el.offsetHeight + controls.offsetHeight + 26, min: 240 });
  async function load(p, s, e, resume) {
    p.stat.textContent = '…';
    try {
      const stream = p.cam.sub_status ? 'sub' : 'main';
      await p.mse.play(`${camApi(p.cam)}/video.mp4?stream=${stream}&start=${Math.round(s)}&end=${Math.round(e)}`, { startMs: s });
      p.stat.textContent = ''; p.video.playbackRate = p.own || rate(); if (paused || !resume) p.video.pause();
      p.video.addEventListener('loadeddata', () => scrubber.hide(p), { once: true });
    } catch { p.stat.textContent = 'no recording'; scrubber.hide(p); }
  }
  async function seekAll(ms, { resume = true } = {}) {
    playhead = Math.max(range.start, Math.min(ms, range.end - 1000)); tl.setPlayhead(playhead); timeLabel.textContent = fmtDT(playhead);
    if (!resume && !paused) togglePause();
    const s = playhead, e = Math.min(range.end, s + CHUNK);
    await Promise.all(players.map((p) => load(p, s, e, resume)));
  }
  // back into the group: load at the shared playhead
  const resync = (p) => load(p, playhead, Math.min(range.end, playhead + CHUNK), !paused);
  let skipHold = 0; // no second skip while the first one loads
  const sync = setInterval(() => {
    if (tl.dragging) return;
    // not paused by the user, so nothing should sit paused (a first play() lost
    // while the tab was in the background, a stall after a seek)
    if (!paused) for (const p of players) { if (p.video.paused && p.video.readyState >= 2 && !p.video.ended) p.video.play().catch(() => {}); }
    const group = synced();
    const lead = group.find((p) => p.mse.wallMs() !== null && !p.video.paused && p.video.readyState >= 2);
    if (!lead) return;
    const t = lead.mse.wallMs(); playhead = t; timeLabel.textContent = fmtDT(t); tl.setPlayhead(t);
    // quiet time: no counted event here. A jump lands on the keyframe at or
    // before its 2 s lead-in, so up to 4 s before an event already counts as in it.
    const quiet = quietSel.value !== 'play' && Date.now() > skipHold && !events().some((ev) => ev.start - 4000 <= t && t <= (ev.end || ev.start) + 1000);
    quietBadge.hidden = !quiet;
    if (quiet && quietSel.value === 'skip') {
      const next = events().find((ev) => ev.start - 4000 > t);
      if (next) { skipHold = Date.now() + 4000; seekAll(next.start - 2000); return; }
    }
    const want = quiet && quietSel.value === 'fast' ? Math.max(8, rate()) : rate();
    for (const p of group) { if (p.video.playbackRate !== want) p.video.playbackRate = want; }
    for (const p of group) { if (p === lead) continue; const w = p.mse.wallMs(); if (w === null) continue; const d = (t - w) / 1000; if (Math.abs(d) > 0.3 * Math.max(1, want / 2)) p.video.currentTime += d; }
  }, 500);
  // the next chunk when the group runs out (a camera playing at its own speed loads its own)
  players.forEach((p) => p.video.addEventListener('ended', () => {
    const w = p.mse.wallMs() ?? playhead;
    if (p.own) { if (w + 1000 < range.end) load(p, w, Math.min(range.end, w + CHUNK), true); return; }
    if (p === synced()[0] && playhead + 2000 < range.end) seekAll(playhead + 500);
  }));
  const keys = (e) => {
    if (['INPUT', 'SELECT', 'TEXTAREA'].includes(e.target.tagName) || e.metaKey || e.ctrlKey) return;
    if (e.key === ' ') { e.preventDefault(); togglePause(); }
    else if (e.key === 'n' || e.key === 'N') jump(1);
    else if (e.key === 'p' || e.key === 'P') jump(-1);
    else if (e.key === 'ArrowRight') seekAll(playhead + (e.shiftKey ? 60000 : 10000));
    else if (e.key === 'ArrowLeft') seekAll(playhead - (e.shiftKey ? 60000 : 10000));
  };
  document.addEventListener('keydown', keys);
  tl.load().then(() => seekAll(startAt));
  return { playhead: () => playhead, fit: () => requestAnimationFrame(sizer.fit), destroy() { clearInterval(sync); sizer.destroy(); scrubber.destroy(); document.removeEventListener('keydown', keys); players.forEach((p) => p.mse.stop()); tl.destroy(); } };
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------
async function viewEvents(main) {
  const cams = state.cameras;
  const q = { camera: localStorage.getItem('evCam') || '', min_score: Number(localStorage.getItem('evMin') || 0), archived: false, range: localStorage.getItem('evRange') || '7d', kind: localStorage.getItem('evKind') || '', min_motion: Number(localStorage.getItem('evLen') || 0) };
  // without a detector every event is "motion": an object filter would only ever come back empty
  if (!state.me.objects) q.kind = '';
  const list = h('div', { class: 'events' });
  const more = h('button', { class: 'ghost' }, 'Load more');
  const count = h('span', { class: 'muted' });
  const camSel = h('select', { onchange: (e) => { q.camera = e.target.value; localStorage.setItem('evCam', q.camera); load(true); } },
    h('option', { value: '' }, 'All cameras'), ...allGroups().map((g) => h('option', { value: 'g:' + g, selected: 'g:' + g === q.camera }, `Group: ${g}`)), ...cams.map((c) => h('option', { value: keyOf(c), selected: keyOf(c) === q.camera }, camLabel(c))));
  const minSel = h('select', { onchange: (e) => { q.min_score = Number(e.target.value); localStorage.setItem('evMin', q.min_score); load(true); } },
    ...[[0, 'Any motion'], [60, 'Moderate+'], [120, 'Strong+'], [200, 'Very strong']].map(([v, l]) => h('option', { value: v, selected: v === q.min_score }, l)));
  const rangeSel = h('select', { onchange: (e) => { q.range = e.target.value; localStorage.setItem('evRange', q.range); load(true); } },
    ...[['1d', 'Last 24 h'], ['7d', 'Last 7 days'], ['30d', 'Last 30 days'], ['all', 'All']].map(([v, l]) => h('option', { value: v, selected: v === q.range }, l)));
  const lenSel = h('select', { title: 'hide short blips: seconds of actual motion', onchange: (e) => { q.min_motion = Number(e.target.value); localStorage.setItem('evLen', q.min_motion); load(true); } },
    ...LENGTHS.map(([v, l]) => h('option', { value: v, selected: v === q.min_motion }, l)));
  const arch = h('label', { class: 'row' }, h('input', { type: 'checkbox', style: 'width:auto', onchange: (e) => { q.archived = e.target.checked; load(true); } }), ' Archived only');
  const kindSel = state.me.objects ? h('select', { onchange: (e) => { q.kind = e.target.value; localStorage.setItem('evKind', q.kind); load(true); } },
    ...[['', 'Anything'], ...KINDS.map(([, l, v]) => [v, l]), ['motion', 'Motion only (no object)']].map(([v, l]) => h('option', { value: v, selected: v === q.kind }, l))) : null;
  fill(main, h('div', { class: 'toolbar' }, camSel, kindSel, minSel, lenSel, rangeSel, arch, h('span', { class: 'grow' }), count), list, h('div', { class: 'row', style: 'margin-top:12px' }, more));
  let before = null, total = 0;
  async function load(reset) {
    if (reset) { list.replaceChildren(); before = null; total = 0; }
    const days = { '1d': 1, '7d': 7, '30d': 30 }[q.range];
    // one query per server: the local one pages with `before`, peers contribute their first page once
    const servers = [...new Set(cams.map((c) => c.peer || ''))];
    const wanted = q.camera.startsWith('g:') ? cams.filter((c) => groupsOf(c).includes(q.camera.slice(2))) : q.camera ? [camById(q.camera)].filter(Boolean) : cams;
    const t0 = performance.now();
    const pages = await Promise.all(servers.map(async (pr) => {
      if (before && pr) return [];
      const mine = wanted.filter((c) => (c.peer || '') === pr);
      if (!mine.length) return [];
      const p = new URLSearchParams({ limit: 60 });
      if (q.camera) p.set('camera', mine.map((c) => c.id).join(','));
      if (q.min_score) p.set('min_score', q.min_score);
      if (q.kind) p.set('kind', q.kind);
      if (q.archived) p.set('archived', 'true');
      if (q.min_motion) p.set('min_motion', q.min_motion);
      if (days) p.set('start', Date.now() - days * 86400000);
      if (before && !pr) p.set('before', before);
      const evs = await api.get(`${apiBase(pr)}/events?${p}`).catch(() => []);
      return evs.map((ev) => (pr ? { ...ev, peer: pr, thumb: ev.thumb ? `/api/peers/${pr}${ev.thumb}` : null } : ev));
    }));
    const local = pages[servers.indexOf('')] || [];
    const evs = pages.flat().sort((a, b) => b.start - a.start);
    total += evs.length; before = local.length ? local[local.length - 1].id : before;
    more.disabled = local.length < 60;
    count.textContent = `${total} events (${(performance.now() - t0).toFixed(0)} ms)`;
    for (const ev of evs) list.append(eventCard(ev));
  }
  more.onclick = () => load(false);
  await load(true);
}
function eventCard(ev) {
  const cam = evCam(ev);
  return h('div', { class: 'event' + (ev.archived ? ' archived' : ''), onclick: () => openEvent(ev) },
    h('div', { class: 'thumb', style: ev.thumb ? `background-image:url(${ev.thumb})` : '' }, h('span', { class: 'score' + (ev.score > 120 ? ' hot' : '') }, `${ev.score}`),
      kindClass(ev) !== 'motion' ? h('span', { class: 'kind', style: `background:${KIND_COLOR[kindClass(ev)]}` }, ev.kind) : null),
    h('div', { class: 'meta' }, h('b', {}, cam ? camLabel(cam) : `camera ${ev.camera_id}`), `${fmtDT(ev.start)} · ${ev.end ? fmtDur(ev.end - ev.start) : 'in progress'}${ev.motion_secs != null ? ` · ${ev.motion_secs < 1 ? '<1' : Math.round(ev.motion_secs)} s motion` : ''}`,
      ev.objects?.length ? h('div', { class: 'labels' }, ...ev.objects.map((o) => h('span', { class: 'pill ok' }, `${o.label} ${Math.round(o.confidence * 100)}%`))) : null,
      ev.notes ? h('div', { class: 'muted' }, ev.notes) : null));
}
function openEvent(ev) {
  const cam = evCam(ev);
  const video = h('video', { controls: true, autoplay: true, playsinline: true, muted: true, style: 'width:100%;background:#000;border-radius:8px' });
  const notes = h('textarea', { rows: 2, placeholder: 'Notes' }, ev.notes || '');
  const archBtn = h('button', { class: 'ghost', onclick: async () => { ev.archived = !ev.archived; await api.patch(evApi(ev), { archived: ev.archived }); archBtn.textContent = ev.archived ? '★ Archived' : '☆ Archive'; } }, ev.archived ? '★ Archived' : '☆ Archive');
  const modal = h('div', { class: 'modal', onclick: (e) => { if (e.target === modal) close(); } },
    h('div', { class: 'card' },
      h('div', { class: 'row' }, h('h2', {}, `${cam ? camLabel(cam) : ''} — ${fmtDT(ev.start)}`), h('span', { class: 'grow' }), h('button', { class: 'ghost small', onclick: close }, '✕')),
      video,
      h('div', { class: 'row', style: 'margin-top:8px' }, archBtn,
        h('button', { class: 'ghost', onclick: () => { close(); location.hash = `#/camera/${cam ? keyOf(cam) : ev.camera_id}/${ev.peak || ev.start}`; } }, 'Open camera timeline'),
        h('button', { class: 'ghost', onclick: () => { close(); location.hash = `#/review/${ev.peak || ev.start}`; } }, 'Review all cameras at this time'),
        h('a', { class: 'muted', href: `${apiBase(ev.peer)}/cameras/${ev.camera_id}/video.mp4?start=${ev.start}&end=${ev.end || ev.start + 60000}`, download: `event-${ev.id}.mp4`, onclick: (e) => e.stopPropagation() }, 'Download'),
        h('span', { class: 'muted' }, `peak ${ev.score}${ev.objects?.length ? ' · ' + ev.objects.map((o) => `${o.label} ${Math.round(o.confidence * 100)}%`).join(', ') : ''}`)),
      h('label', {}, notes),
      h('button', { class: 'small', onclick: async () => { await api.patch(evApi(ev), { notes: notes.value }); ev.notes = notes.value; } }, 'Save notes')));
  closeOnEscape(modal, () => close());
  document.body.append(modal);
  const mse = new MsePlayer(video);
  const mq = cam ? mainQuery(cam) : null;
  const base = `${apiBase(ev.peer)}/cameras/${ev.camera_id}`;
  const url = mq !== null ? `${base}/video.mp4?start=${ev.start}&end=${ev.end || ev.start + 60000}${mq}` : `${base}/video.mp4?stream=sub&start=${ev.start}&end=${ev.end || ev.start + 60000}`;
  if (support.mse) mse.play(url, { startMs: ev.start }).catch((e) => { video.replaceWith(h('p', { class: 'err' }, e.message)); });
  else video.src = url;
  function close() { mse.stop(); modal.remove(); }
}

// ---------------------------------------------------------------------------
// Status: what a volunteer reads when something seems wrong
// ---------------------------------------------------------------------------
async function viewStatus(main) {
  const render = async () => {
    const s = await api.get('/api/status');
    const ago = (ms) => ms ? fmtDur(Date.now() - ms) + ' ago' : '—';
    const issues = s.issues.length ? h('div', { class: 'card issues' }, ...s.issues.map((i) => h('div', { class: 'issue ' + i.level }, h('b', {}, i.subject), ' ', i.text))) : h('div', { class: 'card ok' }, '✓ Everything is recording and detecting.');
    const cams = h('table', {}, h('tr', {}, h('th', {}, 'Camera'), h('th', {}, 'Recording'), h('th', {}, 'Stream'), h('th', {}, 'Detector'), h('th', {}, 'Last event'), h('th', {}, 'Events 24 h'), h('th', {}, 'Ingest 24 h'), h('th', {}, 'Oldest footage'), h('th', {}, 'Reconnects')),
      ...s.cameras.map((c) => h('tr', { onclick: () => { location.hash = `#/camera/${c.id}`; }, style: 'cursor:pointer' },
        h('td', {}, c.name, c.enabled ? '' : h('span', { class: 'muted' }, ' (disabled)')),
        h('td', {}, h('span', { class: 'pill ' + (c.recording ? 'ok' : 'bad') }, c.recording ? 'recording' : 'down'), c.last_error ? h('div', { class: 'muted small' }, c.last_error) : null),
        h('td', {}, c.recording ? `${c.codec || ''} ${c.width}×${c.height} ${c.fps.toFixed(0)} fps ${(c.kbps / 1000).toFixed(1)} Mb/s` : ago(c.last_frame_ms), c.sub_recording === false ? h('div', { class: 'muted small' }, 'substream down') : null),
        h('td', {}, h('span', { class: 'pill ' + (c.detect_running ? 'ok' : 'bad') }, c.detect_running ? `${c.detect_fps.toFixed(1)} fps` : 'stopped'), c.in_event ? ' MOTION' : ''),
        h('td', {}, ago(c.last_event_ms)), h('td', {}, c.events_24h), h('td', {}, gb(c.ingest_24h)), h('td', {}, c.oldest_ms ? fmtDur(Date.now() - c.oldest_ms) : '—'), h('td', {}, c.reconnects))));
    const stor = s.storages.length ? h('table', {}, h('tr', {}, h('th', {}, 'Volume'), h('th', {}, 'Used'), h('th', {}, 'Free'), h('th', {}, 'Headroom'), h('th', {}, 'Ingest / day'), h('th', {}, 'Holds'), h('th', {}, 'Oldest'), h('th', {}, 'Archive backlog')),
      ...s.storages.map((v) => h('tr', {}, h('td', {}, h('code', {}, v.path), v.available ? '' : h('span', { class: 'pill bad' }, ' not mounted'), v.archive_to ? h('span', { class: 'muted small' }, ` → archive ${v.archive_to}`) : null),
        h('td', {}, gb(v.used_bytes)), h('td', {}, gb(v.free_bytes)), h('td', {}, gb(v.headroom_bytes)), h('td', {}, gb(v.ingest_24h)),
        h('td', {}, v.effective_days === null ? '—' : `${v.effective_days.toFixed(1)} days`), h('td', {}, v.oldest_ms ? fmtDur(Date.now() - v.oldest_ms) : '—'),
        h('td', {}, v.archive_to ? (v.ingest_24h > 0 && v.archive_backlog_bytes > v.ingest_24h ? h('span', { class: 'pill bad' }, gb(v.archive_backlog_bytes)) : gb(v.archive_backlog_bytes)) : '—')))) : null;
    const peers = state.me.peers?.length ? await api.get('/api/peers').catch(() => []) : [];
    const peerCard = peers.length ? h('div', { class: 'card' }, h('h2', {}, 'Peers'), ...peers.map((p) => h('div', {}, h('span', { class: 'pill ' + (p.ok ? 'ok' : 'bad') }, p.ok ? 'reachable' : 'unreachable'), ' ', p.name, p.url ? h('span', { class: 'muted small' }, ` ${p.url}`) : null))) : null;
    fill(main,
      peerCard,
      h('div', { class: 'toolbar' }, h('h2', {}, 'Status'), h('span', { class: 'muted' }, `zmng ${s.version} · up ${fmtDur(s.uptime_secs * 1000)} · load ${s.load.map((l) => l.toFixed(1)).join(' ')}`), h('span', { class: 'grow' }), h('button', { class: 'ghost small', onclick: render }, 'Refresh')),
      issues,
      h('div', { class: 'card' }, h('h2', {}, 'Cameras'), cams),
      stor ? h('div', { class: 'card' }, h('h2', {}, 'Storage'), stor, h('p', { class: 'muted' }, '"Holds" is how many days of footage fit at the last 24 hours\' ingest rate before retention starts deleting. Prometheus: GET /api/metrics with an admin token.')) : null);
  };
  await render();
  const t = setInterval(() => render().catch(() => {}), 30000);
  state.cleanup = () => clearInterval(t);
}

// ---------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------
async function viewAdmin(main) {
  if (state.me.user.role !== 'admin') { fill(main, h('p', { class: 'muted' }, 'Admins only.')); return; }
  const [cams, users, storages] = await Promise.all([api.get('/api/cameras'), api.get('/api/users'), api.get('/api/storages')]);
  state.cameras = [...cams, ...state.cameras.filter((c) => c.peer)];
  const pill = (st, label) => h('span', { class: 'pill ' + (st?.connected ? 'ok' : 'bad') }, st?.connected ? label : (st?.last_error || 'offline'));
  const codecName = (c) => (c?.startsWith('hvc') || c?.startsWith('hev') ? 'HEVC' : c?.startsWith('avc') ? 'H.264' : c || '');
  const chips = (list) => list.map((g) => h('span', { class: 'pill' }, g));
  const camTable = h('table', {}, h('tr', {}, h('th', {}, 'Camera'), h('th', {}, 'Groups'), h('th', {}, 'Streams'), h('th', {}, 'Storage'), h('th', {}, 'Retention'), h('th', {}, 'Motion'), h('th', {}, '')),
    ...cams.map((c) => h('tr', {},
      h('td', {}, h('b', {}, c.name), h('div', { class: 'muted small' }, `#${c.id}${c.enabled ? '' : ' · disabled'}`)),
      h('td', {}, ...chips(groupsOf(c))),
      h('td', {}, pill(c.status, `${codecName(c.status.codec)} ${c.status.width}×${c.status.height} ${c.status.fps.toFixed(0)} fps`), ' ', c.sub_status ? pill(c.sub_status, `sub ${c.sub_status.width}×${c.sub_status.height}`) : null),
      h('td', {}, storageName(storages, c.storage_id)), h('td', {}, `${c.retention_days} d${c.event_retention_days ? ` · events ${c.event_retention_days} d` : ''}`),
      h('td', {}, c.detect ? `${c.detect.fps.toFixed(1)} fps${c.objects && state.me.objects ? ' + objects' : ''}` : '—'),
      h('td', { class: 'actions' }, h('button', { class: 'ghost small', onclick: () => editCamera(c) }, 'Edit'), h('button', { class: 'ghost small', onclick: async () => { if (confirm(`Delete camera ${c.name} and ALL its recordings? This cannot be undone.`)) { await api.del(`/api/cameras/${c.id}`); route(); } } }, 'Delete')))));
  // new cameras go to the primary volume (the one that tiers to an archive), not the archive itself
  const primary = storages.find((st) => st.archive_to && !st.read_only) || storages.find((st) => !st.read_only);
  const addCam = h('form', { class: 'row', onsubmit: async (e) => { e.preventDefault(); const f = new FormData(e.target); try { await api.post('/api/cameras', { name: f.get('name'), main_url: f.get('main_url'), sub_url: f.get('sub_url') || null, storage_id: Number(f.get('storage_id')) }); toast(`Added ${f.get('name')}; recording starts within ~30 s`, { kind: 'ok' }); route(); } catch (err) { toast(err.message, { kind: 'bad' }); } } },
    h('input', { name: 'name', placeholder: 'Name', required: true, style: 'width:160px' }), h('input', { name: 'main_url', placeholder: 'main rtsp://user:pass@ip/Streaming/Channels/101', required: true, style: 'flex:2;min-width:240px' }), h('input', { name: 'sub_url', placeholder: 'substream rtsp://…/102 (recommended)', style: 'flex:2;min-width:240px' }),
    h('select', { name: 'storage_id', style: 'width:auto', title: 'where new recordings go' }, ...storages.filter((st) => !st.read_only).map((st) => h('option', { value: st.id, selected: st === primary }, `→ ${storageName(storages, st.id)}`))), h('button', {}, 'Add camera'));
  const camName = (id) => cams.find((c) => c.id === id)?.name || `#${id}`;
  const access = (u) => u.role === 'admin' ? h('span', { class: 'muted' }, 'all cameras')
    : (u.groups.length || u.cameras.length) ? h('span', {}, ...u.groups.map((g) => h('span', { class: 'pill' }, `group: ${g}`)), ...u.cameras.map((id) => h('span', { class: 'pill' }, camName(id))), h('div', { class: 'muted small' }, `sees ${u.visible.length} camera${u.visible.length === 1 ? '' : 's'}`))
      : h('span', { class: 'err' }, 'nothing yet');
  const userTable = h('table', {}, h('tr', {}, h('th', {}, 'User'), h('th', {}, 'Role'), h('th', {}, 'Can see'), h('th', {}, '')),
    ...users.map((u) => h('tr', {}, h('td', {}, h('b', {}, u.username), u.id === state.me.user.id ? h('span', { class: 'muted small' }, ' (you)') : null), h('td', {}, u.role), h('td', {}, access(u)),
      h('td', { class: 'actions' }, h('button', { class: 'ghost small', onclick: () => editUser(u, cams, users) }, 'Edit'), u.id === state.me.user.id ? null : h('button', { class: 'ghost small', onclick: async () => { if (confirm(`Delete user ${u.username}?`)) { await api.del(`/api/users/${u.id}`); route(); } } }, 'Delete')))));
  const addUser = h('form', { class: 'row', onsubmit: async (e) => {
    e.preventDefault(); const f = new FormData(e.target);
    try {
      const r = await api.post('/api/users', { username: f.get('username'), password: f.get('password'), role: f.get('role') });
      const fresh = await api.get('/api/users');
      const u = fresh.find((x) => x.id === r.id);
      if (u && u.role !== 'admin') { toast(`Created ${u.username}: now choose what they can see`, { kind: 'ok' }); editUser(u, cams, fresh); } else route();
    } catch (err) { toast(err.message, { kind: 'bad' }); }
  } },
    h('input', { name: 'username', placeholder: 'username', required: true, style: 'width:150px' }), h('input', { name: 'password', type: 'password', placeholder: 'password (8+)', required: true, style: 'width:160px' }), h('select', { name: 'role', style: 'width:auto' }, h('option', { value: 'viewer' }, 'viewer'), h('option', { value: 'admin' }, 'admin')), h('button', {}, 'Add user'));
  const stor = h('table', {}, h('tr', {}, h('th', {}, 'Volume'), h('th', {}, 'Used'), h('th', {}, 'Free'), h('th', {}, 'Cap'), h('th', {}, 'Keep free'), h('th', {}, 'Tiering'), h('th', {}, '')),
    ...storages.map((st) => h('tr', {}, h('td', {}, h('b', {}, storageName(storages, st.id)), h('div', {}, h('code', {}, st.path)), st.available ? null : h('span', { class: 'pill bad' }, 'not mounted')), h('td', {}, gb(st.used_bytes)), h('td', {}, gb(st.free_bytes)), h('td', {}, st.max_bytes ? gb(st.max_bytes) : '—'), h('td', {}, gb(st.reserve_bytes)),
      h('td', {}, st.read_only ? h('span', { class: 'pill' }, 'read-only') : st.archive_to ? `→ ${storageName(storages, st.archive_to)} after ${st.archive_after_days ?? '—'} d @ ${st.archive_rate_mbps} Mb/s` : '—'),
      h('td', { class: 'actions' }, h('button', { class: 'ghost small', onclick: () => editStorage(st, storages) }, 'Edit')))));
  const tokenBtn = h('button', { class: 'ghost small', onclick: async () => { const t = await api.post('/api/tokens'); prompt('Long-lived API token (Home Assistant etc.). Shown once:', t.token); } }, 'Create API token');
  const objects = state.me.objects
    ? h('div', {}, h('p', {}, h('span', { class: 'pill ok' }, 'detector configured'), ' People, vehicles and animals are labelled on motion, and again from each event\'s full-resolution picture when it ends.'),
      h('div', { class: 'row' }, h('button', { class: 'ghost small', onclick: async (e) => { e.target.disabled = true; try { const r = await api.post('/api/events/classify', { days: 7 }); toast(`Checking ${r.queued} events from the last 7 days in the background`, { kind: 'ok' }); } catch (err) { toast(err.message, { kind: 'bad' }); } } }, 'Label the last 7 days of events'), h('span', { class: 'muted small' }, 'for events recorded before the detector was set up')))
    : h('p', {}, h('span', { class: 'pill' }, 'not configured'), ' Only motion is detected, so events cannot be filtered by people or vehicles. Add an ', h('code', {}, '[objects]'), ' section to zmng.toml pointing at a detection server (see deploy/detector).');
  const focus = sessionStorage.getItem('adminFocus'); sessionStorage.removeItem('adminFocus');
  const groupsCard = h('div', { class: 'card', id: 'groups' }, h('h2', {}, 'Camera groups'), groupsEditor(cams, users));
  fill(main,
    h('div', { class: 'card' }, h('h2', {}, 'Cameras'), h('div', { class: 'tablewrap' }, camTable), h('div', { style: 'margin-top:10px' }, addCam), h('p', { class: 'muted small' }, 'Camera changes take effect within about 30 seconds.')),
    groupsCard,
    h('div', { class: 'two' },
      h('div', { class: 'card' }, h('h2', {}, 'Users'), h('div', { class: 'tablewrap' }, userTable), h('div', { style: 'margin-top:10px' }, addUser), h('p', { class: 'muted small' }, 'Viewers see the cameras in their groups plus any assigned one by one — nothing until you choose. A camera added to a group is shared with that group\'s users at once.'), tokenBtn),
      h('div', { class: 'card' }, h('h2', {}, 'Object detection'), objects)),
    h('div', { class: 'card' }, h('h2', {}, 'Storage'), h('div', { class: 'tablewrap' }, stor), h('p', { class: 'muted small' }, 'Retention deletes whole segments, oldest first: per-camera age limit (events kept longer), then per-volume space budget. Tiering copies old segments to an archive volume first.')),
    h('details', { class: 'card' }, h('summary', {}, 'Diagnostics'), h('p', {}, `This browser — MSE: ${support.mse} · HEVC via MSE: ${support.hevcMse} · H.264 via MSE: ${support.h264Mse} · native HLS: ${support.nativeHls} · server transcode: ${!!state.me.transcode}`), h('p', { class: 'muted' }, 'zmNinjaNg: use this server\'s address plus /zm as the portal URL. Live video plays as MSE (H.264 substream) through the app\'s go2rtc player.')));
  if (focus === 'groups') groupsCard.scrollIntoView({ block: 'start' });
}
const gb = (b) => `${(b / 1e9).toFixed(1)} GB`;
const storageName = (all, id) => { const st = all.find((x) => x.id === id); if (!st) return `storage ${id}`; return st.read_only ? `imported (${id})` : st.archive_to ? `primary (${id})` : all.some((x) => x.archive_to === id) ? `archive (${id})` : `storage ${id}`; };

// Groups are the cameras' comma-separated tags: one matrix of checkboxes
// (cameras × groups), saved as you click.
function groupsEditor(cams, users = []) {
  const groups = [...new Set(cams.flatMap(groupsOf))].sort((a, b) => a.localeCompare(b));
  const save = async (c, tags) => {
    const t = [...new Set(tags)];
    await api.patch(`/api/cameras/${c.id}`, { tags: t.join(', ') });
    c.tags = t.join(', ');
    const sc = state.cameras.find((x) => !x.peer && x.id === c.id); if (sc) sc.tags = c.tags;
  };
  const redraw = () => { const card = $('#groups'); if (card) fill(card, h('h2', {}, 'Camera groups'), groupsEditor(cams, users)); };
  const sharedWith = (g) => users.filter((u) => u.role !== 'admin' && u.groups.some((x) => x.toLowerCase() === g.toLowerCase())).map((u) => u.username);
  // rename/remove on the server: camera tags and users' group grants change together
  const renameGroup = async (g) => {
    const n = (prompt(`Rename group "${g}" to:`, g) || '').trim();
    if (!n || n === g) return;
    try { await api.post('/api/groups/rename', { from: g, to: n }); } catch (err) { toast(err.message, { kind: 'bad' }); return; }
    toast(`Renamed "${g}" to "${n}"`, { kind: 'ok' }); refresh();
  };
  const deleteGroup = async (g) => {
    const who = sharedWith(g);
    if (!confirm(`Remove group "${g}"? The cameras are not changed${who.length ? `, but ${who.join(', ')} will no longer see them through this group` : ''}.`)) return;
    try { await api.post('/api/groups/rename', { from: g, to: null }); } catch (err) { toast(err.message, { kind: 'bad' }); return; }
    toast(`Removed group "${g}"`, { kind: 'ok' }); refresh();
  };
  // users' grants change with a rename/removal: redraw the whole page, back at the groups
  const refresh = () => { sessionStorage.setItem('adminFocus', 'groups'); route(); };
  const newName = h('input', { placeholder: 'New group name, e.g. Outside', style: 'width:220px' });
  const newBoxes = cams.map((c) => h('label', { class: 'chip' }, h('input', { type: 'checkbox', value: c.id }), c.name));
  const create = async (e) => {
    e.preventDefault();
    const n = newName.value.trim();
    if (!n) { newName.focus(); return; }
    if (n.includes(',')) { toast('Group names cannot contain commas', { kind: 'bad' }); return; }
    const picked = newBoxes.filter((b) => $('input', b).checked).map((b) => cams.find((c) => String(c.id) === $('input', b).value));
    if (!picked.length) { toast('Tick the cameras that belong in the group', { kind: 'bad' }); return; }
    for (const c of picked) await save(c, [...groupsOf(c), n]);
    toast(`Created group "${n}" with ${picked.length} camera${picked.length === 1 ? '' : 's'}`, { kind: 'ok' }); redraw();
  };
  const matrix = groups.length ? h('div', { class: 'tablewrap' }, h('table', { class: 'matrix' },
    h('tr', {}, h('th', {}, 'Camera'), ...groups.map((g) => h('th', {}, h('span', {}, g), ' ', h('button', { class: 'ghost small', title: `rename ${g}`, onclick: () => renameGroup(g) }, '✎'), h('button', { class: 'ghost small', title: `remove ${g}`, onclick: () => deleteGroup(g) }, '✕')))),
    ...cams.map((c) => h('tr', {}, h('td', {}, c.name), ...groups.map((g) => h('td', {}, h('input', { type: 'checkbox', checked: groupsOf(c).includes(g), 'aria-label': `${c.name} in ${g}`,
      onchange: async (e) => { try { await save(c, e.target.checked ? [...groupsOf(c), g] : groupsOf(c).filter((x) => x !== g)); redraw(); } catch (err) { e.target.checked = !e.target.checked; toast(err.message, { kind: 'bad' }); } } }))))),
    h('tr', {}, h('td', { class: 'muted small' }, 'cameras'), ...groups.map((g) => h('td', { class: 'muted small' }, String(cams.filter((c) => groupsOf(c).includes(g)).length)))),
    h('tr', {}, h('td', { class: 'muted small' }, 'shared with'), ...groups.map((g) => h('td', { class: 'muted small' }, sharedWith(g).join(', ') || '—')))))
    : h('p', { class: 'muted' }, 'No groups yet. Groups let you pick a set of cameras with one click on the Live and Review pages (e.g. Outside, Doors, CBA).');
  return h('div', {}, matrix,
    h('form', { class: 'newgroup', onsubmit: create }, h('div', { class: 'row' }, newName, h('button', {}, 'Create group')), h('div', { class: 'row' }, h('span', { class: 'muted small' }, 'with:'), ...newBoxes)),
    h('p', { class: 'muted small' }, 'Ticks save immediately. A camera can be in several groups. Share a group with viewers in Users → Edit; zmNinjaNg shows these groups too.'));
}

function closeOnEscape(modal, close) {
  modal._close = close;
  modal.tabIndex = -1;
  setTimeout(() => modal.focus(), 0);
}
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  const m = [...document.querySelectorAll('.modal')].pop();
  if (m) { e.preventDefault(); (m._close || (() => m.remove()))(); }
});

function editCamera(c) {
  const inputs = {};
  const text = (k, label, opts = {}) => h('label', { class: opts.wide ? 'wide' : '' }, label, inputs[k] = h('input', { value: c[k] ?? '', type: opts.type || 'text', placeholder: opts.ph || '', autocomplete: 'off' }), opts.hint ? h('span', { class: 'hint' }, opts.hint) : null);
  const num = (k, label, hint) => text(k, label, { type: 'number', hint });
  const check = (k, label) => h('label', { class: 'row check' }, inputs[k] = h('input', { type: 'checkbox', checked: c[k] }), label);
  const secret = (k, label) => {
    const inp = h('input', { value: c[k] ?? '', type: 'password', autocomplete: 'off', spellcheck: 'false' });
    inputs[k] = inp;
    return h('label', { class: 'wide' }, label, h('div', { class: 'row nowrap' }, inp, h('button', { type: 'button', class: 'ghost small', onclick: (e) => { inp.type = inp.type === 'password' ? 'text' : 'password'; e.target.textContent = inp.type === 'password' ? 'show' : 'hide'; } }, 'show')));
  };
  // existing groups as one-click chips that edit the text field
  const tagsField = text('tags', 'Groups', { wide: true, ph: 'comma separated, e.g. Outside, CBA' });
  const groupChips = h('div', { class: 'row' }, ...allGroups().map((g) => h('button', { type: 'button', class: 'ghost small', onclick: () => { const cur = inputs.tags.value.split(',').map((x) => x.trim()).filter(Boolean); inputs.tags.value = (cur.includes(g) ? cur.filter((x) => x !== g) : [...cur, g]).join(', '); } }, g)));
  const section = (title, ...kids) => h('fieldset', {}, h('legend', {}, title), h('div', { class: 'fgrid' }, ...kids));
  const strings = ['name', 'tags', 'object_labels', 'onvif_url', 'onvif_user', 'onvif_pass', 'main_url', 'sub_url', 'zones_json', 'masks_json'];
  const checks = ['enabled', 'record_sub', 'record_audio', 'objects', 'require_object'];
  const errEl = h('p', { class: 'err' });
  const form = h('form', { onsubmit: async (e) => {
    e.preventDefault(); const patch = {};
    for (const [k, inp] of Object.entries(inputs)) {
      if (checks.includes(k)) { if (inp.checked !== c[k]) patch[k] = inp.checked; continue; }
      let v = inp.value;
      if (k === 'onvif_pass' && v === '') continue;
      if (v === '' && k === 'sub_url') v = null; else if (!strings.includes(k)) v = Number(v);
      if (v !== (c[k] ?? (k === 'sub_url' ? null : ''))) patch[k] = v;
    }
    if (!Object.keys(patch).length) { close(); return; }
    try { await api.patch(`/api/cameras/${c.id}`, patch); close(); toast(`Saved ${c.name}; changes apply within ~30 s`, { kind: 'ok' }); state.cameras = [...await api.get('/api/cameras'), ...state.cameras.filter((x) => x.peer)]; route(); } catch (err) { errEl.textContent = err.message; }
  } },
    section('General', text('name', 'Name'), h('div', { class: 'wide' }, check('enabled', 'Enabled (recording)')), tagsField, allGroups().length ? h('div', { class: 'wide' }, groupChips) : null, num('sort_order', 'Sort order')),
    section('Streams', secret('main_url', 'Main RTSP URL'), secret('sub_url', 'Substream RTSP URL (low-res; used for live tiles, review and motion)'), h('div', { class: 'wide' }, check('record_sub', 'Record the substream too (review, phones, previews)')), h('div', { class: 'wide' }, check('record_audio', 'Record audio (AAC track of the main stream)'))),
    section('Retention', num('retention_days', 'Keep footage (days)'), num('event_retention_days', 'Keep footage with events (days, 0 = same)')),
    section('Motion detection', num('detect_fps', 'Frames per second'), num('pixel_threshold', 'Pixel threshold (0–255)', 'higher = ignores smaller brightness changes'), num('min_area_pct', 'Min changed area %'), num('min_blob_pct', 'Min blob %', 'size of the largest moving object'),
      num('pre_secs', 'Pre-roll (s)'), num('post_secs', 'Post-roll (s)'), num('cooldown_secs', 'Cooldown (s)', 'quiet time before an event ends'), num('detect_width', 'Analysis width'), num('detect_height', 'Analysis height'),
      text('zones_json', 'Zones JSON', { wide: true, hint: '[[[x,y],…]] in 0..1; empty = whole frame' }), text('masks_json', 'Masks JSON', { wide: true, hint: 'areas to ignore, same format' })),
    section('Object detection', h('div', { class: 'wide' }, check('objects', state.me.objects ? 'Detect people, vehicles and animals on motion' : 'Detect objects on motion (no detector configured yet)')), h('div', { class: 'wide' }, check('require_object', 'Only keep events with a detected object')), text('object_labels', 'Labels', { wide: true, ph: 'blank = server default (person, car, truck, bus, motorcycle, bicycle, dog, cat)' })),
    section('ONVIF / PTZ', text('onvif_url', 'ONVIF device URL', { wide: true, ph: 'blank = http://<camera host>/onvif/device_service' }), text('onvif_user', 'ONVIF user', { ph: 'blank = RTSP user' }), text('onvif_pass', 'ONVIF password', { type: 'password', ph: 'blank = keep / RTSP password' }),
      h('div', { class: 'row wide' }, h('span', { class: 'muted small' }, c.ptz ? `PTZ: ${c.ptz_url} profile ${c.ptz_profile}` : 'PTZ: not probed'), h('button', { type: 'button', class: 'ghost small', onclick: async () => { try { const r = await api.post(`/api/cameras/${c.id}/ptz/probe`); errEl.textContent = `PTZ found: ${r.ptz_url} (profile ${r.profile})`; } catch (e) { errEl.textContent = `PTZ probe: ${e.message}`; } } }, 'Probe PTZ (ONVIF)'))),
    h('div', { class: 'row sticky-actions' }, h('button', {}, 'Save'), h('button', { type: 'button', class: 'ghost', onclick: () => close() }, 'Cancel'), h('span', { class: 'muted small' }, 'Changes apply within ~30 s')));
  const modal = h('div', { class: 'modal' }, h('div', { class: 'card wide-modal' }, h('div', { class: 'row' }, h('h2', {}, `${c.name}`), h('span', { class: 'muted' }, `camera ${c.id}`), h('span', { class: 'grow' }), h('button', { type: 'button', class: 'ghost small', onclick: () => close() }, '✕')), form, errEl));
  const close = () => modal.remove();
  closeOnEscape(modal, close);
  document.body.append(modal);
}
function editStorage(s, all) {
  const inputs = {};
  const form = h('form', { onsubmit: async (e) => { e.preventDefault(); const patch = { max_bytes: inputs.max_gb.value === '' ? null : Math.round(Number(inputs.max_gb.value) * 1e9), reserve_bytes: Math.round(Number(inputs.reserve_gb.value) * 1e9), archive_to: inputs.archive_to.value === '' ? null : Number(inputs.archive_to.value), archive_after_days: inputs.archive_after_days.value === '' ? null : Number(inputs.archive_after_days.value), archive_rate_mbps: Number(inputs.archive_rate_mbps.value), read_only: inputs.read_only.checked }; try { await api.patch(`/api/storages/${s.id}`, patch); modal.remove(); route(); } catch (err) { errEl.textContent = err.message; } } },
    h('label', {}, 'Cap (GB, blank = use free-space reserve only)', inputs.max_gb = h('input', { value: s.max_bytes ? (s.max_bytes / 1e9).toFixed(0) : '' })),
    h('label', {}, 'Reserve free (GB)', inputs.reserve_gb = h('input', { value: (s.reserve_bytes / 1e9).toFixed(0) })),
    h('label', {}, 'Archive to (tiering)', inputs.archive_to = h('select', {}, h('option', { value: '' }, 'never (delete here by retention)'), ...all.filter((o) => o.id !== s.id).map((o) => h('option', { value: o.id, selected: o.id === s.archive_to }, `storage ${o.id}: ${o.path}`)))),
    h('label', {}, 'Move segments older than (days)', inputs.archive_after_days = h('input', { value: s.archive_after_days ?? '' })),
    h('label', {}, 'Copy rate limit (Mb/s)', inputs.archive_rate_mbps = h('input', { value: s.archive_rate_mbps })),
    h('label', { class: 'row' }, inputs.read_only = h('input', { type: 'checkbox', style: 'width:auto', checked: s.read_only }), ' Read-only (imported ZoneMinder events: never delete, move or write here)'),
    h('p', { class: 'muted' }, 'Recent footage of every camera stays here; older segments are copied to the archive volume (checksummed), then removed here. If the archive volume is missing, nothing is moved and recording continues.'),
    h('div', { class: 'row' }, h('button', {}, 'Save'), h('button', { type: 'button', class: 'ghost', onclick: () => modal.remove() }, 'Cancel')));
  const errEl = h('p', { class: 'err' });
  const modal = h('div', { class: 'modal' }, h('div', { class: 'card' }, h('h2', {}, `Storage ${s.id}`), form, errEl));
  closeOnEscape(modal, () => modal.remove());
  document.body.append(modal);
}
// Edit a user: name, password, role, and — for viewers — what they may see:
// whole groups (cameras added to the group later are included) and/or
// individual cameras.
function editUser(u, cams, users) {
  const isMe = u.id === state.me.user.id;
  const name = h('input', { value: u.username, autocomplete: 'off' });
  const pw = h('input', { type: 'password', placeholder: 'leave blank to keep', autocomplete: 'new-password' });
  const role = h('select', { disabled: isMe, title: isMe ? 'you cannot change your own role' : '' }, ...['viewer', 'admin'].map((r) => h('option', { value: r, selected: r === u.role }, r)));
  const groups = [...new Set([...allGroups(), ...u.groups])].sort((a, b) => a.localeCompare(b));
  const gBoxes = groups.map((g) => h('label', { class: 'chip' }, h('input', { type: 'checkbox', value: g, checked: u.groups.some((x) => x.toLowerCase() === g.toLowerCase()), onchange: preview }), g, h('span', { class: 'muted small' }, ` (${cams.filter((c) => groupsOf(c).some((x) => x.toLowerCase() === g.toLowerCase())).length})`)));
  const cBoxes = cams.map((c) => h('label', { class: 'chip' }, h('input', { type: 'checkbox', value: c.id, checked: u.cameras.includes(c.id), onchange: preview }), c.name));
  const sees = h('p', { class: 'muted small' });
  const accessBox = h('div', {}, h('h3', {}, 'Can see'),
    groups.length ? h('div', {}, h('div', { class: 'muted small' }, 'Groups (cameras added to a group later are included automatically)'), h('div', { class: 'row' }, ...gBoxes)) : h('p', { class: 'muted small' }, 'No camera groups yet — create them under Camera groups to share sets of cameras.'),
    h('div', { class: 'muted small', style: 'margin-top:8px' }, 'Individual cameras'), h('div', { class: 'row' }, ...cBoxes), sees);
  function preview() {
    const gs = gBoxes.filter((b) => $('input', b).checked).map((b) => $('input', b).value.toLowerCase());
    const ids = new Set(cBoxes.filter((b) => $('input', b).checked).map((b) => Number($('input', b).value)));
    cams.forEach((c) => { if (groupsOf(c).some((g) => gs.includes(g.toLowerCase()))) ids.add(c.id); });
    sees.textContent = ids.size ? `Will see ${ids.size}: ${cams.filter((c) => ids.has(c.id)).map((c) => c.name).join(', ')}` : 'Will see nothing (fails closed).';
  }
  const showAccess = () => { accessBox.hidden = role.value === 'admin'; };
  role.addEventListener('change', showAccess);
  const errEl = h('p', { class: 'err' });
  const save = async (e) => {
    e.preventDefault();
    const patch = {};
    if (name.value.trim() !== u.username) patch.username = name.value.trim();
    if (pw.value) patch.password = pw.value;
    if (!isMe && role.value !== u.role) patch.role = role.value;
    if (role.value !== 'admin') {
      patch.groups = gBoxes.filter((b) => $('input', b).checked).map((b) => $('input', b).value);
      patch.cameras = cBoxes.filter((b) => $('input', b).checked).map((b) => Number($('input', b).value));
    }
    try {
      await api.patch(`/api/users/${u.id}`, patch);
      close();
      if (isMe && patch.username) { state.me.user.username = patch.username; toast(`You are now "${patch.username}" (use it next time you sign in, also in zmNinjaNg)`, { kind: 'ok', ttl: 12000 }); } else toast(`Saved ${patch.username || u.username}`, { kind: 'ok' });
      route();
    } catch (err) { errEl.textContent = err.message; }
  };
  const modal = h('div', { class: 'modal' }, h('form', { class: 'card wide-modal', onsubmit: save },
    h('div', { class: 'row' }, h('h2', {}, `User ${u.username}`), h('span', { class: 'grow' }), h('button', { type: 'button', class: 'ghost small', onclick: () => close() }, '✕')),
    h('div', { class: 'fgrid' }, h('label', {}, 'Username', name), h('label', {}, 'New password (8+)', pw), h('label', {}, 'Role', role)),
    h('p', { class: 'muted small' }, 'Renaming keeps this user\'s sign-ins and API tokens working; the new name is needed at the next sign-in.'),
    accessBox, errEl,
    h('div', { class: 'row sticky-actions' }, h('button', {}, 'Save'), h('button', { type: 'button', class: 'ghost', onclick: () => close() }, 'Cancel'))));
  const close = () => modal.remove();
  closeOnEscape(modal, close);
  showAccess(); preview();
  document.body.append(modal);
}

// ---------------------------------------------------------------------------
// login / routing
// ---------------------------------------------------------------------------
function showLogin(setup = false) {
  $('#login').hidden = false;
  $('#login-hint').textContent = setup ? 'No users yet — create the first admin account. The setup token is printed in the server log at startup.' : '';
  $('#login-form').dataset.setup = setup ? '1' : '';
  $('#setup-token-row').hidden = !setup;
}
$('#login-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const f = new FormData(e.target);
  const body = { username: f.get('username'), password: f.get('password') };
  try {
    if (e.target.dataset.setup) await api.post('/api/setup', { ...body, setup_token: f.get('setup_token') });
    await api.post('/api/login', body);
    $('#login').hidden = true; $('#login-err').textContent = '';
    await boot();
  } catch (err) { $('#login-err').textContent = err.message; }
});
$('#logout').addEventListener('click', async () => { await api.post('/api/logout'); location.reload(); });
setInterval(() => { $('#clock').textContent = new Date().toLocaleString(); }, 1000);

// ---------------------------------------------------------------------------
// live notifications (SSE): toasts for new events and camera outages
// ---------------------------------------------------------------------------
function toast(text, { kind = '', onclick = null, ttl = 8000 } = {}) {
  let host = $('#toasts');
  if (!host) { host = h('div', { id: 'toasts' }); document.body.append(host); }
  const el = h('div', { class: 'toast ' + kind, onclick: () => { el.remove(); onclick?.(); } }, text);
  host.append(el);
  setTimeout(() => el.remove(), ttl);
}
let sse = null;
function listen() {
  if (sse) sse.close();
  sse = new EventSource('/api/events/stream');
  sse.addEventListener('event_start', (m) => {
    const e = JSON.parse(m.data);
    const what = e.objects?.length ? e.objects.map((o) => o.label).join(', ') : 'motion';
    toast(`${e.camera_name}: ${what}`, { kind: 'motion', onclick: () => { location.hash = `#/camera/${e.camera_id}/${e.start_ms}`; } });
  });
  sse.addEventListener('camera_down', (m) => { const e = JSON.parse(m.data); toast(`${e.name}: not recording`, { kind: 'bad', ttl: 30000 }); });
  sse.addEventListener('camera_up', (m) => { const e = JSON.parse(m.data); toast(`${e.name}: recording again`, { kind: 'ok' }); });
  sse.addEventListener('storage_low', (m) => { const e = JSON.parse(m.data); toast(`Storage ${e.path} low: ${gb(e.free_bytes)} free`, { kind: 'bad', ttl: 30000 }); });
}

async function boot() {
  try { state.me = await api.get('/api/me'); } catch { return; }
  if (state.me.setup_needed) { showLogin(true); return; }
  state.cameras = await api.get('/api/cameras');
  for (const pr of state.me.peers || []) {
    try { const remote = await api.get(`/api/peers/${pr}/api/cameras`); state.cameras.push(...remote.map((c) => ({ ...c, peer: pr }))); } catch (e) { toast(`Peer ${pr}: ${e.message}`, { kind: 'bad' }); }
  }
  document.querySelectorAll('.admin-only').forEach((el) => { el.hidden = state.me.user.role !== 'admin'; });
  listen();
  route();
}
async function route() {
  if (state.cleanup) { state.cleanup(); state.cleanup = null; }
  const parts = (location.hash || '#/live').slice(2).split('/');
  let view = parts[0] || 'live';
  if (view === 'montage') { location.hash = '#/review'; return; }
  if (view === 'review' && parts[1] && parts[2]) { location.hash = `#/camera/${parts[1]}/${parts[2]}`; return; } // old links
  document.querySelectorAll('#top nav a').forEach((a) => a.classList.toggle('active', a.dataset.view === view || (view === 'camera' && a.dataset.view === 'live')));
  const main = $('#main');
  try {
    if (view === 'live') await viewLive(main);
    else if (view === 'camera') await viewCamera(main, parts[1], parts[2] ? Number(parts[2]) : null);
    else if (view === 'review') await viewReview(main, parts[1] ? Number(parts[1]) : null);
    else if (view === 'events') await viewEvents(main);
    else if (view === 'status') await viewStatus(main);
    else if (view === 'admin') await viewAdmin(main);
    else main.replaceChildren(h('p', { class: 'muted' }, 'Unknown view.'));
  } catch (e) { main.replaceChildren(h('p', { class: 'err' }, e.message)); }
}
window.addEventListener('hashchange', route);
// PWA: cache the shell (never the API) so the app opens instantly on phones and works as a home-screen app
if ('serviceWorker' in navigator && location.protocol === 'https:') navigator.serviceWorker.register('/sw.js').catch(() => {});
boot();
