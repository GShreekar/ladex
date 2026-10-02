// End-to-end test against real LADEX processes. Needs a built binary
// (cargo build; override with LADEX_BIN) and Node 22+. Run: node tests/e2e/client.test.js
const { spawn } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const vm = require('node:vm');

const os = require('node:os');
const BIN = process.env.LADEX_BIN || path.join(__dirname, '..', '..', 'target', 'debug', 'ladex');
const STATIC = path.join(__dirname, '..', '..', 'static');
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), 'ladex-e2e-client-')); const PASS = 'correct-horse-battery'; const MiB = 1048576;
const procs = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sha = (b) => crypto.createHash('sha256').update(b).digest('hex');
function start(n, extra = []) {
    fs.mkdirSync(path.join(ROOT, `n${n}/home`), { recursive: true });
    const port = 9300 + n * 10; const log = fs.openSync(path.join(ROOT, `n${n}.log`), 'w');
    procs.push(spawn(BIN, [PASS, '--port', String(port), '--no-discovery', '--data-dir', path.join(ROOT, `n${n}/files`), '--stall-timeout-secs', '10', ...extra], { env: { ...process.env, HOME: path.join(ROOT, `n${n}/home`) }, stdio: ['ignore', log, log] }));
    return `localhost:${port + 1}`;
}
const login = async (b) => (await fetch(`http://${b}/auth`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ passphrase: PASS }) })).headers.get('set-cookie').match(/auth=([0-9a-f]+)/)[1];
let failures = 0; const check = (n, ok, d = '') => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${n}${ok ? '' : '  ' + d}`); if (!ok) failures++; };

// ---- a page: the real app.js + receive-policy.js in a VM, talking to `base`
function loadPage(base, token) {
    const log = { progress: [], toasts: [], clicks: [], navigations: [], sent: [], html: {} };
    const anything = () => new Proxy(function () {}, {
        get: (t, p) => (p === Symbol.toPrimitive ? () => '' : p === 'length' ? 0 : p === 'then' ? undefined : anything()),
        apply: () => anything(), set: () => true, construct: () => anything(),
    });
    const element = (id) => new Proxy({ id, classList: { add() {}, remove() {}, contains: () => false }, style: {}, dataset: {}, addEventListener() {}, querySelector: () => element('q'), appendChild() {}, remove() {}, set innerHTML(v) { log.html[id] = v; }, get innerHTML() { return log.html[id] || ''; } }, { get: (t, p) => (p in t ? t[p] : anything()), set: (t, p, v) => { t[p] = v; return true; } });
    const document = {
        getElementById: (id) => element(id), querySelector: () => null, querySelectorAll: () => [], body: element('body'),
        addEventListener() {}, createElement: (tag) => { const e = element(tag); e.click = () => log.clicks.push({ href: e.href, download: e.download }); return e; },
    };
    class XHR {
        constructor() { this.headers = {}; this.upload = {}; this.status = 0; this.responseText = ''; }
        open(method, url) { this.method = method; this.url = url; }
        setRequestHeader(k, v) { this.headers[k] = v; }
        abort() { this._aborted = true; this._req && this._req.destroy(); this.onabort && this.onabort(); }
        async send(body) {
            const u = new URL(this.url, `http://${base}`);
            const req = http.request({ host: u.hostname, port: u.port, path: u.pathname + u.search, method: this.method, headers: { ...this.headers, Cookie: `auth=${token}`, Origin: `http://${base}`, 'Content-Length': body.size } }, (res) => {
                const parts = []; res.on('data', (c) => parts.push(c));
                res.on('end', () => { this.status = res.statusCode; this.responseText = Buffer.concat(parts).toString(); this.onload && this.onload(); });
            });
            this._req = req;
            req.on('error', () => { if (!this._aborted) this.onerror && this.onerror(); });
            const failAfter = page.failNextPutAfter; page.failNextPutAfter = null;
            const reader = body.stream().getReader(); let sent = 0;
            for (;;) {
                const { done, value } = await reader.read(); if (done) break;
                if (this._aborted) return;
                for (let at = 0; at < value.length; at += 65536) {
                    if (this._aborted) return;
                    const piece = value.subarray(at, at + 65536);
                    await new Promise((r) => req.write(Buffer.from(piece), r));
                    sent += piece.length; this.upload.onprogress && this.upload.onprogress({ lengthComputable: true, loaded: sent });
                    if (page.slowMs) await sleep(page.slowMs);
                    if (failAfter != null && sent >= failAfter) { req.destroy(); return; }
                }
            }
            req.end();
        }
    }
    const storage = () => { const m = new Map(); return { getItem: (k) => m.get(k) ?? null, setItem: (k, v) => m.set(k, String(v)) }; };
    const window = { location: { get href() { return ''; }, set href(v) { log.navigations.push(v); }, protocol: 'http:', host: base }, addEventListener() {} };
    const context = {
        window, location: window.location, document, console: { log() {}, warn() {}, error() {} }, navigator: { userAgent: 'test' },
        localStorage: storage(), sessionStorage: storage(), XMLHttpRequest: XHR, crypto: globalThis.crypto,
        fetch: (url, opts = {}) => fetch(new URL(url, `http://${base}`), { ...opts, headers: { ...(opts.headers || {}), Cookie: `auth=${token}`, Origin: `http://${base}` } }),
        WebSocket: class { static OPEN = 1; constructor() { this.readyState = 1; } send(m) { log.sent.push(JSON.parse(m)); } close() {} },
        setTimeout, clearTimeout, setInterval: () => 0, clearInterval() {}, AbortController, TransformStream, File, Blob, URL, TextEncoder, Promise, Math, JSON, Date, Array, Object, Set, Map, Number, String, Error, Symbol, encodeURIComponent, decodeURIComponent, Uint8Array, Intl, parseInt, Proxy, qrcode: undefined,
    };
    context.globalThis = context; context.window.window = window;
    vm.createContext(context);
    vm.runInContext(fs.readFileSync(path.join(STATIC, 'receive-policy.js'), 'utf8'), context);
    context.LadexPolicy = window.LadexPolicy;
    const App = vm.runInContext(fs.readFileSync(path.join(STATIC, 'app.js'), 'utf8') + '\n;LADEXApp;', context);
    const app = new App();
    app.showProgress = (id, name, pct, speed, eta, done, total) => log.progress.push({ id, name, pct, done, total });
    app.hideProgress = () => {};
    app.toast = (message, type) => log.toasts.push({ message, type });
    app.updateFileList = () => {};
    const page = { app, log, failNextPutAfter: null, slowMs: 0, context };
    return page;
}

