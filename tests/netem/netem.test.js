// Real nodes over a bad network: each node runs in its own Linux network namespace,
// joined to a bridge by a veth pair, with tc netem adding loss, delay and reordering.
// Needs root, iproute2, the sch_netem kernel module, a built binary (cargo build;
// override with LADEX_BIN) and Node 22+. Run: sudo "$(which node)" tests/netem/netem.test.js
const { spawn, execSync } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const BIN = process.env.LADEX_BIN || path.join(__dirname, '..', '..', 'target', 'debug', 'ladex');
const ROOT = process.env.LADEX_NETEM_DIR || fs.mkdtempSync(path.join(os.tmpdir(), 'ladex-netem-'));
const PASS = 'correct-horse-battery';
const PORT = 9400;
const BRIDGE = 'ladexbr0';
// The plan's real-network profile: a few percent loss and 50 ms of jitter.
const LOSSY = 'delay 50ms 20ms distribution normal loss 3% reorder 25% 50%';
const CUT = 'loss 100%';
// Longer than the mesh heartbeat timeout (15 s), so the nodes notice the cut.
const PARTITION_MS = 25000;

const nodes = [1, 2, 3].map((i) => ({
    name: `node${i}`, ns: `ladex${i}`, hostIf: `lxh${i}`, ip: `10.77.0.${10 + i}`,
    url: `http://10.77.0.${10 + i}:${PORT}`, dir: path.join(ROOT, `node${i}`),
}));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sh = (cmd) => execSync(cmd, { stdio: 'pipe' });
const shQuiet = (cmd) => { try { sh(cmd); } catch {} };

function setUpNetwork() {
    sh(`ip link add ${BRIDGE} type bridge`);
    sh(`ip addr add 10.77.0.1/24 dev ${BRIDGE}`);
    sh(`ip link set ${BRIDGE} up`);
    // Docker's FORWARD policy would otherwise drop traffic between bridge ports.
    shQuiet(`iptables -I FORWARD -i ${BRIDGE} -o ${BRIDGE} -j ACCEPT`);
    for (const n of nodes) {
        sh(`ip netns add ${n.ns}`);
        // Inside the namespace the interface is eth0: LADEX ignores veth* names as virtual.
        sh(`ip link add ${n.hostIf} type veth peer name eth0 netns ${n.ns}`);
        sh(`ip link set ${n.hostIf} master ${BRIDGE} up`);
        sh(`ip -n ${n.ns} addr add ${n.ip}/24 dev eth0`);
        sh(`ip -n ${n.ns} link set eth0 up`);
        sh(`ip -n ${n.ns} link set lo up`);
        sh(`ip -n ${n.ns} route add default via 10.77.0.1`);
    }
}

function tearDownNetwork() {
    for (const n of nodes) {
        try { n.proc?.kill('SIGKILL'); } catch {}
        shQuiet(`ip netns del ${n.ns}`);
    }
    shQuiet(`ip link del ${BRIDGE}`);
    shQuiet(`iptables -D FORWARD -i ${BRIDGE} -o ${BRIDGE} -j ACCEPT`);
}

// Applies to everything the node sends, so a cut node can neither send nor answer.
const netem = (n, spec) => sh(`ip -n ${n.ns} qdisc replace dev eth0 root netem ${spec}`);

function start(n, peers) {
    fs.mkdirSync(path.join(n.dir, 'home'), { recursive: true });
    const log = fs.openSync(path.join(n.dir, 'node.log'), 'a');
    const args = [PASS, '--port', String(PORT), '--no-discovery', '--no-tls', '--data-dir', path.join(n.dir, 'files'),
        ...peers.flatMap((p) => ['--peer', `${p.ip}:${PORT}`])];
    n.proc = spawn('ip', ['netns', 'exec', n.ns, BIN, ...args], {
        env: { ...process.env, HOME: path.join(n.dir, 'home'), RUST_LOG: 'info' }, stdio: ['ignore', log, log],
    });
}

async function up(n) {
    for (let i = 0; i < 200; i++) {
        try { await fetch(`${n.url}/auth-status`, { signal: AbortSignal.timeout(2000) }); return; } catch { await sleep(100); }
    }
    throw new Error(`${n.name} did not start`);
}

async function login(n) {
    const r = await fetch(`${n.url}/auth`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ passphrase: PASS }) });
    n.token = r.headers.get('set-cookie')?.match(/auth=([0-9a-f]+)/)?.[1];
    if (!n.token) throw new Error(`could not log in to ${n.name}`);
}

function join(n, session) {
    return new Promise((resolve, reject) => {
        const ws = new WebSocket(`ws://${n.ip}:${PORT}/ws`, { headers: { Cookie: `auth=${n.token}`, Origin: n.url } });
        const received = [];
        ws.onmessage = (e) => received.push(JSON.parse(e.data));
        const c = {
            ws, received, send: (m) => ws.send(JSON.stringify(m)),
            wait: async (pred, ms = 10000) => { const end = Date.now() + ms; while (Date.now() < end) { const h = received.find(pred); if (h) return h; await sleep(25); } return null; },
        };
        ws.onopen = () => { c.send({ type: 'join', session_id: session }); resolve(c); };
        ws.onerror = () => reject(new Error(`ws error from ${n.name}`));
    });
}

