// ZoneMinder NG web UI. No build step, no framework: ES modules + fetch + MSE.
//
// Views
//   #/live            tile wall of every camera (snapshots or go2rtc WebRTC)
//   #/camera/<id>[/t] one camera: full-res live, its timeline, scrub previews, Live button
//   #/review          multi-camera synchronized playback with date/group/camera filters
//   #/events          motion events, instant paging, thumbnails
//   #/admin           cameras, users, storage/tiering

const $ = (sel, el = document) => el.querySelector(sel);
const h = (tag, attrs = {}, ...kids) => {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (v !== null && v !== undefined && v !== false) el.setAttribute(k, v === true ? '' : v);
  }
  for (const k of kids.flat()) if (k !== null && k !== undefined && k !== false) el.append(k.nodeType ? k : document.createTextNode(k));
  return el;
};
const fmtTime = (ms) => new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
const fmtDate = (ms) => new Date(ms).toLocaleDateString([], { month: 'short', day: 'numeric' });
const fmtDT = (ms) => `${fmtDate(ms)} ${fmtTime(ms)}`;
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
const camById = (id) => state.cameras.find((c) => c.id === Number(id));
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
function makeTimeline({ rows, range, onSeek, onRangeChange, fixedRange = false, tall = false }) {
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
      const tl = data.get(row.id); const y = 14 + i * rowH;
      ctx.fillStyle = '#2b3442'; if (tl) for (const [a, b] of tl.coverage) ctx.fillRect(xOf(a), y, Math.max(1, xOf(b) - xOf(a)), rowH - 1);
      if (tl) {
        const bw = Math.max(1, W / ((range.end - range.start) / tl.bucket_ms));
        for (let k = 0; k < tl.motion.length; k++) { const m = tl.motion[k]; if (!m) continue; const hh = rows.length === 1 ? (rowH - 1) * m / 255 : rowH - 1; ctx.fillStyle = m > 120 ? '#ff5a5f' : '#f9b84f'; ctx.fillRect(xOf(range.start + k * tl.bucket_ms), y + (rowH - 1 - hh), bw, hh); }
        ctx.fillStyle = 'rgba(79,156,249,.8)'; for (const ev of tl.events) ctx.fillRect(xOf(ev.start), y, Math.max(2, xOf(ev.end || ev.start) - xOf(ev.start)), 3);
      }
      if (rows.length > 1) { ctx.fillStyle = '#c8ced8'; ctx.font = '10px system-ui'; ctx.fillText(row.name, 3, y + Math.min(rowH - 2, 11)); }
    });
    ctx.fillStyle = '#8b93a1'; ctx.font = '11px system-ui';
    const span = range.end - range.start; const step = span > 3 * 86400000 ? 86400000 : span > 20 * 3600000 ? 4 * 3600000 : span > 6 * 3600000 ? 3600000 : span > 2 * 3600000 ? 1800000 : span > 3600000 ? 900000 : span > 1200000 ? 300000 : 60000;
    for (let t = Math.ceil(range.start / step) * step; t < range.end; t += step) { const x = xOf(t); ctx.fillRect(x, 0, 1, 10); ctx.fillText(step >= 86400000 ? fmtDate(t) : fmtTime(t).slice(0, 5), x + 3, 10); }
    cursor.style.left = `${xOf(playhead)}px`;
  }
  async function load() {
    const res = await Promise.all(rows.map((r) => api.get(`/api/cameras/${r.id}/timeline?start=${Math.round(range.start)}&end=${Math.round(range.end)}`).catch(() => null)));
    data = new Map(rows.map((r, i) => [r.id, res[i]]));
    draw();
  }
  function showPreview(ev) {
    const t = msAt(ev); const row = rowAt(ev);
    hover.style.left = `${xOf(t)}px`; tip.style.left = `${xOf(t)}px`; tip.textContent = fmtDT(t);
    preview.hidden = false; preview.style.left = `${Math.min(canvas.clientWidth - 170, Math.max(0, xOf(t) - 80))}px`;
    $('.ptime', preview).textContent = (rows.length > 1 ? row.name + ' · ' : '') + fmtTime(t);
    if (t > Date.now() - 3000) return;
    const key = `${row.id}:${Math.round(t / 2000)}`;
    clearTimeout(previewTimer);
    if (cache.has(key)) { $('img', preview).src = cache.get(key); return; }
    previewTimer = setTimeout(async () => {
      try {
        const r = await fetch(`/api/cameras/${row.id}/frame.jpg?t=${Math.round(t)}&width=320&stream=sub`);
        if (!r.ok) return;
        const url = URL.createObjectURL(await r.blob());
        cache.set(key, url); if (cache.size > 400) { const k = cache.keys().next().value; URL.revokeObjectURL(cache.get(k)); cache.delete(k); }
        if (!preview.hidden) $('img', preview).src = url;
      } catch {}
    }, dragging ? 60 : 120);
  }
  el.addEventListener('pointermove', (ev) => { showPreview(ev); if (dragging) { playhead = msAt(ev); cursor.style.left = `${xOf(playhead)}px`; } });
  el.addEventListener('pointerleave', () => { if (!dragging) { preview.hidden = true; tip.textContent = ''; } });
  el.addEventListener('pointerdown', (ev) => { dragging = true; el.setPointerCapture(ev.pointerId); showPreview(ev); });
  el.addEventListener('pointerup', (ev) => { if (!dragging) return; dragging = false; preview.hidden = true; onSeek(msAt(ev), rowAt(ev)); });
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
    setPlayhead(ms) { playhead = ms; cursor.style.left = `${xOf(ms)}px`; },
    setRange(s, e) { range.start = s; range.end = e; return load(); },
    get range() { return range; },
    destroy() { window.removeEventListener('resize', draw); for (const u of cache.values()) URL.revokeObjectURL(u); cache.clear(); },
  };
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
  const grid = h('div', { class: 'grid' });
  main.replaceChildren(toolbar, grid);
  const players = [];
  for (const c of shown) {
    const tile = h('div', { class: 'tile', onclick: () => { location.hash = `#/camera/${c.id}`; } },
      h('div', { class: 'label' }, c.name), h('div', { class: 'stat' }, statusText(c)), h('div', { class: 'motion' }));
    grid.append(tile);
    if (mode === 'sub' && support.h264Mse && c.sub_status?.connected) {
      const v = h('video', { muted: true, playsinline: true, autoplay: true }); tile.prepend(v);
      const p = new MsePlayer(v); const start = () => p.play(`/api/cameras/${c.id}/live.mp4?stream=sub`, { live: true }).catch(() => fallbackSnapshot(tile, c));
      p.onLiveEnded = () => setTimeout(start, 2000); start(); players.push(p);
    } else if (mode === 'webrtc' && state.me.go2rtc_url) {
      tile.prepend(h('iframe', { src: `${state.me.go2rtc_url}/stream.html?src=${encodeURIComponent(c.id + '_CameraDirectSecondary')}&mode=webrtc`, style: 'width:100%;height:100%;border:0;background:#000;pointer-events:none', allow: 'autoplay' }));
    } else fallbackSnapshot(tile, c);
  }
  const poll = setInterval(async () => {
    try {
      const st = await api.get('/api/stats');
      for (const s of st.cameras) {
        const i = shown.findIndex((c) => c.id === s.id); if (i < 0) continue;
        shown[i].status = s.status; shown[i].detect = s.detect;
        const tile = grid.children[i]; if (!tile) continue;
        $('.stat', tile).textContent = statusText(shown[i]);
        $('.motion', tile).style.transform = `scaleX(${(s.detect?.score || 0) / 255})`;
      }
    } catch {}
  }, 3000);
  state.cleanup = () => { clearInterval(poll); players.forEach((p) => p.stop()); grid.querySelectorAll('img[data-snap]').forEach((i) => clearInterval(i._t)); };
}
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
    next.src = `/api/cameras/${c.id}/snapshot.jpg?width=${width}&t=${Date.now()}`;
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
    h('a', { href: '#/live', class: 'muted' }, '◀ all cameras'), h('h2', { style: 'margin:0' }, cam.name),
    h('span', { class: 'pill ' + (cam.status?.connected ? 'ok' : 'bad') }, statusText(cam)), h('span', { class: 'grow' }), timeLabel, liveBtn);
  const tl = makeTimeline({ rows: [{ id: cam.id, name: cam.name }], range, onSeek: (t) => play(t), onRangeChange: () => {} });
  const controls = h('div', { class: 'controls' }, winSel,
    h('button', { class: 'ghost', onclick: () => shift(-windowMs / 2) }, '◀'), h('button', { class: 'ghost', onclick: () => shift(windowMs / 2) }, '▶'),
    h('button', { class: 'ghost', onclick: () => play(playhead - 10000) }, '−10 s'), h('button', { class: 'ghost', onclick: () => play(playhead + 10000) }, '+10 s'),
    h('span', { class: 'grow' }),
    h('button', { class: 'ghost', onclick: () => window.open(`/api/cameras/${cam.id}/video.mp4?start=${Math.round(playhead - 30000)}&end=${Math.round(playhead + 30000)}`) }, 'Export ±30 s'),
    h('button', { class: 'ghost', onclick: () => window.open(`/api/cameras/${cam.id}/frame.jpg?t=${Math.round(playhead)}&width=3840`) }, 'Full-res frame'),
    h('button', { class: 'ghost', onclick: () => { location.hash = '#/events'; localStorage.setItem('evCam', cam.id); } }, 'Events'),
    h('span', { class: 'muted small' }, 'drag the timeline to scrub · wheel to zoom · space pause · ←/→ 10 s'));
  main.replaceChildren(head, player, tl.el, controls);

  const mse = new MsePlayer(video);
  let playRange = null;
  function shift(d) { range.end = Math.min(Date.now(), range.end + d); range.start = range.end - windowMs; tl.load(); }
  async function goLive() {
    mode = 'live'; liveBadge.hidden = false; liveBtn.disabled = true; overlay.textContent = 'connecting…';
    const url = canPlayMain(cam) ? `/api/cameras/${cam.id}/live.mp4` : (support.h264Mse && cam.sub_status ? `/api/cameras/${cam.id}/live.mp4?stream=sub` : null);
    if (!url) { overlay.textContent = 'browser cannot decode this stream — snapshots'; snapshots(); return; }
    mse.onLiveEnded = () => { if (mode === 'live') setTimeout(goLive, 2000); };
    try { await mse.play(url, { live: true }); overlay.textContent = url.includes('stream=sub') ? 'live (substream)' : 'live'; }
    catch (e) { overlay.textContent = `${e.message} — snapshots`; snapshots(); }
  }
  let snapTimer = null;
  function snapshots() { const img = h('img', { style: 'width:100%;display:block' }); video.replaceWith(img); const load = () => { img.src = `/api/cameras/${cam.id}/snapshot.jpg?width=1920&t=${Date.now()}`; }; load(); snapTimer = setInterval(load, 1000); }
  async function play(ms) {
    mode = 'playback'; liveBadge.hidden = true; liveBtn.disabled = false; mse.onLiveEnded = null;
    playhead = Math.min(ms, Date.now() - 3000); tl.setPlayhead(playhead);
    const s = Math.max(playhead - 1500, 0), e = Math.min(Date.now(), s + CHUNK); playRange = [s, e];
    overlay.textContent = 'loading…';
    try {
      const url = canPlayMain(cam) ? `/api/cameras/${cam.id}/video.mp4?start=${Math.round(s)}&end=${Math.round(e)}` : `/api/cameras/${cam.id}/video.mp4?stream=sub&start=${Math.round(s)}&end=${Math.round(e)}`;
      if (support.mse) await mse.play(url, { startMs: s });
      else if (support.nativeHls) { video.src = `/api/cameras/${cam.id}/playlist.m3u8?start=${Math.round(s)}&end=${Math.round(e)}`; video.play(); }
    } catch (err) { overlay.textContent = err.message; }
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
  };
  document.addEventListener('keydown', keys);
  await tl.load();
  if (mode === 'live') goLive(); else play(playhead);
  const tick = setInterval(() => {
    if (mode === 'live') { playhead = Date.now(); tl.setPlayhead(playhead); timeLabel.textContent = fmtDT(playhead); }
    if (range.end >= Date.now() - 20000) { range.end = Date.now(); range.start = range.end - windowMs; tl.load(); }
  }, 15000);
  state.cleanup = () => { clearInterval(tick); clearInterval(snapTimer); mse.stop(); tl.destroy(); document.removeEventListener('keydown', keys); };
}

