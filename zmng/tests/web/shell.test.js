// Drives the real index.html + app.js in jsdom with a scripted server: the page must never
// end up as an empty shell. Run: npm i jsdom && node tests/web/shell.test.js web
const { JSDOM } = require('jsdom'); const fs = require('fs'); const assert = require('assert');
const web = process.argv[2];
const html = fs.readFileSync(web + '/index.html', 'utf8').replace(/<script[^>]*src="\/app.js"[^>]*><\/script>/, '').replace(/<link[^>]*>/g, '');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function page(server) {
  const dom = new JSDOM(html, { url: 'https://10.10.100.100/', runScripts: 'outside-only', pretendToBeVisual: true });
  const w = dom.window; const calls = [];
  const errors = [];
  w.addEventListener('error', (e) => errors.push(e.message));
  w.addEventListener('unhandledrejection', (e) => errors.push('unhandled: ' + e.reason));
  process.on('unhandledRejection', (e) => errors.push('unhandled: ' + (e && e.message)));
  w.fetch = async (url, opt = {}) => {
    calls.push(`${opt.method || 'GET'} ${url}`);
    const r = await server(url, opt);
    if (r instanceof Error) throw r;
    if (r === 'hang') return new Promise((_, rej) => opt.signal?.addEventListener('abort', () => { const e = new Error('aborted'); e.name = 'AbortError'; rej(e); }));
    return { status: r.status, ok: r.status >= 200 && r.status < 300, statusText: r.text || '', headers: { get: (k) => (k.toLowerCase() === 'content-type' ? (typeof r.body === 'string' ? 'text/html' : 'application/json') : null) }, json: async () => r.body, text: async () => String(r.body) };
  };
  w.EventSource = class { constructor() { this.readyState = 1; } addEventListener() {} close() {} }; w.EventSource.CLOSED = 2;
  w.ResizeObserver = class { observe() {} disconnect() {} };
  let reloads = 0;
  w.eval(fs.readFileSync(web + '/app.js', 'utf8').replace(/location\.reload\(\)/g, 'window.__reload()'));
  w.__reload = () => { reloads++; };
  return { w, calls, errors, reloads: () => reloads, $: (s) => w.document.querySelector(s), nav: async (hash) => { w.location.hash = hash; await sleep(30); } };
}
const me = { user: { id: 1, username: 'admin', role: 'admin' }, cameras: [], setup_needed: false, go2rtc_url: null, transcode: true, objects: true, peers: [], armed: true };
(async () => {
  const old = process.argv[3] === 'old';
  // 1. the server cannot be reached (network down, certificate refused): what the screenshot showed
  let up = false, auth = true;
  const server = async (url) => {
    if (!up) return new TypeError('Load failed');
    if (!auth) return { status: 401, body: { error: 'unauthorized' } };
    if (url === '/api/me') return { status: 200, body: me };
    if (url === '/api/cameras') return { status: 200, body: [] };
    if (url === '/api/logout') return { status: 200, body: { ok: true } };
    return { status: 200, body: { cameras: [] } };
  };
  let p = await page(server); await sleep(50);
  await p.nav('#/live');
  const main1 = p.$('#main').textContent;
  console.log('unreachable -> main says:', JSON.stringify(main1.slice(0, 90)));
  p.$('#logout').click(); await sleep(50);
  console.log('unreachable -> sign out reloads:', p.reloads(), ' errors:', p.errors);
  if (!old) {
    assert(/Cannot reach the zmng server/.test(main1) && /Load failed/.test(main1), 'the page must say it cannot reach the server and why');
    assert(!/null|undefined/.test(main1), 'no script error text in the page');
    assert.strictEqual(p.reloads(), 1, 'sign out must always reload');
    assert.deepStrictEqual(p.errors, []);
    // 2. the server comes back: the page recovers by itself (retry countdown starts at 1 s)
    up = true; await sleep(1400);
    assert.strictEqual(p.$('#account').textContent, 'admin', 'recovered without a reload');
    assert(!/Cannot reach/.test(p.$('#main').textContent), 'the notice is gone');
    console.log('server back -> recovered by itself, account button:', p.$('#account').textContent);
    // 3. a session that expired: the sign-in form, not an empty page
    auth = false; p = await page(server); await sleep(50); await p.nav('#/events');
    assert.strictEqual(p.$('#login').hidden, false, 'sign-in form shows on 401');
    assert.deepStrictEqual(p.errors, []);
    console.log('expired session -> sign-in form shown');
    // 4. the proxy answers 502 while zmng restarts, and a server that never answers
    for (const [name, r] of [['502', { status: 502, body: 'Bad Gateway' }], ['hang', 'hang']]) {
      const t0 = Date.now();
      p = await page(async () => r); p.w.eval('0'); await sleep(name === 'hang' ? 10300 : 60);
      assert(/Cannot reach the zmng server/.test(p.$('#main').textContent), name + ': notice');
      console.log(name, '->', JSON.stringify(p.$('#main').querySelector('.err').textContent), `(${Date.now() - t0} ms)`);
    }
    // 5. clicking around before the first answer arrives does nothing bad
    let release; const gate = new Promise((r) => { release = r; });
    p = await page(async (url) => { await gate; return url === '/api/me' ? { status: 200, body: me } : { status: 200, body: url === '/api/cameras' ? [] : { cameras: [] } }; });
    await p.nav('#/live'); await p.nav('#/status');
    assert.strictEqual(p.$('#main').textContent, '', 'nothing drawn, no error, while waiting');
    release(); await sleep(80);
    assert.strictEqual(p.$('#account').textContent, 'admin'); assert.deepStrictEqual(p.errors, []);
    console.log('early clicks -> ignored until the server answers, then the view draws');
  }
  console.log(old ? 'OLD BUILD BEHAVIOUR SHOWN' : 'ALL OK'); process.exit(0);
})().catch((e) => { console.error('FAIL', e.message); process.exit(1); });
