// ZoneMinder NG web UI. No build step, no framework: ES modules + fetch + MSE.
// Views: live grid (MSE full-res / go2rtc WebRTC), review (timeline scrub +
// range playback), events (instant paged list with thumbnails), admin.

const $ = (sel, el = document) => el.querySelector(sel);
const h = (tag, attrs = {}, ...kids) => {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'html') el.innerHTML = v;
    else if (v !== null && v !== undefined) el.setAttribute(k, v);
  }
  for (const k of kids.flat()) if (k !== null && k !== undefined) el.append(k.nodeType ? k : document.createTextNode(k));
  return el;
};
const fmtTime = (ms) => new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
const fmtDate = (ms) => new Date(ms).toLocaleDateString([], { month: 'short', day: 'numeric' });
const fmtDT = (ms) => `${fmtDate(ms)} ${fmtTime(ms)}`;
const fmtDur = (ms) => { const s = Math.round(ms / 1000); return s >= 3600 ? `${Math.floor(s / 3600)}h ${Math.floor(s % 3600 / 60)}m` : s >= 60 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${s}s`; };

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

const state = { me: null, cameras: [], view: null, cleanup: null };

// ---------------------------------------------------------------------------
// codec support probe
// ---------------------------------------------------------------------------
const support = {
  mse: !!window.MediaSource,
  hevcMse: !!window.MediaSource && ['video/mp4; codecs="hvc1.1.6.L153.B0"', 'video/mp4; codecs="hev1.1.6.L153.B0"'].some((c) => MediaSource.isTypeSupported(c)),
  h264Mse: !!window.MediaSource && MediaSource.isTypeSupported('video/mp4; codecs="avc1.640028"'),
  nativeHls: (() => { const v = document.createElement('video'); return v.canPlayType('application/vnd.apple.mpegurl') !== ''; })(),
};

// ---------------------------------------------------------------------------
// MSE player for our fMP4 streams (live or range). Absolute tfdt: we shift
// with timestampOffset so the media timeline starts at 0.
// ---------------------------------------------------------------------------
class MsePlayer {
  constructor(video) { this.video = video; this.abort = null; this.ms = null; this.sb = null; this.queue = []; this.mime = null; this.baseSec = null; this.ended = false; }
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
    const sb = ms.addSourceBuffer(this.mime); this.sb = sb;
    sb.mode = 'segments';
    sb.timestampOffset = -this.baseSec;
    sb.addEventListener('updateend', () => this.pump());
    const reader = res.body.getReader();
    (async () => {
      try {
        for (;;) {
          const { value, done } = await reader.read();
          if (done) { if (live && this.onLiveEnded) this.onLiveEnded(); this.ended = true; this.pump(); break; }
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
          // evict everything more than 20 s behind the playhead, then retry on updateend
          const b = sb.buffered; const t = this.video.currentTime;
          if (b.length && t - b.start(0) > 25) { try { sb.remove(b.start(0), t - 20); return; } catch {} }
          setTimeout(() => this.pump(), 200);
        } else { console.warn('append', e); this.queue.shift(); }
      }
    }
    else if (this.ended) { try { this.ms.endOfStream(); } catch {} this.ended = false; }
  }
  trimLive() {
    // keep the live edge close and the buffer small
    const v = this.video, sb = this.sb; if (!sb || sb.updating) return;
    const b = sb.buffered; if (!b.length) return;
    const end = b.end(b.length - 1);
    if (end - v.currentTime > 4) v.currentTime = end - 0.5;
    if (b.start(0) < end - 30) { try { sb.remove(0, end - 20); } catch {} }
  }
  /// absolute wall time (ms) of the playhead
  wallMs() { return this.baseSec === null ? null : (this.baseSec + this.video.currentTime) * 1000; }
  stop() {
    if (this.abort) this.abort.abort(); this.abort = null;
    this.queue = []; this.ended = false;
    if (this.ms && this.ms.readyState === 'open') { try { this.ms.endOfStream(); } catch {} }
    this.ms = null; this.sb = null;
    if (this.video.src) { URL.revokeObjectURL(this.video.src); this.video.removeAttribute('src'); this.video.load(); }
  }
}

// ---------------------------------------------------------------------------
// Live view
// ---------------------------------------------------------------------------
async function viewLive(main, focusId) {
  const cams = state.cameras;
  // Grid tiles never use full-resolution streams: 23 x 4 MP HEVC decodes would
  // exceed any client GPU and HTTP/1.1 allows only ~6 streaming fetches per
  // origin. Tiles are snapshots (or go2rtc WebRTC substreams); click a tile
  // for the full-resolution MSE stream of that one camera.
  const mode = localStorage.getItem('liveMode') || 'snapshot';
  const focus = focusId ? cams.find((c) => c.id === Number(focusId)) : null;
  const toolbar = h('div', { class: 'toolbar' },
    h('span', { class: 'muted' }, focus ? h('a', { href: '#/live' }, '◀ all cameras') : `${cams.length} cameras`),
    h('span', { class: 'grow' }),
    focus ? null : h('label', { class: 'row' }, 'Tiles ', h('select', { onchange: (e) => { localStorage.setItem('liveMode', e.target.value); route(); } },
      h('option', { value: 'snapshot', selected: mode === 'snapshot' ? '' : null }, 'Snapshots every 2 s'),
      h('option', { value: 'webrtc', selected: mode === 'webrtc' ? '' : null, disabled: state.me.go2rtc_url ? null : '' }, 'go2rtc WebRTC substream (low latency)'),
    )),
  );
  const grid = h('div', { class: 'grid' + (focus ? ' focus' : '') });
  main.replaceChildren(toolbar, grid);
  const players = [];
  for (const c of (focus ? [focus] : cams)) {
    const tile = h('div', { class: 'tile' });
    const label = h('div', { class: 'label' }, c.name);
    const stat = h('div', { class: 'stat' }, statusText(c));
    const motion = h('div', { class: 'motion' });
    tile.append(label, stat, motion);
    if (focus) {
      tile.addEventListener('dblclick', () => { location.hash = `#/review/${c.id}`; });
      const canMse = support.mse && (c.status.codec?.startsWith('avc') ? support.h264Mse : support.hevcMse);
      if (canMse && c.status.connected) {
        const v = h('video', { muted: '', playsinline: '', autoplay: '', controls: '' });
        tile.prepend(v);
        const p = new MsePlayer(v);
        const start = () => p.play(`/api/cameras/${c.id}/live.mp4`, { live: true }).catch((e) => { stat.textContent = `${e.message} — showing snapshots`; fallbackSnapshot(tile, c); });
        p.onLiveEnded = () => setTimeout(start, 2000); // recorder reconnected: resubscribe
        start();
        players.push(p);
      } else {
        stat.textContent = (c.status.connected ? 'browser cannot decode this codec — snapshots' : 'offline');
        fallbackSnapshot(tile, c, 1000);
      }
      grid.append(tile);
      grid.append(h('p', { class: 'muted' }, 'Full-resolution live stream decoded by your browser (MSE). Double-click for the timeline.'));
      continue;
    }
    tile.addEventListener('click', () => { location.hash = `#/live/${c.id}`; });
    grid.append(tile);
    if (mode === 'webrtc' && state.me.go2rtc_url) {
      // go2rtc's own WebRTC player (H.264 substream). NOTE: this exposes go2rtc's port to the browser;
      // keep go2rtc on the LAN only or proxy it behind zmng auth (see ARCHITECTURE.md §3.6).
      const src = `${state.me.go2rtc_url}/stream.html?src=${encodeURIComponent(c.id + '_CameraDirectSecondary')}&mode=webrtc`;
      const f = h('iframe', { src, style: 'width:100%;height:100%;border:0;background:#000;pointer-events:none', allow: 'autoplay' });
      tile.prepend(f);
    } else {
      fallbackSnapshot(tile, c);
    }
  }
  const poll = setInterval(async () => {
    try {
      const st = await api.get('/api/stats');
      const shown = focus ? [focus] : cams;
      for (const s of st.cameras) {
        const i = shown.findIndex((c) => c.id === s.id); if (i < 0) continue;
        shown[i].status = s.status; shown[i].detect = s.detect;
        const tile = grid.children[i]; if (!tile || !tile.classList.contains('tile')) continue;
        if (!focus) tile.querySelector('.stat').textContent = statusText(shown[i]);
        tile.querySelector('.motion').style.transform = `scaleX(${(s.detect?.score || 0) / 255})`;
      }
    } catch {}
  }, 3000);
  state.cleanup = () => { clearInterval(poll); players.forEach((p) => p.stop()); grid.querySelectorAll('img[data-snap]').forEach((i) => clearInterval(i._t)); };
}
function statusText(c) {
  if (!c.status.connected) return 'offline';
  const s = c.status;
  return `${s.width}×${s.height} ${s.fps.toFixed(0)} fps ${(s.kbps / 1000).toFixed(1)} Mb/s${c.detect?.in_event ? ' • MOTION' : ''}`;
}
function fallbackSnapshot(tile, c, every = 2000) {
  tile.querySelector('video')?.remove();
  const img = h('img', { 'data-snap': '1', alt: c.name });
  const width = every < 2000 ? 1920 : 640;
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
// Review: timeline + range playback
// ---------------------------------------------------------------------------
async function viewReview(main, camId, atMs) {
  const cams = state.cameras;
  if (!cams.length) { main.replaceChildren(h('p', { class: 'muted' }, 'No cameras.')); return; }
  const cam = cams.find((c) => c.id === Number(camId)) || cams[0];
  const now = Date.now();
  let windowMs = Number(localStorage.getItem('reviewWindow')) || 3 * 3600 * 1000;
  let end = atMs ? Math.min(now, atMs + windowMs / 2) : now;
  let start = end - windowMs;
  let playhead = atMs || (now - 60 * 1000);
  let tl = { coverage: [], motion: [], events: [], bucket_ms: 1000 };

  const video = h('video', { controls: '', playsinline: '', muted: '' });
  const overlay = h('div', { class: 'overlay' }, '');
  const player = h('div', { class: 'player' }, video, overlay);
  const canvas = h('canvas');
  const cursor = h('div', { class: 'cursor' });
  const hover = h('div', { class: 'hover' }); const tip = h('div', { class: 'tip' });
  const timeline = h('div', { class: 'timeline' }, canvas, hover, tip, cursor);
  const timeLabel = h('span', { class: 'time' });
  const camSel = h('select', { onchange: (e) => { location.hash = `#/review/${e.target.value}`; } }, ...cams.map((c) => h('option', { value: c.id, selected: c.id === cam.id ? '' : null }, c.name)));
  const winSel = h('select', { onchange: (e) => { windowMs = Number(e.target.value); localStorage.setItem('reviewWindow', windowMs); start = end - windowMs; refresh(); } },
    ...[[900000, '15 min'], [3600000, '1 h'], [3 * 3600000, '3 h'], [12 * 3600000, '12 h'], [86400000, '24 h']].map(([v, l]) => h('option', { value: v, selected: v === windowMs ? '' : null }, l)));
  const controls = h('div', { class: 'controls' },
    camSel, winSel,
    h('button', { class: 'ghost', onclick: () => { end -= windowMs / 2; start = end - windowMs; refresh(); } }, '◀'),
    h('button', { class: 'ghost', onclick: () => { end = Math.min(Date.now(), end + windowMs / 2); start = end - windowMs; refresh(); } }, '▶'),
    h('button', { class: 'ghost', onclick: () => { end = Date.now(); start = end - windowMs; playhead = end - 60000; refresh(); play(playhead); } }, 'Now'),
    timeLabel,
    h('span', { class: 'grow' }),
    h('button', { class: 'ghost', onclick: () => exportClip() }, 'Export clip'),
    h('a', { class: 'muted', href: '#', onclick: (e) => { e.preventDefault(); window.open(`/api/cameras/${cam.id}/frame.jpg?t=${Math.round(playhead)}&width=2688`); } }, 'Full-res frame'),
  );
  const strip = h('div', { class: 'strip' });
  main.replaceChildren(player, timeline, controls, strip);

  const mse = new MsePlayer(video);
  let playRange = null; // [startMs,endMs] currently loaded
  const CHUNK = 2 * 60 * 1000;

  async function play(ms) {
    playhead = ms;
    const s = Math.max(start, ms - 2000), e = Math.min(Date.now(), s + CHUNK);
    playRange = [s, e];
    overlay.textContent = 'loading…';
    try {
      if (support.mse) {
        await mse.play(`/api/cameras/${cam.id}/video.mp4?start=${Math.round(s)}&end=${Math.round(e)}`, { startMs: s });
      } else if (support.nativeHls) {
        video.src = `/api/cameras/${cam.id}/playlist.m3u8?start=${Math.round(s)}&end=${Math.round(e)}`; video.play();
      } else { overlay.textContent = 'no MSE/HLS support'; }
    } catch (err) { overlay.textContent = err.message; }
  }
  video.addEventListener('timeupdate', () => {
    const w = mse.wallMs(); if (w === null) return;
    playhead = w; overlay.textContent = fmtDT(w); timeLabel.textContent = fmtDT(w);
    if (w > end - 5000 && end < Date.now()) { end = Math.min(Date.now(), w + windowMs / 4); start = end - windowMs; refresh(false); }
    draw();
  });
  video.addEventListener('ended', () => { if (playRange && playRange[1] < Date.now() - 5000) play(playRange[1]); });

  async function refresh(redrawStrip = true) {
    try {
      tl = await api.get(`/api/cameras/${cam.id}/timeline?start=${Math.round(start)}&end=${Math.round(end)}`);
    } catch (e) { overlay.textContent = e.message; }
    draw();
    if (redrawStrip) drawStrip();
  }
  function xOf(ms) { return (ms - start) / (end - start) * canvas.clientWidth; }
  function draw() {
    const dpr = window.devicePixelRatio || 1;
    const W = canvas.clientWidth, H = canvas.clientHeight;
    if (canvas.width !== W * dpr) { canvas.width = W * dpr; canvas.height = H * dpr; }
    const ctx = canvas.getContext('2d'); ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, W, H);
    // coverage
    ctx.fillStyle = '#2b3442';
    for (const [a, b] of tl.coverage) ctx.fillRect(xOf(a), 18, Math.max(1, xOf(b) - xOf(a)), H - 18);
    // motion histogram
    const bw = Math.max(1, W / ((end - start) / tl.bucket_ms));
    for (let i = 0; i < tl.motion.length; i++) {
      const m = tl.motion[i]; if (!m) continue;
      const x = xOf(start + i * tl.bucket_ms); const hh = (H - 22) * m / 255;
      ctx.fillStyle = m > 120 ? '#ff5a5f' : '#f9b84f';
      ctx.fillRect(x, H - hh, bw, hh);
    }
    // events
    ctx.fillStyle = 'rgba(79,156,249,.7)';
    for (const ev of tl.events) ctx.fillRect(xOf(ev.start), 12, Math.max(2, xOf(ev.end || ev.start) - xOf(ev.start)), 5);
    // time ticks
    ctx.fillStyle = '#8b93a1'; ctx.font = '11px system-ui';
    const span = end - start; const step = span > 20 * 3600000 ? 3600000 * 4 : span > 6 * 3600000 ? 3600000 : span > 2 * 3600000 ? 1800000 : span > 3600000 ? 900000 : 300000;
    for (let t = Math.ceil(start / step) * step; t < end; t += step) { const x = xOf(t); ctx.fillRect(x, 0, 1, 10); ctx.fillText(fmtTime(t).slice(0, 5), x + 3, 10); }
    cursor.style.left = `${xOf(playhead)}px`;
  }
  function drawStrip() {
    strip.replaceChildren();
    const n = Math.min(24, Math.max(6, Math.floor(strip.clientWidth / 132)));
    for (let i = 0; i < n; i++) {
      const t = start + (end - start) * (i + 0.5) / n;
      if (t > Date.now() - 65000) break;
      const img = h('img', { loading: 'lazy', src: `/api/cameras/${cam.id}/frame.jpg?t=${Math.round(t)}&width=256`, title: fmtDT(t), onclick: () => play(t) });
      img.onerror = () => { img.style.opacity = .2; };
      strip.append(img);
    }
  }
  const msAt = (ev) => { const r = timeline.getBoundingClientRect(); return start + (ev.clientX - r.left) / r.width * (end - start); };
  timeline.addEventListener('pointermove', (ev) => { const t = msAt(ev); hover.style.left = `${xOf(t)}px`; tip.style.left = `${xOf(t)}px`; tip.textContent = fmtDT(t); });
  timeline.addEventListener('pointerleave', () => { tip.textContent = ''; });
  timeline.addEventListener('click', (ev) => play(msAt(ev)));
  timeline.addEventListener('wheel', (ev) => { ev.preventDefault(); const t = msAt(ev); const f = ev.deltaY > 0 ? 1.25 : 0.8; windowMs = Math.min(86400000, Math.max(300000, windowMs * f)); const frac = (t - start) / (end - start); start = t - frac * windowMs; end = start + windowMs; if (end > Date.now()) { end = Date.now(); start = end - windowMs; } refresh(); }, { passive: false });
  window.addEventListener('resize', draw);
  document.addEventListener('keydown', keys);
  function keys(e) {
    if (e.target.tagName === 'INPUT') return;
    if (e.key === ' ') { e.preventDefault(); video.paused ? video.play() : video.pause(); }
    if (e.key === 'ArrowRight') play(playhead + (e.shiftKey ? 60000 : 10000));
    if (e.key === 'ArrowLeft') play(playhead - (e.shiftKey ? 60000 : 10000));
  }
  function exportClip() {
    const s = prompt('Clip start (leave as is for current position ±30 s):', fmtDT(playhead - 30000));
    if (s === null) return;
    window.open(`/api/cameras/${cam.id}/video.mp4?start=${Math.round(playhead - 30000)}&end=${Math.round(playhead + 30000)}`);
  }
  await refresh();
  play(playhead);
  const tick = setInterval(() => { if (end >= Date.now() - 2000) { end = Date.now(); start = end - windowMs; refresh(false); } }, 15000);
  state.cleanup = () => { clearInterval(tick); mse.stop(); window.removeEventListener('resize', draw); document.removeEventListener('keydown', keys); };
}