async function upload(n, session, id, buf) {
    const headers = {
        Cookie: `auth=${n.token}`, Origin: n.url, 'X-Ladex-Session': session, 'X-Ladex-Size': String(buf.length),
        'X-Ladex-Offset': '0', 'X-Ladex-Name': `${id}.bin`, 'Content-Type': 'application/octet-stream',
    };
    return (await fetch(`${n.url}/api/files/${id}`, { method: 'PUT', headers, body: buf })).status;
}

async function download(n, id) {
    const r = await fetch(`${n.url}/api/files/${id}`, { headers: { Cookie: `auth=${n.token}` }, signal: AbortSignal.timeout(60000) });
    return r.ok ? Buffer.from(await r.arrayBuffer()) : null;
}

// What a freshly opened browser tab on this node would show.
async function snapshot(n) {
    let c;
    try {
        c = await join(n, 'probe');
        const files = await c.wait((m) => m.type === 'file_list_update', 5000);
        const history = await c.wait((m) => m.type === 'message_history', 5000);
        if (!files || !history) return null;
        return { files: files.files.map((f) => f.id), messages: history.messages.map((m) => m.content) };
    } catch {
        return null;
    } finally {
        try { c?.ws.close(); } catch {}
    }
}

async function waitUntil(n, pred, ms) {
    const end = Date.now() + ms;
    while (Date.now() < end) {
        const s = await snapshot(n);
        if (s && pred(s)) return true;
        await sleep(1000);
    }
    return false;
}

let failures = 0;
const check = (name, ok, detail = '') => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok ? '' : '  ' + detail}`); if (!ok) failures++; };

(async () => {
    if (process.getuid() !== 0) throw new Error('needs root: sudo "$(which node)" tests/netem/netem.test.js');
    tearDownNetwork();
    setUpNetwork();
    for (const n of nodes) netem(n, LOSSY);
    const [one, two, three] = nodes;

    start(one, []); await up(one);
    start(two, [one]); await up(two);
    start(three, [one, two]); await up(three);
    for (const n of nodes) await login(n);

    // ---- a lossy, jittery, reordering network
    const fileA = crypto.randomBytes(3 * 1024 * 1024 + 321);
    const uploader = await join(one, 'uploader');
    const chatter = await join(two, 'chatter');
    // Uploads are refused until the node has processed the device's join.
    await uploader.wait((m) => m.type === 'file_list_update');
    await chatter.wait((m) => m.type === 'file_list_update');
    check('upload over a lossy network accepted', (await upload(one, 'uploader', 'file_a', fileA)) === 201);
    await upload(one, 'uploader', 'file_c', crypto.randomBytes(2000));
    chatter.send({ type: 'text_message', session_id: 'chatter', content: 'hello over a lossy network' });
    for (const n of nodes) {
        const ok = await waitUntil(n, (s) => s.files.includes('file_a') && s.files.includes('file_c') && s.messages.includes('hello over a lossy network'), 60000);
        check(`${n.name} sees the shared files and the message`, ok);
    }
    check('node3 downloads a file held by node1 intact', (await download(three, 'file_a'))?.equals(fileA));

    // ---- node3 cut off, while the others carry on
    netem(three, CUT);
    const fileB = crypto.randomBytes(1024 * 1024 + 99);
    check('upload during the partition accepted', (await upload(one, 'uploader', 'file_b', fileB)) === 201);
    uploader.send({ type: 'delete_file', session_id: 'uploader', file_id: 'file_c' });
    chatter.send({ type: 'text_message', session_id: 'chatter', content: 'said during the partition' });
    check('node2 sees the change made on node1 during the partition',
        await waitUntil(two, (s) => s.files.includes('file_b') && !s.files.includes('file_c'), 30000));
    await sleep(PARTITION_MS);

    // ---- healed: node3 must catch up on everything it missed
    netem(three, LOSSY);
    const caughtUp = await waitUntil(three, (s) => s.files.includes('file_b') && !s.files.includes('file_c')
        && s.messages.includes('said during the partition'), 120000);
    check('node3 catches up after the partition heals, unshares included', caughtUp);
    check('node3 then downloads the file shared while it was away', (await download(three, 'file_b'))?.equals(fileB));

    uploader.ws.close();
    chatter.ws.close();
    tearDownNetwork();
    console.log(failures ? `\n${failures} FAILED (logs in ${ROOT})` : '\nALL PASSED');
    process.exit(failures ? 1 : 0);
})().catch((e) => { console.error(e); tearDownNetwork(); console.error(`logs in ${ROOT}`); process.exit(1); });