function joinWs(base, token, session) {
    return new Promise((resolve, reject) => {
        const ws = new WebSocket(`ws://${base}/ws`, { headers: { Cookie: `auth=${token}`, Origin: `http://${base}` } });
        const received = []; ws.onmessage = (e) => received.push(JSON.parse(e.data));
        const c = { ws, received, send: (m) => ws.send(JSON.stringify(m)), files: () => [...received].reverse().find((m) => m.type === 'file_list_update')?.files ?? [],
            wait: async (p, ms = 6000) => { const end = Date.now() + ms; while (Date.now() < end) { const h = received.find(p); if (h) return h; await sleep(25); } return null; } };
        ws.onopen = () => { c.send({ type: 'join', session_id: session }); resolve(c); }; ws.onerror = () => reject(new Error('ws'));
    });
}

// A directory in memory that behaves like the File System Access API's.
function memoryDirectory(existing = {}) {
    const files = new Map(Object.entries(existing)); const dirs = new Map();
    const notFound = () => Object.assign(new Error('nf'), { name: 'NotFoundError' });
    return {
        files, dirs,
        async getDirectoryHandle(name, { create } = {}) { if (!dirs.has(name)) { if (!create) throw notFound(); dirs.set(name, memoryDirectory()); } return dirs.get(name); },
        async getFileHandle(name, { create } = {}) {
            if (!files.has(name)) { if (!create) throw notFound(); files.set(name, Buffer.alloc(0)); }
            return { async createWritable() { const parts = []; return new WritableStream({ write(c) { parts.push(Buffer.from(c)); }, close() { files.set(name, Buffer.concat(parts)); } }); } };
        },
    };
}