// ---------------------------------------------------------------------------
// Montage review: every camera's substream recording, synchronized on one
// timeline. Substreams are 640x360 H.264, so a browser decodes 20+ of them
// easily and the server only copies bytes.
// ---------------------------------------------------------------------------
async function viewMontage(main, atMs) {
  const cams = state.cameras.filter((c) => c.sub_status);
  if (!cams.length) { main.replaceChildren(h('p', { class: 'muted' }, 'No cameras with substream recording enabled.')); return; }
  const now = Date.now();
  let windowMs = Number(localStorage.getItem('montageWindow')) || 3600 * 1000;
  let end = atMs ? Math.min(now, atMs + windowMs / 2) : now;
  let start = end - windowMs;
  let playhead = atMs || (now - 120 * 1000);
  const CHUNK = 2 * 60 * 1000;
  const grid = h('div', { class: 'grid montage' });
  const players = [];
  for (const c of cams) {
    const v = h('video', { muted: '', playsinline: '' });
    const tile = h('div', { class: 'tile' }, v, h('div', { class: 'label' }, c.name), h('div', { class: 'stat' }, ''));
    tile.addEventListener('dblclick', () => { location.hash = `#/review/${c.id}/${Math.round(playhead)}`; });
    grid.append(tile);
    players.push({ cam: c, video: v, mse: new MsePlayer(v), tile, stat: tile.querySelector('.stat') });
  }
  const canvas = h('canvas'); const cursor = h('div', { class: 'cursor' }); const tip = h('div', { class: 'tip' }); const hover = h('div', { class: 'hover' });
  const timeline = h('div', { class: 'timeline tall' }, canvas, hover, tip, cursor);
  const timeLabel = h('span', { class: 'time' });
  const playBtn = h('button', { class: 'ghost', onclick: () => togglePlay() }, '⏸');
  const speedSel = h('select', { style: 'width:90px', onchange: (e) => { players.forEach((p) => { p.video.playbackRate = Number(e.target.value); }); } }, ...[1, 2, 4, 8].map((r) => h('option', { value: r }, `${r}×`)));
  const winSel = h('select', { onchange: (e) => { windowMs = Number(e.target.value); localStorage.setItem('montageWindow', windowMs); start = end - windowMs; refresh(); } },
    ...[[900000, '15 min'], [3600000, '1 h'], [3 * 3600000, '3 h'], [12 * 3600000, '12 h'], [86400000, '24 h']].map(([v, l]) => h('option', { value: v, selected: v === windowMs ? '' : null }, l)));
  const controls = h('div', { class: 'controls' }, playBtn, speedSel, winSel,
    h('button', { class: 'ghost', onclick: () => seekAll(playhead - 60000) }, '−1 min'),
    h('button', { class: 'ghost', onclick: () => seekAll(playhead + 60000) }, '+1 min'),
    h('button', { class: 'ghost', onclick: () => { end = Date.now(); start = end - windowMs; refresh(); seekAll(end - 120000); } }, 'Now'),
    timeLabel, h('span', { class: 'grow' }), h('span', { class: 'muted' }, 'double-click a tile for full resolution'));
  main.replaceChildren(grid, timeline, controls);
  let tls = new Map(); // camera id -> timeline
  let paused = false;
  function togglePlay() { paused = !paused; playBtn.textContent = paused ? '▶' : '⏸'; players.forEach((p) => paused ? p.video.pause() : p.video.play().catch(() => {})); }
  async function seekAll(ms) {
    playhead = ms;
    const s = Math.max(start, ms - 1000), e = Math.min(Date.now(), s + CHUNK);
    await Promise.all(players.map(async (p) => {
      p.stat.textContent = '…';
      try { await p.mse.play(`/api/cameras/${p.cam.id}/video.mp4?stream=sub&start=${Math.round(s)}&end=${Math.round(e)}`, { startMs: s }); p.stat.textContent = ''; p.video.playbackRate = Number(speedSel.value); if (paused) p.video.pause(); }
      catch (err) { p.stat.textContent = 'no recording'; }
    }));
  }
  // keep players within 300 ms of the first one that is playing
  const sync = setInterval(() => {
    const lead = players.find((p) => p.mse.wallMs() !== null && !p.video.paused && p.video.readyState >= 2);
    if (!lead) return;
    const t = lead.mse.wallMs(); playhead = t; timeLabel.textContent = fmtDT(t); draw();
    for (const p of players) {
      if (p === lead) continue; const w = p.mse.wallMs(); if (w === null) continue;
      const d = (t - w) / 1000; if (Math.abs(d) > 0.3) p.video.currentTime += d;
    }
    if (t > end - 5000 && end < Date.now()) { end = Math.min(Date.now(), t + windowMs / 4); start = end - windowMs; refresh(); }
  }, 1000);
  players.forEach((p) => p.video.addEventListener('ended', () => { if (p === players[0]) seekAll(playhead + 500); }));
  async function refresh() {
    const res = await Promise.all(cams.map((c) => api.get(`/api/cameras/${c.id}/timeline?start=${Math.round(start)}&end=${Math.round(end)}`).catch(() => null)));
    tls = new Map(cams.map((c, i) => [c.id, res[i]]));
    draw();
  }
  function xOf(ms) { return (ms - start) / (end - start) * canvas.clientWidth; }
  function draw() {
    const dpr = window.devicePixelRatio || 1; const W = canvas.clientWidth, H = canvas.clientHeight;
    if (canvas.width !== W * dpr) { canvas.width = W * dpr; canvas.height = H * dpr; }
    const ctx = canvas.getContext('2d'); ctx.setTransform(dpr, 0, 0, dpr, 0, 0); ctx.clearRect(0, 0, W, H);
    const rowH = Math.max(6, (H - 14) / cams.length);
    cams.forEach((c, i) => {
      const tl = tls.get(c.id); const y = 14 + i * rowH;
      ctx.fillStyle = '#2b3442'; if (tl) for (const [a, b] of tl.coverage) ctx.fillRect(xOf(a), y, Math.max(1, xOf(b) - xOf(a)), rowH - 1);
      if (tl) { const bw = Math.max(1, W / ((end - start) / tl.bucket_ms)); for (let k = 0; k < tl.motion.length; k++) { const m = tl.motion[k]; if (!m) continue; ctx.fillStyle = m > 120 ? '#ff5a5f' : '#f9b84f'; ctx.fillRect(xOf(start + k * tl.bucket_ms), y, bw, rowH - 1); } }
      ctx.fillStyle = '#8b93a1'; ctx.font = '10px system-ui'; ctx.fillText(c.name, 3, y + rowH - 2);
    });
    ctx.fillStyle = '#8b93a1'; ctx.font = '11px system-ui';
    const span = end - start; const step = span > 20 * 3600000 ? 3600000 * 4 : span > 6 * 3600000 ? 3600000 : span > 2 * 3600000 ? 1800000 : span > 3600000 ? 900000 : 300000;
    for (let t = Math.ceil(start / step) * step; t < end; t += step) { const x = xOf(t); ctx.fillRect(x, 0, 1, 10); ctx.fillText(fmtTime(t).slice(0, 5), x + 3, 10); }
    cursor.style.left = `${xOf(playhead)}px`;
  }
  const msAt = (ev) => { const r = timeline.getBoundingClientRect(); return start + (ev.clientX - r.left) / r.width * (end - start); };
  timeline.addEventListener('pointermove', (ev) => { const t = msAt(ev); hover.style.left = `${xOf(t)}px`; tip.style.left = `${xOf(t)}px`; tip.textContent = fmtDT(t); });
  timeline.addEventListener('click', (ev) => seekAll(msAt(ev)));
  timeline.addEventListener('wheel', (ev) => { ev.preventDefault(); const t = msAt(ev); const f = ev.deltaY > 0 ? 1.25 : 0.8; windowMs = Math.min(86400000, Math.max(300000, windowMs * f)); const frac = (t - start) / (end - start); start = t - frac * windowMs; end = start + windowMs; if (end > Date.now()) { end = Date.now(); start = end - windowMs; } refresh(); }, { passive: false });
  window.addEventListener('resize', draw);
  await refresh();
  seekAll(playhead);
  state.cleanup = () => { clearInterval(sync); players.forEach((p) => p.mse.stop()); window.removeEventListener('resize', draw); };
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
    h('option', { value: '' }, 'All cameras'), ...cams.map((c) => h('option', { value: c.id, selected: String(c.id) === q.camera ? '' : null }, c.name)));
  const minSel = h('select', { onchange: (e) => { q.min_score = Number(e.target.value); localStorage.setItem('evMin', q.min_score); load(true); } },
    ...[[0, 'Any motion'], [60, 'Moderate+'], [120, 'Strong+'], [200, 'Very strong']].map(([v, l]) => h('option', { value: v, selected: v === q.min_score ? '' : null }, l)));
  const rangeSel = h('select', { onchange: (e) => { q.range = e.target.value; localStorage.setItem('evRange', q.range); load(true); } },
    ...[['1d', 'Last 24 h'], ['7d', 'Last 7 days'], ['30d', 'Last 30 days'], ['all', 'All']].map(([v, l]) => h('option', { value: v, selected: v === q.range ? '' : null }, l)));
  const arch = h('label', { class: 'row' }, h('input', { type: 'checkbox', style: 'width:auto', onchange: (e) => { q.archived = e.target.checked; load(true); } }), ' Archived only');
  main.replaceChildren(h('div', { class: 'toolbar' }, camSel, minSel, rangeSel, arch, h('span', { class: 'grow' }), count), list, h('div', { class: 'row', style: 'margin-top:12px' }, more));
  let before = null, total = 0;
  async function load(reset) {
    if (reset) { list.replaceChildren(); before = null; total = 0; }
    const days = { '1d': 1, '7d': 7, '30d': 30 }[q.range];
    const p = new URLSearchParams({ limit: 60 });
    if (q.camera) p.set('camera', q.camera);
    if (q.min_score) p.set('min_score', q.min_score);
    if (q.archived) p.set('archived', 'true');
    if (days) p.set('start', Date.now() - days * 86400000);
    if (before) p.set('before', before);
    const t0 = performance.now();
    const evs = await api.get(`/api/events?${p}`);
    const dt = performance.now() - t0;
    total += evs.length; before = evs.length ? evs[evs.length - 1].id : before;
    more.disabled = evs.length < 60;
    count.textContent = `${total} events (${dt.toFixed(0)} ms)`;
    for (const ev of evs) list.append(eventCard(ev, cams));
  }
  more.onclick = () => load(false);
  await load(true);
}
function eventCard(ev, cams) {
  const cam = cams.find((c) => c.id === ev.camera_id);
  const card = h('div', { class: 'event' + (ev.archived ? ' archived' : ''), onclick: () => openEvent(ev, cams) },
    h('div', { class: 'thumb', style: ev.thumb ? `background-image:url(${ev.thumb})` : '' }, h('span', { class: 'score' + (ev.score > 120 ? ' hot' : '') }, `${ev.score}`)),
    h('div', { class: 'meta' }, h('b', {}, cam?.name || `camera ${ev.camera_id}`), `${fmtDT(ev.start)} · ${ev.end ? fmtDur(ev.end - ev.start) : 'in progress'}`, ev.notes ? h('div', { class: 'muted' }, ev.notes) : null));
  return card;
}
function openEvent(ev, cams) {
  const cam = cams.find((c) => c.id === ev.camera_id);
  const video = h('video', { controls: '', autoplay: '', playsinline: '', muted: '', style: 'width:100%;background:#000;border-radius:8px' });
  const notes = h('textarea', { rows: 2, placeholder: 'Notes' }, ev.notes || '');
  const archBtn = h('button', { class: 'ghost', onclick: async () => { ev.archived = !ev.archived; await api.patch(`/api/events/${ev.id}`, { archived: ev.archived }); archBtn.textContent = ev.archived ? '★ Archived' : '☆ Archive'; } }, ev.archived ? '★ Archived' : '☆ Archive');
  const modal = h('div', { class: 'modal', onclick: (e) => { if (e.target === modal) close(); } },
    h('div', { class: 'card' },
      h('div', { class: 'row' }, h('h2', {}, `${cam?.name || ''} — ${fmtDT(ev.start)}`), h('span', { class: 'grow' }), h('button', { class: 'ghost small', onclick: close }, '✕')),
      video,
      h('div', { class: 'row', style: 'margin-top:8px' },
        archBtn,
        h('button', { class: 'ghost', onclick: () => { close(); location.hash = `#/review/${ev.camera_id}/${ev.peak || ev.start}`; } }, 'Open in timeline'),
        h('a', { class: 'muted', href: `/api/cameras/${ev.camera_id}/video.mp4?start=${ev.start}&end=${ev.end || ev.start + 60000}`, download: `event-${ev.id}.mp4`, onclick: (e) => e.stopPropagation() }, 'Download'),
        h('span', { class: 'muted' }, `peak ${ev.score}`)),
      h('label', {}, notes),
      h('button', { class: 'small', onclick: async () => { await api.patch(`/api/events/${ev.id}`, { notes: notes.value }); ev.notes = notes.value; } }, 'Save notes'),
    ));
  document.body.append(modal);
  const mse = new MsePlayer(video);
  const url = `/api/cameras/${ev.camera_id}/video.mp4?start=${ev.start}&end=${ev.end || ev.start + 60000}`;
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
  const camTable = h('table', {}, h('tr', {}, h('th', {}, 'Id'), h('th', {}, 'Name'), h('th', {}, 'Status'), h('th', {}, 'Storage'), h('th', {}, 'Retention'), h('th', {}, 'Detect'), h('th', {}, '')),
    ...cams.map((c) => h('tr', {},
      h('td', {}, c.id), h('td', {}, c.name),
      h('td', {}, h('span', { class: 'pill ' + (c.status.connected ? 'ok' : 'bad') }, c.status.connected ? `${c.status.codec} ${c.status.width}×${c.status.height} ${c.status.fps.toFixed(0)}fps` : (c.status.last_error || 'offline')), ' ',
        c.sub_status ? h('span', { class: 'pill ' + (c.sub_status.connected ? 'ok' : 'bad') }, c.sub_status.connected ? `sub ${c.sub_status.width}×${c.sub_status.height}` : 'sub offline') : null),
      h('td', {}, c.storage_id), h('td', {}, `${c.retention_days} d`),
      h('td', {}, c.detect ? `${c.detect.fps.toFixed(1)} fps, score ${c.detect.score}` : '—'),
      h('td', {}, h('button', { class: 'ghost small', onclick: () => editCamera(c) }, 'Edit'), ' ', h('button', { class: 'ghost small', onclick: async () => { if (confirmDelete(`Delete camera ${c.name}? Recordings are removed by retention.`)) { await api.del(`/api/cameras/${c.id}`); route(); } } }, 'Delete')))));
  const addCam = h('form', { class: 'row', onsubmit: async (e) => { e.preventDefault(); const f = new FormData(e.target); await api.post('/api/cameras', { name: f.get('name'), main_url: f.get('main_url'), sub_url: f.get('sub_url') || null }); route(); } },
    h('input', { name: 'name', placeholder: 'Name', required: '', style: 'width:160px' }), h('input', { name: 'main_url', placeholder: 'rtsp://user:pass@ip/Streaming/Channels/101', required: '', style: 'flex:2;min-width:240px' }), h('input', { name: 'sub_url', placeholder: 'substream rtsp:// (optional)', style: 'flex:2;min-width:240px' }), h('button', {}, 'Add camera'));
  const userTable = h('table', {}, h('tr', {}, h('th', {}, 'User'), h('th', {}, 'Role'), h('th', {}, 'Cameras'), h('th', {}, '')),
    ...users.map((u) => h('tr', {}, h('td', {}, u.username), h('td', {}, u.role), h('td', {}, u.role === 'admin' ? 'all' : u.cameras.map((id) => cams.find((c) => c.id === id)?.name || id).join(', ') || h('span', { class: 'err' }, 'none')),
      h('td', {}, h('button', { class: 'ghost small', onclick: () => editUserCams(u, cams) }, 'Cameras'), ' ', h('button', { class: 'ghost small', onclick: async () => { if (confirmDelete(`Delete user ${u.username}?`)) { await api.del(`/api/users/${u.id}`); route(); } } }, 'Delete')))));
  const addUser = h('form', { class: 'row', onsubmit: async (e) => { e.preventDefault(); const f = new FormData(e.target); await api.post('/api/users', { username: f.get('username'), password: f.get('password'), role: f.get('role') }); route(); } },
    h('input', { name: 'username', placeholder: 'username', required: '', style: 'width:160px' }), h('input', { name: 'password', type: 'password', placeholder: 'password (8+)', required: '', style: 'width:180px' }), h('select', { name: 'role', style: 'width:120px' }, h('option', { value: 'viewer' }, 'viewer'), h('option', { value: 'admin' }, 'admin')), h('button', {}, 'Add user'));
  const stor = h('table', {}, h('tr', {}, h('th', {}, 'Id'), h('th', {}, 'Path'), h('th', {}, 'Used'), h('th', {}, 'Free'), h('th', {}, 'Cap'), h('th', {}, 'Reserve'), h('th', {}, 'Archive to'), h('th', {}, '')),
    ...storages.map((s) => h('tr', {}, h('td', {}, s.id), h('td', {}, h('code', {}, s.path), s.available ? '' : h('span', { class: 'pill bad' }, 'missing')), h('td', {}, gb(s.used_bytes)), h('td', {}, gb(s.free_bytes)), h('td', {}, s.max_bytes ? gb(s.max_bytes) : '—'), h('td', {}, gb(s.reserve_bytes)),
      h('td', {}, s.archive_to ? `storage ${s.archive_to} after ${s.archive_after_days ?? '—'} d @ ${s.archive_rate_mbps} Mb/s` : '—'),
      h('td', {}, h('button', { class: 'ghost small', onclick: () => editStorage(s, storages) }, 'Edit')))));
  main.replaceChildren(h('div', { class: 'two' },
    h('div', { class: 'card', style: 'grid-column:1/-1' }, h('h2', {}, 'Cameras'), camTable, h('div', { style: 'margin-top:10px' }, addCam)),
    h('div', { class: 'card' }, h('h2', {}, 'Users'), userTable, h('div', { style: 'margin-top:10px' }, addUser), h('p', { class: 'muted' }, 'Viewers see only the cameras assigned to them (fails closed).')),
    h('div', { class: 'card' }, h('h2', {}, 'Storage'), stor, h('p', { class: 'muted' }, 'Retention deletes whole segments, oldest first, per camera age limit then per volume space budget. Add volumes with the CLI: zmng add-storage.')),
    h('div', { class: 'card' }, h('h2', {}, 'Browser'), h('p', {}, `MSE: ${support.mse} · HEVC via MSE: ${support.hevcMse} · H.264 via MSE: ${support.h264Mse} · native HLS: ${support.nativeHls}`)),
  ));
}
const gb = (b) => `${(b / 1e9).toFixed(1)} GB`;
function confirmDelete(msg) { return window.confirm ? window.confirm(msg) : true; }
function editCamera(c) {
  const fields = [['name', 'Name'], ['main_url', 'Main RTSP URL'], ['sub_url', 'Substream RTSP URL'], ['retention_days', 'Retention (days)'], ['event_retention_days', 'Keep footage with events (days, 0 = same)'], ['detect_fps', 'Detect fps'], ['detect_width', 'Detect width'], ['detect_height', 'Detect height'], ['pixel_threshold', 'Pixel threshold (0-255)'], ['min_area_pct', 'Min changed area %'], ['min_blob_pct', 'Min blob %'], ['pre_secs', 'Pre-roll s'], ['post_secs', 'Post-roll s'], ['cooldown_secs', 'Cooldown s'], ['zones_json', 'Zones JSON [[[x,y],...]] (0..1)'], ['masks_json', 'Masks JSON'], ['sort_order', 'Sort order']];
  const inputs = {};
  const form = h('form', { onsubmit: async (e) => { e.preventDefault(); const patch = {}; for (const [k] of fields) { let v = inputs[k].value; if (v === '' && k === 'sub_url') v = null; else if (!['name', 'main_url', 'sub_url', 'zones_json', 'masks_json'].includes(k)) v = Number(v); if (v !== c[k]) patch[k] = v; } patch.enabled = inputs.enabled.checked; patch.record_sub = inputs.record_sub.checked; try { await api.patch(`/api/cameras/${c.id}`, patch); modal.remove(); route(); } catch (err) { errEl.textContent = err.message; } } },
    ...fields.map(([k, l]) => h('label', {}, l, inputs[k] = h('input', { value: c[k] ?? '' }))),
    h('label', { class: 'row' }, inputs.enabled = h('input', { type: 'checkbox', style: 'width:auto', checked: c.enabled ? '' : null }), ' Enabled'),
    h('label', { class: 'row' }, inputs.record_sub = h('input', { type: 'checkbox', style: 'width:auto', checked: c.record_sub ? '' : null }), ' Record substream too (multi-camera review, phones)'),
    h('div', { class: 'row' }, h('button', {}, 'Save'), h('button', { type: 'button', class: 'ghost', onclick: () => modal.remove() }, 'Cancel')));
  const errEl = h('p', { class: 'err' });
  const modal = h('div', { class: 'modal' }, h('div', { class: 'card' }, h('h2', {}, `Camera ${c.id}`), form, errEl));
  document.body.append(modal);
}
function editStorage(s, all) {
  const inputs = {};
  const form = h('form', { onsubmit: async (e) => { e.preventDefault(); const patch = {
      max_bytes: inputs.max_gb.value === '' ? null : Math.round(Number(inputs.max_gb.value) * 1e9),
      reserve_bytes: Math.round(Number(inputs.reserve_gb.value) * 1e9),
      archive_to: inputs.archive_to.value === '' ? null : Number(inputs.archive_to.value),
      archive_after_days: inputs.archive_after_days.value === '' ? null : Number(inputs.archive_after_days.value),
      archive_rate_mbps: Number(inputs.archive_rate_mbps.value) };
    try { await api.patch(`/api/storages/${s.id}`, patch); modal.remove(); route(); } catch (err) { errEl.textContent = err.message; } } },
    h('label', {}, 'Cap (GB, blank = use free-space reserve only)', inputs.max_gb = h('input', { value: s.max_bytes ? (s.max_bytes / 1e9).toFixed(0) : '' })),
    h('label', {}, 'Reserve free (GB)', inputs.reserve_gb = h('input', { value: (s.reserve_bytes / 1e9).toFixed(0) })),
    h('label', {}, 'Archive to (tiering)', inputs.archive_to = h('select', {}, h('option', { value: '' }, 'never (delete here by retention)'), ...all.filter((o) => o.id !== s.id).map((o) => h('option', { value: o.id, selected: o.id === s.archive_to ? '' : null }, `storage ${o.id}: ${o.path}`)))),
    h('label', {}, 'Move segments older than (days)', inputs.archive_after_days = h('input', { value: s.archive_after_days ?? '' })),
    h('label', {}, 'Copy rate limit (Mb/s)', inputs.archive_rate_mbps = h('input', { value: s.archive_rate_mbps })),
    h('p', { class: 'muted' }, 'Recent footage of every camera stays here; older segments are copied to the archive volume (checksummed), then removed here. If the archive volume is missing, nothing is moved and recording continues.'),
    h('div', { class: 'row' }, h('button', {}, 'Save'), h('button', { type: 'button', class: 'ghost', onclick: () => modal.remove() }, 'Cancel')));
  const errEl = h('p', { class: 'err' });
  const modal = h('div', { class: 'modal' }, h('div', { class: 'card' }, h('h2', {}, `Storage ${s.id}`), form, errEl));
  document.body.append(modal);
}
function editUserCams(u, cams) {
  const boxes = cams.map((c) => h('label', { class: 'row' }, h('input', { type: 'checkbox', style: 'width:auto', value: c.id, checked: u.cameras.includes(c.id) ? '' : null }), ` ${c.name}`));
  const modal = h('div', { class: 'modal' }, h('div', { class: 'card' }, h('h2', {}, `Cameras for ${u.username}`), ...boxes,
    h('div', { class: 'row' }, h('button', { onclick: async () => { const ids = boxes.map((b) => b.querySelector('input')).filter((i) => i.checked).map((i) => Number(i.value)); await api.patch(`/api/users/${u.id}`, { cameras: ids }); modal.remove(); route(); } }, 'Save'), h('button', { class: 'ghost', onclick: () => modal.remove() }, 'Cancel'))));
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
  try {
    state.me = await api.get('/api/me');
  } catch { return; }
  if (state.me.setup_needed) { showLogin(true); return; }
  state.cameras = await api.get('/api/cameras');
  document.querySelectorAll('.admin-only').forEach((el) => { el.hidden = state.me.user.role !== 'admin'; });
  route();
}
async function route() {
  if (state.cleanup) { state.cleanup(); state.cleanup = null; }
  const parts = (location.hash || '#/live').slice(2).split('/');
  const view = parts[0] || 'live';
  document.querySelectorAll('#top nav a').forEach((a) => a.classList.toggle('active', a.dataset.view === view));
  const main = $('#main');
  try {
    if (view === 'live') await viewLive(main, parts[1]);
    else if (view === 'review') await viewReview(main, parts[1], parts[2] ? Number(parts[2]) : null);
    else if (view === 'montage') await viewMontage(main, parts[1] ? Number(parts[1]) : null);
    else if (view === 'events') await viewEvents(main);
    else if (view === 'admin') await viewAdmin(main);
  } catch (e) { main.replaceChildren(h('p', { class: 'err' }, e.message)); }
}
window.addEventListener('hashchange', route);
boot();
