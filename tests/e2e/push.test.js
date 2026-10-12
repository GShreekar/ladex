// A file sent to one device, against real LADEX processes; needs `cargo build` (or LADEX_BIN) and Node 22+. Run: node tests/e2e/push.test.js
const { spawn } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const BIN = process.env.LADEX_BIN || path.join(__dirname, '..', '..', 'target', 'debug', 'ladex');
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), 'ladex-push-'));
const PASS = 'correct-horse-battery';
const MiB = 1024 * 1024;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const procs = {};
function startNode(n, extraArgs = []) {
    const dir = path.join(ROOT, `n${n}`);
    fs.mkdirSync(path.join(dir, 'home'), { recursive: true });
    const port = 9400 + n * 10;
    const args = [PASS, '--port', String(port), '--no-discovery', '--no-keychain', '--data-dir', path.join(dir, 'files'), ...extraArgs];
    const log = fs.openSync(path.join(ROOT, `n${n}.log`), 'a');
    procs[n] = spawn(BIN, args, { env: { ...process.env, HOME: path.join(dir, 'home') }, stdio: ['ignore', log, log] });
    return { base: `localhost:${port + 1}`, files: path.join(dir, 'files') };
}

async function login(base) {
    const r = await fetch(`http://${base}/auth`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ passphrase: PASS }) });
    return r.headers.get('set-cookie').match(/auth=([0-9a-f]+)/)[1];
}

function join(base, token, session) {
    return new Promise((resolve, reject) => {
        const ws = new WebSocket(`ws://${base}/ws`, { headers: { Cookie: `auth=${token}`, Origin: `http://${base}` } });
        const received = [];
        ws.onmessage = (e) => received.push(JSON.parse(e.data));
        const tab = {
            received, session,
            send: (m) => ws.send(JSON.stringify({ ...m, session_id: session })),
            knows: (other) => received.some((m) => (m.type === 'peer_sync' && m.peers.some((p) => p.session_id === other)) || (m.type === 'peer_joined' && m.peer.session_id === other)),
            wait: async (pred, ms = 8000) => { const end = Date.now() + ms; while (Date.now() < end) { const hit = received.find(pred); if (hit) return hit; await sleep(25); } return null; },
        };
        ws.onopen = () => { tab.send({ type: 'join' }); resolve(tab); };
        ws.onerror = () => reject(new Error('ws error'));
    });
}

async function sendTo(base, token, session, to, id, buf) {
    const headers = {
        Cookie: `auth=${token}`, Origin: `http://${base}`, 'X-Ladex-Session': session, 'X-Ladex-To': to, 'X-Ladex-Size': String(buf.length),
        'X-Ladex-Offset': '0', 'X-Ladex-Name': encodeURIComponent('for-you.bin'), 'Content-Type': 'application/octet-stream',
    };
    const r = await fetch(`http://${base}/api/files/${id}`, { method: 'PUT', headers, body: buf });
    return { status: r.status, body: await r.json().catch(() => ({})) };
}

async function fetchFile(base, token, id, device) {
    const r = await fetch(`http://${base}/api/files/${id}${device ? `?to=${device}` : ''}`, { headers: { Cookie: `auth=${token}` } });
    return { status: r.status, name: r.headers.get('content-disposition'), buf: r.ok ? Buffer.from(await r.arrayBuffer()) : null };
}

const stored = (node, id) => ['blobs', 'partial'].some((dir) => fs.readdirSync(path.join(node.files, dir)).some((name) => name.startsWith(id)));