// ---------------------------------------------------------------------------
// Review: filters (date range, groups, cameras) + synchronized multi-camera
// playback of the substreams on a shared timeline with per-row previews.
// ---------------------------------------------------------------------------
async function viewReview(main, atMs) {
  const cams = state.cameras.filter((c) => c.sub_status || c.status?.connected);
  const now = Date.now();
  const saved = JSON.parse(localStorage.getItem('reviewFilter') || '{}');
  const f = { from: saved.from || now - 3600000, to: saved.to || now, groups: saved.groups || [], cameras: saved.cameras || [], motionOnly: saved.motionOnly || false };
  if (f.to > now) f.to = now;
  if (atMs) { f.from = atMs - 1800000; f.to = Math.min(now, atMs + 1800000); }

  const fromIn = h('input', { type: 'datetime-local', value: toLocalInput(f.from) });
  const toIn = h('input', { type: 'datetime-local', value: toLocalInput(f.to) });
  const quick = h('select', { onchange: (e) => { const m = Number(e.target.value); if (!m) return; toIn.value = toLocalInput(Date.now()); fromIn.value = toLocalInput(Date.now() - m); } },
    h('option', { value: 0 }, 'Quick range…'), ...[[900000, 'last 15 min'], [3600000, 'last hour'], [6 * 3600000, 'last 6 h'], [86400000, 'last 24 h'], [7 * 86400000, 'last 7 days']].map(([v, l]) => h('option', { value: v }, l)));
  const groupBoxes = allGroups().map((g) => h('label', { class: 'chip' }, h('input', { type: 'checkbox', value: g, checked: f.groups.includes(g), onchange: syncCamsFromGroups }), g));
  const camBoxes = cams.map((c) => h('label', { class: 'chip' }, h('input', { type: 'checkbox', value: c.id, checked: f.cameras.length ? f.cameras.includes(c.id) : true }), c.name));
  const motionOnly = h('input', { type: 'checkbox', checked: f.motionOnly });
  function syncCamsFromGroups() {
    const gs = groupBoxes.filter((b) => $('input', b).checked).map((b) => $('input', b).value);
    if (!gs.length) return;
    camBoxes.forEach((b) => { const c = camById($('input', b).value); $('input', b).checked = groupsOf(c).some((g) => gs.includes(g)); });
  }
  const filters = h('div', { class: 'card filters' },
    h('div', { class: 'row' }, h('label', {}, 'From', fromIn), h('label', {}, 'To', toIn), h('label', {}, ' ', quick),
      h('label', { class: 'row', style: 'align-self:end' }, motionOnly, ' only cameras with motion in range'),
      h('span', { class: 'grow' }), h('button', { onclick: apply }, 'Apply')),
    groupBoxes.length ? h('div', { class: 'row' }, h('span', { class: 'muted' }, 'Groups:'), ...groupBoxes) : null,
    h('div', { class: 'row' }, h('span', { class: 'muted' }, 'Cameras:'), h('button', { class: 'ghost small', onclick: () => camBoxes.forEach((b) => { $('input', b).checked = true; }) }, 'all'), h('button', { class: 'ghost small', onclick: () => camBoxes.forEach((b) => { $('input', b).checked = false; }) }, 'none'), ...camBoxes));
  const stage = h('div');
  main.replaceChildren(filters, stage);
  let session = null;
  async function apply() {
    const from = new Date(fromIn.value).getTime(), to = new Date(toIn.value).getTime();
    if (!(from < to)) { alert('From must be before To'); return; }
    let chosen = camBoxes.filter((b) => $('input', b).checked).map((b) => camById($('input', b).value)).filter(Boolean);
    localStorage.setItem('reviewFilter', JSON.stringify({ from, to, groups: groupBoxes.filter((b) => $('input', b).checked).map((b) => $('input', b).value), cameras: chosen.map((c) => c.id), motionOnly: motionOnly.checked }));
    if (motionOnly.checked) {
      const res = await Promise.all(chosen.map((c) => api.get(`/api/cameras/${c.id}/timeline?start=${Math.round(from)}&end=${Math.round(to)}`).catch(() => null)));
      chosen = chosen.filter((c, i) => res[i] && (res[i].events.length || res[i].motion.some((m) => m > 0)));
    }
    session?.destroy();
    session = reviewSession(stage, chosen, { start: from, end: Math.min(to, Date.now()) }, atMs && atMs > from && atMs < to ? atMs : from);
    atMs = null;
  }
  await apply();
  state.cleanup = () => session?.destroy();
}

