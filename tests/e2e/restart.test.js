// What a node remembers across a restart. Needs a built binary (cargo build; override with LADEX_BIN)
// and Node 22+. Run: node tests/e2e/restart.test.js
const { spawn } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const BIN = process.env.LADEX_BIN || path.join(__dirname, '..', '..', 'target', 'debug', 'ladex');
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), 'ladex-restart-'));
const PASS = 'correct-horse-battery';
const PORT = 9300;
const BASE = `localhost:${PORT + 1}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let proc;
function start(pass = PASS) {
    fs.mkdirSync(path.join(ROOT, 'home'), { recursive: true });
    const log = fs.openSync(path.join(ROOT, 'node.log'), 'a');
    proc = spawn(BIN, [pass, '--port', String(PORT), '--no-discovery', '--no-keychain', '--data-dir', path.join(ROOT, 'files')], {
        env: { ...process.env, HOME: path.join(ROOT, 'home') }, stdio: ['ignore', log, log],
    });
}
async function stop(signal) {
    proc.kill(signal);
    await new Promise((r) => proc.once('exit', r));
}
async function up() {
    for (let i = 0; i < 100; i++) {
        try { await fetch(`http://${BASE}/auth-status`); return; } catch { await sleep(100); }
    }
    throw new Error('node did not start');
}

async function login(pass = PASS) {
    const r = await fetch(`http://${BASE}/auth`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ passphrase: pass }) });
    return r.headers.get('set-cookie')?.match(/auth=([0-9a-f]+)/)?.[1];
}
const status = async (token) => (await fetch(`http://${BASE}/auth-status`, { headers: { Cookie: `auth=${token}` } })).json();

function join(token, session) {
    return new Promise((resolve, reject) => {
        const ws = new WebSocket(`ws://${BASE}/ws`, { headers: { Cookie: `auth=${token}`, Origin: `http://${BASE}` } });
        const received = [];
        ws.onmessage = (e) => received.push(JSON.parse(e.data));
        const c = {
            ws, received, send: (m) => ws.send(JSON.stringify(m)),
            wait: async (pred, ms = 5000) => { const end = Date.now() + ms; while (Date.now() < end) { const h = received.find(pred); if (h) return h; await sleep(25); } return null; },
        };
        ws.onopen = () => { c.send({ type: 'join', session_id: session }); resolve(c); };
        ws.onerror = () => reject(new Error('ws error'));
    });
}

async function upload(token, session, id, buf, name) {
    const headers = {
        Cookie: `auth=${token}`, Origin: `http://${BASE}`, 'X-Ladex-Session': session, 'X-Ladex-Size': String(buf.length),
        'X-Ladex-Offset': '0', 'X-Ladex-Name': encodeURIComponent(name), 'Content-Type': 'application/octet-stream',
    };
    return (await fetch(`http://${BASE}/api/files/${id}`, { method: 'PUT', headers, body: buf })).status;
}

let failures = 0;
const check = (name, ok, detail = '') => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok ? '' : '  ' + detail}`); if (!ok) failures++; };

(async () => {
    start(); await up();
    let token = await login();
    const nodeId = (await status(token)).node_id;
    const body = crypto.randomBytes(3 * 1024 * 1024 + 123);
    const gone = crypto.randomBytes(1000);
    const c = await join(token, 'peer_one');
    check('file upload accepted', (await upload(token, 'peer_one', 'file_keep', body, 'keep.bin')) === 201);
    await upload(token, 'peer_one', 'file_gone', gone, 'gone.bin');
    await c.wait((m) => m.type === 'file_list_update' && m.files.some((f) => f.id === 'file_gone'));
    c.send({ type: 'text_message', session_id: 'peer_one', content: 'remember me' });
    await c.wait((m) => m.type === 'text_message');
    c.send({ type: 'delete_file', session_id: 'peer_one', file_id: 'file_gone' });
    await sleep(400);
    c.ws.close();

    // ---- graceful stop: SIGINT flushes the saved state at once
    await stop('SIGINT');
    start(); await up();
    check('the same session cookie still works after a restart', (await fetch(`http://${BASE}/auth-status`, { headers: { Cookie: `auth=${token}` } })).ok);
    const after = await status(token);
    check('the node keeps its identity', after.node_id === nodeId, `${after.node_id} vs ${nodeId}`);

    const d = await join(token, 'peer_one');
    const history = await d.wait((m) => m.type === 'message_history');
    check('chat history survives', history?.messages.some((m) => m.content === 'remember me'));
    const list = await d.wait((m) => m.type === 'file_list_update' && m.files.some((f) => f.id === 'file_keep'));
    const kept = list?.files.find((f) => f.id === 'file_keep');
    check('the shared file is still listed', !!kept);
    check('the node is still its holder under the same id', kept?.holders?.[nodeId]?.present === true, JSON.stringify(kept?.holders));
    check('an unshared file stays unshared', !list?.files.some((f) => f.id === 'file_gone'));
    const saved = JSON.parse(fs.readFileSync(path.join(ROOT, 'files', 'node.state'), 'utf8'));
    check('the unshare record is saved', saved.tombstones.some((f) => f.id === 'file_gone' && f.deleted));
    const r = await fetch(`http://${BASE}/api/files/file_keep`, { headers: { Cookie: `auth=${token}` } });
    check('the file downloads intact', Buffer.from(await r.arrayBuffer()).equals(body));
    d.send({ type: 'delete_file', session_id: 'peer_one', file_id: 'file_keep' });
    const del = await d.wait((m) => m.type === 'file_list_update' && !m.files.some((f) => f.id === 'file_keep'));
    check('the original uploader session can still unshare after a restart', !!del);
    d.ws.close();

    // ---- hard kill: nothing needs a clean shutdown for the files and identity
    await sleep(100);
    const hardJoin = await join(token, 'peer_one');
    check('upload after a restart accepted', (await upload(token, 'peer_one', 'file_hard', gone, 'hard.bin')) === 201);
    await sleep(400);
    hardJoin.ws.close();
    await stop('SIGKILL');
    start(); await up();
    const hard = await status(token);
    check('identity survives a hard kill', hard.node_id === nodeId);
    const e = await join(token, 'peer_one');
    const hardList = await e.wait((m) => m.type === 'file_list_update' && m.files.some((f) => f.id === 'file_hard'));
    check('files survive a hard kill', !!hardList);
    e.ws.close();

    // ---- a different passphrase must not honour old logins
    await stop('SIGINT');
    start('another-passphrase-entirely'); await up();
    const old = await fetch(`http://${BASE}/auth-status`, { headers: { Cookie: `auth=${token}` } });
    check('logins from the old passphrase are dropped', !(await old.json()).authenticated);
    check('a login with the new passphrase works', !!(await login('another-passphrase-entirely')));
    await stop('SIGINT');

    const mode = fs.statSync(path.join(ROOT, 'files', 'node.state')).mode & 0o777;
    check('the saved state is private to the user', mode === 0o600, mode.toString(8));

    console.log(failures ? `\n${failures} FAILED` : '\nALL PASSED');
    process.exit(failures ? 1 : 0);
})().catch((e) => { console.error(e); try { proc.kill('SIGKILL'); } catch {} process.exit(1); });