let failures = 0;
const check = (name, ok, detail = '') => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok ? '' : '  ' + detail}`); if (!ok) failures++; };

(async () => {
    const n1 = startNode(1);
    await sleep(1200);
    const n2 = startNode(2, ['--peer', '127.0.0.1:9410']);
    await sleep(2500);
    const [t1, t2, t2other] = [await login(n1.base), await login(n2.base), await login(n2.base)];
    const alice = await join(n1.base, t1, 'peer_alice');
    const bob = await join(n2.base, t2, 'peer_bob');
    const carol = await join(n2.base, t2other, 'peer_carol');
    check('the sending tab learns of the device on the other node', !!(await alice.wait(() => alice.knows('peer_bob'), 15000)));

    const content = crypto.randomBytes(5 * MiB + 77);
    let r = await sendTo(n1.base, t1, 'peer_alice', 'peer_bob', 'file_gift', content);
    check('the node accepts a file meant for one device', r.status === 201 && !r.body.file, JSON.stringify(r));
    const offer = await bob.wait((m) => m.type === 'incoming_file_offer' && m.file_id === 'file_gift');
    check('that device is asked, and told the name and size', offer?.from_session_id === 'peer_alice' && offer?.pushed?.name === 'for-you.bin' && offer?.pushed?.size === content.length, JSON.stringify(offer));
    check('no other device is asked', !carol.received.some((m) => m.type === 'incoming_file_offer'));
    check('nothing reaches the receiving node before the device accepts', !stored(n2, 'file_gift'));
    check('and the file cannot be fetched before it is accepted', (await fetchFile(n2.base, t2, 'file_gift', 'peer_bob')).status === 404);

    bob.send({ type: 'accept_file_offer', file_id: 'file_gift' });
    let got;
    for (let attempt = 0; attempt < 20 && got?.status !== 200; attempt++) { await sleep(200); got = await fetchFile(n2.base, t2, 'file_gift', 'peer_bob'); }
    check('once accepted, the device downloads the file intact from its own node', got.status === 200 && got.buf.equals(content), `status ${got.status}`);
    check('under the name it was sent with', /for-you\.bin/.test(got.name || ''), got.name);
    check('the sender is told it arrived', !!(await alice.wait((m) => m.type === 'file_push_delivered' && m.file_id === 'file_gift' && m.to_session_id === 'peer_bob')));
    await sleep(300);
    check('and its node drops its copy', !stored(n1, 'file_gift'));

    check('another login cannot fetch the file by naming the device', (await fetchFile(n2.base, t2other, 'file_gift', 'peer_bob')).status === 404);
    check('nor as its own device', (await fetchFile(n2.base, t2other, 'file_gift', 'peer_carol')).status === 404);
    check('nor without naming one', (await fetchFile(n2.base, t2, 'file_gift')).status === 404);
    const listed = [alice, bob, carol].some((tab) => tab.received.some((m) => m.type === 'file_list_update' && m.files.some((f) => f.id === 'file_gift')));
    check('the file never shows up in the shared list', !listed);

    r = await sendTo(n1.base, t1, 'peer_alice', 'peer_bob', 'file_unwanted', crypto.randomBytes(MiB));
    await bob.wait((m) => m.type === 'incoming_file_offer' && m.file_id === 'file_unwanted');
    bob.send({ type: 'decline_file_offer', target_session_id: 'peer_alice', file_id: 'file_unwanted' });
    check('a refusal reaches the sender', !!(await alice.wait((m) => m.type === 'file_offer_declined' && m.file_id === 'file_unwanted' && m.from_session_id === 'peer_bob')));
    await sleep(300);
    check('and the declined file is on neither node', !stored(n1, 'file_unwanted') && !stored(n2, 'file_unwanted'));

    r = await sendTo(n2.base, t2, 'peer_bob', 'peer_carol', 'file_near', content);
    const near = await carol.wait((m) => m.type === 'incoming_file_offer' && m.file_id === 'file_near');
    carol.send({ type: 'accept_file_offer', file_id: 'file_near' });
    await sleep(300);
    got = await fetchFile(n2.base, t2other, 'file_near', 'peer_carol');
    check('a device on the same node is asked and gets the file too', r.status === 201 && !!near && got.status === 200 && got.buf.equals(content), `status ${got.status}`);

    r = await sendTo(n1.base, t1, 'peer_alice', 'peer_nobody', 'file_lost', crypto.randomBytes(1000));
    check('a file for a device that is not there is refused', r.status === 404, JSON.stringify(r));

    for (const n of Object.keys(procs)) procs[n].kill('SIGKILL');
    console.log(failures === 0 ? '\nALL PASSED' : `\n${failures} FAILED`);
    if (!process.env.LADEX_KEEP) fs.rmSync(ROOT, { recursive: true, force: true }); else console.log('kept', ROOT);
    process.exit(failures ? 1 : 0);
})().catch((e) => { console.error('test crashed:', e); for (const n of Object.keys(procs)) procs[n].kill('SIGKILL'); process.exit(2); });