(async () => {
    fs.rmSync(ROOT, { recursive: true, force: true });
    const b1 = start(1); await sleep(1200); const b2 = start(2, ['--peer', '127.0.0.1:9310']); await sleep(2500);
    const [t1, t2] = [await login(b1), await login(b2)];
    const p1 = loadPage(b1, t1); const p2 = loadPage(b2, t2);
    const w1 = await joinWs(b1, t1, p1.app.sessionId); const w2 = await joinWs(b2, t2, p2.app.sessionId); await sleep(500);
    const { app } = p1;
    check('a tab keeps one session id and it is a valid id', /^peer_[0-9a-f]{20}$/.test(app.sessionId) && app.generateSessionId() === app.sessionId);

    // ---- upload
    const big = crypto.randomBytes(12 * MiB + 321);
    let entry = await app.uploadFile(new File([big], 'big file.bin', { type: 'application/octet-stream' }));
    check('uploadFile returns the node\'s catalog entry', entry.name === 'big file.bin' && entry.size === big.length && entry.uploader_id === app.sessionId, JSON.stringify(entry));
    check('progress reached 100%', p1.log.progress.length > 0 && p1.log.progress.at(-1).pct === 100, JSON.stringify(p1.log.progress.at(-1)));
    const stored = fs.readFileSync(path.join(ROOT, `n1/files/${entry.id}.data`));
    check('the node stores exactly the bytes sent', sha(stored) === sha(big));

    // ---- a connection that drops half way, then resumes
    const data2 = crypto.randomBytes(10 * MiB + 5);
    p1.failNextPutAfter = 4 * MiB + 100; p1.log.progress.length = 0;
    entry = await app.uploadFile(new File([data2], 'resumed.bin'));
    const sentFull = p1.log.progress.filter((p) => p.done === data2.length).length > 0;
    check('an interrupted upload resumes and completes', entry.size === data2.length && sentFull, JSON.stringify(entry));
    check('the resumed file is intact', sha(fs.readFileSync(path.join(ROOT, `n1/files/${entry.id}.data`))) === sha(data2));

    // ---- cancel
    p1.slowMs = 20;
    const slow = new File([crypto.randomBytes(8 * MiB)], 'slow.bin');
    const group = 'up:cancelme';
    const pending = app.uploadFile(slow, { group }).then(() => 'finished', (e) => e.message);
    await sleep(600);
    app.cancelTransfer(group);
    const outcome = await pending;
    check('cancelling an upload stops it', outcome === 'cancelled', `got: ${outcome}`);
    p1.slowMs = 0;

    // ---- a refused upload says why
    const tooBigStatus = await fetch(`http://${b1}/api/files/file_q/upload?size=1`, { headers: { Cookie: `auth=${t1}` } });
    check('upload status works for an unknown file', (await tooBigStatus.json()).offset === 0);

    // ---- folder
    const parts = [['pics/a.png', crypto.randomBytes(2 * MiB)], ['pics/deep/b.txt', Buffer.from('bee')], ['c.bin', crypto.randomBytes(1500)]];
    const folderFiles = parts.map(([rel, buf]) => { const f = new File([buf], rel.split('/').pop()); Object.defineProperty(f, 'webkitRelativePath', { value: `myfolder/${rel}` }); return f; });
    await app.handleFolderUpload(folderFiles);
    await sleep(500);
    const folder = w1.files().find((f) => f.is_folder);
    check('a folder upload publishes one folder entry', folder && folder.name === 'myfolder' && folder.folder_files === 3, JSON.stringify(folder));
    check('its files carry the folder as parent', w1.files().filter((f) => f.parent === folder.id).length === 3);

    // ---- rendering: folders' files are hidden, holders are named
    const real = vm.runInContext('LADEXApp.prototype.updateFileList', p1.context);
    app.serverFiles = w1.files(); app.nodeId = 'node_x'; app.isHost = false;
    for (const m of w1.received) if (m.type === 'peer_joined') app._rememberNode(m.peer);
    real.call(app, app.serverFiles);
    const html = p1.log.html['files-list'] || '';
    check('the list shows files and the folder but not the files inside it', html.includes('big file.bin') && html.includes('myfolder') && !html.includes('b.txt') && !html.includes('pics'), html.slice(0, 200));
    check('the folder shows its file count', html.includes('Folder · 3 files'));
    check('the sharing device gets an unshare button', html.includes('data-action="delete-file"'));
    check('holders are shown by device name', /host-badge">[^<]+<\/span>/.test(html));

    // ---- download from the other node: native download via an <a>
    await sleep(500);
    const a2 = p2.app; a2.serverFiles = w2.files();
    await a2.downloadFile(entry.id);
    check('downloading starts the browser\'s own download for the file\'s URL', p2.log.clicks.length === 1 && p2.log.clicks[0].href === `/api/files/${entry.id}` && p2.log.clicks[0].download === 'resumed.bin', JSON.stringify(p2.log.clicks));
    const probe = await fetch(`http://${b2}/api/files/${entry.id}`, { headers: { Cookie: `auth=${t2}` } });
    check('and that URL really serves the file from node 2', sha(Buffer.from(await probe.arrayBuffer())) === sha(data2));

    // a file nobody online has
    a2.serverFiles = [{ ...w2.files()[0], id: 'file_ghost', is_folder: false, holders: {} }];
    p2.log.toasts.length = 0;
    await a2.downloadFile('file_ghost');
    check('an unknown file is reported, not downloaded', p2.log.toasts.some((t) => /removed/.test(t.message)) && p2.log.clicks.length === 1, JSON.stringify(p2.log.toasts));

    // ---- folder download, without a folder picker: a zip URL
    a2.serverFiles = w2.files(); p2.log.clicks.length = 0;
    await a2.downloadFolder(a2.serverFiles.find((f) => f.is_folder));
    check('without a folder picker a folder is saved as a zip', p2.log.clicks.length === 1 && p2.log.clicks[0].href === `/api/folders/${folder.id}.zip` && p2.log.clicks[0].download === 'myfolder.zip', JSON.stringify(p2.log.clicks));

    // ---- folder download with a folder picker, into a directory that already has a file of the same name
    const dir = memoryDirectory();
    const existing = await dir.getDirectoryHandle('myfolder', { create: true });
    existing.files.set('c.bin', Buffer.from('MINE, do not overwrite'));
    p2.context.window.showDirectoryPicker = async () => dir;
    await a2.downloadFolder(a2.serverFiles.find((f) => f.is_folder));
    const root = dir.dirs.get('myfolder');
    check('the folder is recreated inside the picked directory, structure and all',
        root.files.has('c (1).bin') !== undefined && root.dirs.get('pics').files.has('a.png') && root.dirs.get('pics').dirs.get('deep').files.get('b.txt').toString() === 'bee',
        JSON.stringify([...root.files.keys(), ...root.dirs.keys()]));
    check('the pictures arrive byte for byte', sha(root.dirs.get('pics').files.get('a.png')) === sha(parts[0][1]));
    check('an existing file is never overwritten; the new one gets a number',
        root.files.get('c.bin').toString() === 'MINE, do not overwrite' && root.files.has('c (1).bin') && sha(root.files.get('c (1).bin')) === sha(parts[2][1]),
        JSON.stringify([...root.files.keys()]));

    // ---- unshare through the page
    app.deleteFile(entry.id);
    check('unsharing sends the delete', p1.log.sent.some((m) => m.type === 'delete_file' && m.file_id === entry.id && m.session_id === app.sessionId));

    procs.forEach((p) => p.kill('SIGKILL'));
    console.log(failures === 0 ? '\nALL PASSED' : `\n${failures} FAILED`); fs.rmSync(ROOT, { recursive: true, force: true });
    process.exit(failures ? 1 : 0);
})().catch((e) => { console.error('crashed', e); procs.forEach((p) => p.kill('SIGKILL')); process.exit(2); });