function reviewSession(stage, cams, range, startAt) {
  if (!cams.length) { stage.replaceChildren(h('p', { class: 'muted' }, 'No cameras selected (or none with motion in this range).')); return { destroy() {} }; }
  const CHUNK = 2 * 60 * 1000;
  let playhead = startAt, paused = false;
  const grid = h('div', { class: 'grid montage' });
  const players = cams.map((c) => {
    const v = h('video', { muted: true, playsinline: true });
    const tile = h('div', { class: 'tile', ondblclick: () => { location.hash = `#/camera/${c.id}/${Math.round(playhead)}`; } }, v, h('div', { class: 'label' }, c.name), h('div', { class: 'stat' }, ''));
    grid.append(tile);
    return { cam: c, video: v, mse: new MsePlayer(v), stat: $('.stat', tile) };
  });
  const tl = makeTimeline({ rows: cams.map((c) => ({ id: c.id, name: c.name })), range, fixedRange: true, tall: cams.length > 3, onSeek: (t) => seekAll(t) });
  const timeLabel = h('span', { class: 'time' });
  const playBtn = h('button', { class: 'ghost', onclick: () => { paused = !paused; playBtn.textContent = paused ? '▶' : '⏸'; players.forEach((p) => paused ? p.video.pause() : p.video.play().catch(() => {})); } }, '⏸');
  const speedSel = h('select', { style: 'width:90px', onchange: (e) => players.forEach((p) => { p.video.playbackRate = Number(e.target.value); }) }, ...[1, 2, 4, 8].map((r) => h('option', { value: r }, `${r}×`)));
  const controls = h('div', { class: 'controls' }, playBtn, speedSel,
    h('button', { class: 'ghost', onclick: () => seekAll(playhead - 60000) }, '−1 min'), h('button', { class: 'ghost', onclick: () => seekAll(playhead + 60000) }, '+1 min'),
    timeLabel, h('span', { class: 'grow' }), h('span', { class: 'muted small' }, `${cams.length} cameras · ${fmtDT(range.start)} → ${fmtDT(range.end)} · drag to scrub · double-click a tile for full resolution`));
  stage.replaceChildren(grid, tl.el, controls);
  async function seekAll(ms) {
    playhead = Math.max(range.start, Math.min(ms, range.end - 1000)); tl.setPlayhead(playhead);
    const s = playhead, e = Math.min(range.end, s + CHUNK);
    await Promise.all(players.map(async (p) => {
      p.stat.textContent = '…';
      try {
        const stream = p.cam.sub_status ? 'sub' : 'main';
        await p.mse.play(`/api/cameras/${p.cam.id}/video.mp4?stream=${stream}&start=${Math.round(s)}&end=${Math.round(e)}`, { startMs: s });
        p.stat.textContent = ''; p.video.playbackRate = Number(speedSel.value); if (paused) p.video.pause();
      } catch { p.stat.textContent = 'no recording'; }
    }));
  }
  const sync = setInterval(() => {
    const lead = players.find((p) => p.mse.wallMs() !== null && !p.video.paused && p.video.readyState >= 2);
    if (!lead) return;
    const t = lead.mse.wallMs(); playhead = t; timeLabel.textContent = fmtDT(t); tl.setPlayhead(t);
    for (const p of players) { if (p === lead) continue; const w = p.mse.wallMs(); if (w === null) continue; const d = (t - w) / 1000; if (Math.abs(d) > 0.3) p.video.currentTime += d; }
  }, 1000);
  players[0].video.addEventListener('ended', () => { if (playhead + 2000 < range.end) seekAll(playhead + 500); });
  tl.load().then(() => seekAll(startAt));
  return { destroy() { clearInterval(sync); players.forEach((p) => p.mse.stop()); tl.destroy(); } };
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------
async function viewEvents(main) {
  const cams = state.cameras;
  const q = { camera: localStorage.getItem('evCam') || '', min_score: Number(localStorage.getItem('evMin') || 0), archived: false, range: localStorage.getItem('evRange') || '7d' };
  const list = h('div', { class: 'events' });
  const more = h('button', { class: 'ghost' }, 'Load more');
  const count = h('span', { class: 'muted' });
  const camSel = h('select', { onchange: (e) => { q.camera = e.target.value; localStorage.setItem('evCam', q.camera); load(true); } },
    h('option', { value: '' }, 'All cameras'), ...allGroups().map((g) => h('option', { value: 'g:' + g, selected: 'g:' + g === q.camera }, `Group: ${g}`)), ...cams.map((c) => h('option', { value: c.id, selected: String(c.id) === q.camera }, c.name)));
  const minSel = h('select', { onchange: (e) => { q.min_score = Number(e.target.value); localStorage.setItem('evMin', q.min_score); load(true); } },
    ...[[0, 'Any motion'], [60, 'Moderate+'], [120, 'Strong+'], [200, 'Very strong']].map(([v, l]) => h('option', { value: v, selected: v === q.min_score }, l)));
  const rangeSel = h('select', { onchange: (e) => { q.range = e.target.value; localStorage.setItem('evRange', q.range); load(true); } },
    ...[['1d', 'Last 24 h'], ['7d', 'Last 7 days'], ['30d', 'Last 30 days'], ['all', 'All']].map(([v, l]) => h('option', { value: v, selected: v === q.range }, l)));
  const arch = h('label', { class: 'row' }, h('input', { type: 'checkbox', style: 'width:auto', onchange: (e) => { q.archived = e.target.checked; load(true); } }), ' Archived only');
  main.replaceChildren(h('div', { class: 'toolbar' }, camSel, minSel, rangeSel, arch, h('span', { class: 'grow' }), count), list, h('div', { class: 'row', style: 'margin-top:12px' }, more));
  let before = null, total = 0;
  async function load(reset) {
    if (reset) { list.replaceChildren(); before = null; total = 0; }
    const days = { '1d': 1, '7d': 7, '30d': 30 }[q.range];
    const p = new URLSearchParams({ limit: 60 });
    if (q.camera.startsWith('g:')) p.set('camera', cams.filter((c) => groupsOf(c).includes(q.camera.slice(2))).map((c) => c.id).join(',') || '0');
    else if (q.camera) p.set('camera', q.camera);
    if (q.min_score) p.set('min_score', q.min_score);
    if (q.archived) p.set('archived', 'true');
    if (days) p.set('start', Date.now() - days * 86400000);
    if (before) p.set('before', before);
    const t0 = performance.now();
    const evs = await api.get(`/api/events?${p}`);
    total += evs.length; before = evs.length ? evs[evs.length - 1].id : before;
    more.disabled = evs.length < 60;
    count.textContent = `${total} events (${(performance.now() - t0).toFixed(0)} ms)`;
    for (const ev of evs) list.append(eventCard(ev));
  }
  more.onclick = () => load(false);
  await load(true);
}
function eventCard(ev) {
  const cam = camById(ev.camera_id);
  return h('div', { class: 'event' + (ev.archived ? ' archived' : ''), onclick: () => openEvent(ev) },
    h('div', { class: 'thumb', style: ev.thumb ? `background-image:url(${ev.thumb})` : '' }, h('span', { class: 'score' + (ev.score > 120 ? ' hot' : '') }, `${ev.score}`)),
    h('div', { class: 'meta' }, h('b', {}, cam?.name || `camera ${ev.camera_id}`), `${fmtDT(ev.start)} · ${ev.end ? fmtDur(ev.end - ev.start) : 'in progress'}`, ev.notes ? h('div', { class: 'muted' }, ev.notes) : null));
}
function openEvent(ev) {
  const cam = camById(ev.camera_id);
  const video = h('video', { controls: true, autoplay: true, playsinline: true, muted: true, style: 'width:100%;background:#000;border-radius:8px' });
  const notes = h('textarea', { rows: 2, placeholder: 'Notes' }, ev.notes || '');
  const archBtn = h('button', { class: 'ghost', onclick: async () => { ev.archived = !ev.archived; await api.patch(`/api/events/${ev.id}`, { archived: ev.archived }); archBtn.textContent = ev.archived ? '★ Archived' : '☆ Archive'; } }, ev.archived ? '★ Archived' : '☆ Archive');
  const modal = h('div', { class: 'modal', onclick: (e) => { if (e.target === modal) close(); } },
    h('div', { class: 'card' },
      h('div', { class: 'row' }, h('h2', {}, `${cam?.name || ''} — ${fmtDT(ev.start)}`), h('span', { class: 'grow' }), h('button', { class: 'ghost small', onclick: close }, '✕')),
      video,
      h('div', { class: 'row', style: 'margin-top:8px' }, archBtn,
        h('button', { class: 'ghost', onclick: () => { close(); location.hash = `#/camera/${ev.camera_id}/${ev.peak || ev.start}`; } }, 'Open camera timeline'),
        h('button', { class: 'ghost', onclick: () => { close(); location.hash = `#/review/${ev.peak || ev.start}`; } }, 'Review all cameras at this time'),
        h('a', { class: 'muted', href: `/api/cameras/${ev.camera_id}/video.mp4?start=${ev.start}&end=${ev.end || ev.start + 60000}`, download: `event-${ev.id}.mp4`, onclick: (e) => e.stopPropagation() }, 'Download'),
        h('span', { class: 'muted' }, `peak ${ev.score}`)),
      h('label', {}, notes),
      h('button', { class: 'small', onclick: async () => { await api.patch(`/api/events/${ev.id}`, { notes: notes.value }); ev.notes = notes.value; } }, 'Save notes')));
  document.body.append(modal);
  const mse = new MsePlayer(video);
  const stream = cam && canPlayMain(cam) ? 'main' : 'sub';
  const url = `/api/cameras/${ev.camera_id}/video.mp4?stream=${stream}&start=${ev.start}&end=${ev.end || ev.start + 60000}`;
  if (support.mse) mse.play(url, { startMs: ev.start }).catch((e) => { video.replaceWith(h('p', { class: 'err' }, e.message)); });
  else video.src = url;
  function close() { mse.stop(); modal.remove(); }
}

// ---------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------
async function viewAdmin(main) {
  if (state.me.user.role !== 'admin') { main.replaceChildren(h('p', { class: 'muted' }, 'Admins only.')); return; }
  const [cams, users, storages] = await Promise.all([api.get('/api/cameras'), api.get('/api/users'), api.get('/api/storages')]);
  const pill = (st, label) => h('span', { class: 'pill ' + (st?.connected ? 'ok' : 'bad') }, st?.connected ? label : (st?.last_error || 'offline'));
  const camTable = h('table', {}, h('tr', {}, h('th', {}, 'Id'), h('th', {}, 'Name'), h('th', {}, 'Groups'), h('th', {}, 'Status'), h('th', {}, 'Storage'), h('th', {}, 'Retention'), h('th', {}, 'Detect'), h('th', {}, '')),
    ...cams.map((c) => h('tr', {},
      h('td', {}, c.id), h('td', {}, c.name), h('td', {}, c.tags),
      h('td', {}, pill(c.status, `${c.status.codec} ${c.status.width}×${c.status.height} ${c.status.fps.toFixed(0)}fps`), ' ', c.sub_status ? pill(c.sub_status, `sub ${c.sub_status.width}×${c.sub_status.height}`) : null),
      h('td', {}, c.storage_id), h('td', {}, `${c.retention_days} d${c.event_retention_days ? ` / events ${c.event_retention_days} d` : ''}`),
      h('td', {}, c.detect ? `${c.detect.fps.toFixed(1)} fps, score ${c.detect.score}` : '—'),
      h('td', {}, h('button', { class: 'ghost small', onclick: () => editCamera(c) }, 'Edit'), ' ', h('button', { class: 'ghost small', onclick: async () => { if (confirm(`Delete camera ${c.name} and all its recordings?`)) { await api.del(`/api/cameras/${c.id}`); route(); } } }, 'Delete')))));
  const addCam = h('form', { class: 'row', onsubmit: async (e) => { e.preventDefault(); const f = new FormData(e.target); await api.post('/api/cameras', { name: f.get('name'), main_url: f.get('main_url'), sub_url: f.get('sub_url') || null }); route(); } },
    h('input', { name: 'name', placeholder: 'Name', required: true, style: 'width:160px' }), h('input', { name: 'main_url', placeholder: 'rtsp://user:pass@ip/Streaming/Channels/101', required: true, style: 'flex:2;min-width:240px' }), h('input', { name: 'sub_url', placeholder: 'substream rtsp:// (optional)', style: 'flex:2;min-width:240px' }), h('button', {}, 'Add camera'));
  const userTable = h('table', {}, h('tr', {}, h('th', {}, 'User'), h('th', {}, 'Role'), h('th', {}, 'Cameras'), h('th', {}, '')),
    ...users.map((u) => h('tr', {}, h('td', {}, u.username), h('td', {}, u.role), h('td', {}, u.role === 'admin' ? 'all' : u.cameras.map((id) => cams.find((c) => c.id === id)?.name || id).join(', ') || h('span', { class: 'err' }, 'none')),
      h('td', {}, h('button', { class: 'ghost small', onclick: () => editUserCams(u, cams) }, 'Cameras'), ' ', h('button', { class: 'ghost small', onclick: async () => { if (confirm(`Delete user ${u.username}?`)) { await api.del(`/api/users/${u.id}`); route(); } } }, 'Delete')))));
  const addUser = h('form', { class: 'row', onsubmit: async (e) => { e.preventDefault(); const f = new FormData(e.target); await api.post('/api/users', { username: f.get('username'), password: f.get('password'), role: f.get('role') }); route(); } },
    h('input', { name: 'username', placeholder: 'username', required: true, style: 'width:160px' }), h('input', { name: 'password', type: 'password', placeholder: 'password (8+)', required: true, style: 'width:180px' }), h('select', { name: 'role', style: 'width:120px' }, h('option', { value: 'viewer' }, 'viewer'), h('option', { value: 'admin' }, 'admin')), h('button', {}, 'Add user'));
  const stor = h('table', {}, h('tr', {}, h('th', {}, 'Id'), h('th', {}, 'Path'), h('th', {}, 'Used'), h('th', {}, 'Free'), h('th', {}, 'Cap'), h('th', {}, 'Reserve'), h('th', {}, 'Archive to'), h('th', {}, '')),
    ...storages.map((s) => h('tr', {}, h('td', {}, s.id), h('td', {}, h('code', {}, s.path), s.available ? '' : h('span', { class: 'pill bad' }, 'missing')), h('td', {}, gb(s.used_bytes)), h('td', {}, gb(s.free_bytes)), h('td', {}, s.max_bytes ? gb(s.max_bytes) : '—'), h('td', {}, gb(s.reserve_bytes)),
      h('td', {}, s.archive_to ? `storage ${s.archive_to} after ${s.archive_after_days ?? '—'} d @ ${s.archive_rate_mbps} Mb/s` : '—'),
      h('td', {}, h('button', { class: 'ghost small', onclick: () => editStorage(s, storages) }, 'Edit')))));
  const tokenBtn = h('button', { class: 'ghost small', onclick: async () => { const t = await api.post('/api/tokens'); prompt('Long-lived API token (Home Assistant etc.). Shown once:', t.token); } }, 'Create API token');
  main.replaceChildren(h('div', { class: 'two' },
    h('div', { class: 'card', style: 'grid-column:1/-1' }, h('h2', {}, 'Cameras'), camTable, h('div', { style: 'margin-top:10px' }, addCam)),
    h('div', { class: 'card' }, h('h2', {}, 'Users'), userTable, h('div', { style: 'margin-top:10px' }, addUser), h('p', { class: 'muted' }, 'Viewers see only the cameras assigned to them (fails closed). '), tokenBtn),
    h('div', { class: 'card' }, h('h2', {}, 'Storage'), stor, h('p', { class: 'muted' }, 'Retention deletes whole segments, oldest first: per-camera age limit (events kept longer), then per-volume space budget. Tiering copies old segments to an archive volume first.')),
    h('div', { class: 'card' }, h('h2', {}, 'Browser'), h('p', {}, `MSE: ${support.mse} · HEVC via MSE: ${support.hevcMse} · H.264 via MSE: ${support.h264Mse} · native HLS: ${support.nativeHls}`), h('p', { class: 'muted' }, 'zmNinjaNg: use this server\'s address plus /zm as the portal URL.'))));
}
const gb = (b) => `${(b / 1e9).toFixed(1)} GB`;
function editCamera(c) {
  const fields = [['name', 'Name'], ['tags', 'Groups (comma separated, e.g. Outside, CBA)'], ['main_url', 'Main RTSP URL'], ['sub_url', 'Substream RTSP URL'], ['retention_days', 'Retention (days)'], ['event_retention_days', 'Keep footage with events (days, 0 = same)'], ['detect_fps', 'Detect fps'], ['detect_width', 'Detect width'], ['detect_height', 'Detect height'], ['pixel_threshold', 'Pixel threshold (0-255)'], ['min_area_pct', 'Min changed area %'], ['min_blob_pct', 'Min blob %'], ['pre_secs', 'Pre-roll s'], ['post_secs', 'Post-roll s'], ['cooldown_secs', 'Cooldown s'], ['zones_json', 'Zones JSON [[[x,y],...]] (0..1)'], ['masks_json', 'Masks JSON'], ['sort_order', 'Sort order']];
  const strings = ['name', 'tags', 'main_url', 'sub_url', 'zones_json', 'masks_json'];
  const inputs = {};
  const form = h('form', { onsubmit: async (e) => { e.preventDefault(); const patch = {}; for (const [k] of fields) { let v = inputs[k].value; if (v === '' && k === 'sub_url') v = null; else if (!strings.includes(k)) v = Number(v); if (v !== c[k]) patch[k] = v; } patch.enabled = inputs.enabled.checked; patch.record_sub = inputs.record_sub.checked; try { await api.patch(`/api/cameras/${c.id}`, patch); modal.remove(); state.cameras = await api.get('/api/cameras'); route(); } catch (err) { errEl.textContent = err.message; } } },
    ...fields.map(([k, l]) => h('label', {}, l, inputs[k] = h('input', { value: c[k] ?? '' }))),
    h('label', { class: 'row' }, inputs.enabled = h('input', { type: 'checkbox', style: 'width:auto', checked: c.enabled }), ' Enabled'),
    h('label', { class: 'row' }, inputs.record_sub = h('input', { type: 'checkbox', style: 'width:auto', checked: c.record_sub }), ' Record substream too (review, phones, previews)'),
    h('div', { class: 'row' }, h('button', {}, 'Save'), h('button', { type: 'button', class: 'ghost', onclick: () => modal.remove() }, 'Cancel')));
  const errEl = h('p', { class: 'err' });
  const modal = h('div', { class: 'modal' }, h('div', { class: 'card' }, h('h2', {}, `Camera ${c.id}`), form, errEl));
  document.body.append(modal);
}
function editStorage(s, all) {
  const inputs = {};
  const form = h('form', { onsubmit: async (e) => { e.preventDefault(); const patch = { max_bytes: inputs.max_gb.value === '' ? null : Math.round(Number(inputs.max_gb.value) * 1e9), reserve_bytes: Math.round(Number(inputs.reserve_gb.value) * 1e9), archive_to: inputs.archive_to.value === '' ? null : Number(inputs.archive_to.value), archive_after_days: inputs.archive_after_days.value === '' ? null : Number(inputs.archive_after_days.value), archive_rate_mbps: Number(inputs.archive_rate_mbps.value) }; try { await api.patch(`/api/storages/${s.id}`, patch); modal.remove(); route(); } catch (err) { errEl.textContent = err.message; } } },
    h('label', {}, 'Cap (GB, blank = use free-space reserve only)', inputs.max_gb = h('input', { value: s.max_bytes ? (s.max_bytes / 1e9).toFixed(0) : '' })),
    h('label', {}, 'Reserve free (GB)', inputs.reserve_gb = h('input', { value: (s.reserve_bytes / 1e9).toFixed(0) })),
    h('label', {}, 'Archive to (tiering)', inputs.archive_to = h('select', {}, h('option', { value: '' }, 'never (delete here by retention)'), ...all.filter((o) => o.id !== s.id).map((o) => h('option', { value: o.id, selected: o.id === s.archive_to }, `storage ${o.id}: ${o.path}`)))),
    h('label', {}, 'Move segments older than (days)', inputs.archive_after_days = h('input', { value: s.archive_after_days ?? '' })),
    h('label', {}, 'Copy rate limit (Mb/s)', inputs.archive_rate_mbps = h('input', { value: s.archive_rate_mbps })),
    h('p', { class: 'muted' }, 'Recent footage of every camera stays here; older segments are copied to the archive volume (checksummed), then removed here. If the archive volume is missing, nothing is moved and recording continues.'),
    h('div', { class: 'row' }, h('button', {}, 'Save'), h('button', { type: 'button', class: 'ghost', onclick: () => modal.remove() }, 'Cancel')));
  const errEl = h('p', { class: 'err' });
  const modal = h('div', { class: 'modal' }, h('div', { class: 'card' }, h('h2', {}, `Storage ${s.id}`), form, errEl));
  document.body.append(modal);
}
function editUserCams(u, cams) {
  const boxes = cams.map((c) => h('label', { class: 'row' }, h('input', { type: 'checkbox', style: 'width:auto', value: c.id, checked: u.cameras.includes(c.id) }), ` ${c.name}`));
  const modal = h('div', { class: 'modal' }, h('div', { class: 'card' }, h('h2', {}, `Cameras for ${u.username}`), ...boxes,
    h('div', { class: 'row' }, h('button', { onclick: async () => { const ids = boxes.map((b) => $('input', b)).filter((i) => i.checked).map((i) => Number(i.value)); await api.patch(`/api/users/${u.id}`, { cameras: ids }); modal.remove(); route(); } }, 'Save'), h('button', { class: 'ghost', onclick: () => modal.remove() }, 'Cancel'))));
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

async function boot() {
  try { state.me = await api.get('/api/me'); } catch { return; }
  if (state.me.setup_needed) { showLogin(true); return; }
  state.cameras = await api.get('/api/cameras');
  document.querySelectorAll('.admin-only').forEach((el) => { el.hidden = state.me.user.role !== 'admin'; });
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
    else if (view === 'admin') await viewAdmin(main);
    else main.replaceChildren(h('p', { class: 'muted' }, 'Unknown view.'));
  } catch (e) { main.replaceChildren(h('p', { class: 'err' }, e.message)); }
}
window.addEventListener('hashchange', route);
boot();
