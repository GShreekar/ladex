// Phase 5 exit criteria (source killed, receiver kill -9, many receivers), against real LADEX processes; needs `cargo build` (or LADEX_BIN) and Node 22+. Run: node tests/e2e/swarm.test.js
// LADEX_SWARM_MIB sets the file size (default 256); LADEX_SWARM_RECEIVERS sets the swarm size (default 10).
const { spawn } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const BIN = process.env.LADEX_BIN || path.join(__dirname, '..', '..', 'target', 'debug', 'ladex');
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), 'ladex-swarm-'));
const PASS = 'correct-horse-battery';
const MiB = 1024 * 1024;
const SIZE = Number(process.env.LADEX_SWARM_MIB || 256) * MiB;
const RECEIVERS = Number(process.env.LADEX_SWARM_RECEIVERS || 10);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const procs = {};
const nodes = {};
function startNode(n, peers = []) {
    const dir = path.join(ROOT, `n${n}`);
    fs.mkdirSync(path.join(dir, 'home'), { recursive: true });
    const port = 9700 + n * 10;
    const files = path.join(dir, 'files');
    const args = [PASS, '--port', String(port), '--no-discovery', '--no-keychain', '--data-dir', files,
        ...peers.flatMap((p) => ['--peer', `127.0.0.1:${9700 + p * 10}`])];
    const log = fs.openSync(path.join(ROOT, `n${n}.log`), 'a');
    procs[n] = spawn(BIN, args, { env: { ...process.env, HOME: path.join(dir, 'home') }, stdio: ['ignore', log, log] });
    nodes[n] = { files, dir };
    return nodes[n];
}
const kill = (n, signal = 'SIGKILL') => new Promise((resolve) => {
    if (procs[n].exitCode !== null) return resolve();
    procs[n].once('exit', resolve);
    procs[n].kill(signal);
});

function start(node, args, cwd = ROOT) {
    const child = spawn(BIN, [...args, '--data-dir', node.files, '--json'], { cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    const out = { stdout: '', stderr: '' };
    child.stdout.on('data', (d) => { out.stdout += d; });
    child.stderr.on('data', (d) => { out.stderr += d; });
    const done = new Promise((resolve) => child.on('close', (code) => resolve({ code, ...out, at: Date.now() })));
    return { child, done };
}
const run = (node, args, cwd) => start(node, args, cwd).done;
const until = async (pred, ms = 20000) => { const end = Date.now() + ms; while (Date.now() < end) { const hit = await pred(); if (hit) return hit; await sleep(100); } return null; };

const sha = (file) => new Promise((resolve, reject) => {
    const h = crypto.createHash('sha256');
    fs.createReadStream(file).on('data', (d) => h.update(d)).on('end', () => resolve(h.digest('hex'))).on('error', reject);
});
const outDir = (name) => { const d = path.join(ROOT, 'out', name); fs.mkdirSync(d, { recursive: true }); return d; };
const seconds = (ms) => (ms / 1000).toFixed(1);

// Waits until `node` lists `name` with a holder, which is when a `get` on it can start.
const listed = (node, name) => until(async () => {
    const r = await run(node, ['ls']);
    const f = r.code === 0 && JSON.parse(r.stdout).files.find((x) => x.name === name);
    return f && f.hosts.length > 0 ? f : null;
});

async function timedGet(node, name, dest) {
    const t0 = Date.now();
    const r = await run(node, ['get', name, '-o', dest]);
    return { ...r, ms: r.at - t0 };
}

let failures = 0;
const check = (name, ok, detail = '') => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok ? '' : '  ' + detail}`); if (!ok) failures++; };
const finish = async () => {
    for (const n of Object.keys(procs)) procs[n].kill('SIGKILL');
    console.log(failures === 0 ? '\nALL PASSED' : `\n${failures} FAILED`);
    if (!process.env.LADEX_KEEP) fs.rmSync(ROOT, { recursive: true, force: true }); else console.log('kept', ROOT);
    process.exit(failures ? 1 : 0);
};

(async () => {
    const src = path.join(ROOT, 'src');
    fs.mkdirSync(src);
    const bigPath = path.join(src, 'big.bin');
    fs.writeFileSync(bigPath, crypto.randomBytes(SIZE));
    const want = await sha(bigPath);
    const isIntact = async (dir) => (await sha(path.join(dir, 'big.bin')).catch(() => null)) === want;

    // Node 1 is the only holder. Node 2 is the 1 -> 1 baseline.
    const n1 = startNode(1);
    await sleep(1500);
    let r = await run(n1, ['send', 'src/big.bin']);
    check('node 1 shares the file', r.code === 0, r.stderr);
    const n2 = startNode(2, [1]);
    await listed(n2, 'big.bin');
    const baseline = await timedGet(n2, 'big.bin', outDir('baseline'));
    check('1 -> 1: the file arrives intact', baseline.code === 0 && await isIntact(outDir('baseline')), baseline.stderr);
    console.log(`      1 -> 1 took ${seconds(baseline.ms)}s`);
    const t1 = baseline.ms;
    await kill(2, 'SIGINT');

    // 1 -> N: fresh nodes, all asking at once.
    const ids = Array.from({ length: RECEIVERS }, (_, i) => 10 + i);
    const swarm = ids.map((n) => startNode(n, [1, ...ids.filter((o) => o < n).slice(-2)]));
    await Promise.all(ids.map((n, i) => listed(swarm[i], 'big.bin')));
    const t0 = Date.now();
    const results = await Promise.all(ids.map((n, i) => run(swarm[i], ['get', 'big.bin', '-o', outDir(`swarm${n}`)])));
    const total = Date.now() - t0;
    check(`1 -> ${RECEIVERS}: every node gets the file intact`,
        results.every((x) => x.code === 0) && (await Promise.all(ids.map((n) => isIntact(outDir(`swarm${n}`))))).every(Boolean),
        results.map((x) => x.stderr).join(' '));
    console.log(`      1 -> ${RECEIVERS} took ${seconds(total)}s (${(total / t1).toFixed(2)}x the 1 -> 1 time)`);
    // Ten nodes on one machine share its CPU and disk, so only the figure is shown; crates/ladex-core/tests/swarm.rs holds the 2x check.
    for (const n of ids.slice(1)) await kill(n);

    // Killing a source mid-transfer: node 10 and node 1 hold the file; node 30 loses node 1 part-way.
    const n30 = startNode(30, [1, ids[0]]);
    await listed(n30, 'big.bin');
    const killing = start(n30, ['get', 'big.bin', '-o', outDir('source-killed')]);
    let finished = false;
    killing.done.then(() => { finished = true; });
    await sleep(Math.max(150, t1 * 0.2));
    check('the source is killed while the transfer is still running', !finished, `the transfer ended before the kill; raise LADEX_SWARM_MIB`);
    await kill(1);
    const survived = await killing.done;
    check('killing a source mid-transfer: the transfer completes from another holder',
        survived.code === 0 && await isIntact(outDir('source-killed')), survived.stderr);
    await kill(30);

    // kill -9 on the receiver mid-transfer; node 10 is now the only source.
    const n31 = startNode(31, [ids[0]]);
    await listed(n31, 'big.bin');
    const interrupted = start(n31, ['get', 'big.bin', '-o', outDir('resumed')]);
    finished = false;
    interrupted.done.then(() => { finished = true; });
    await sleep(Math.max(150, t1 * 0.4));
    check('the receiver is killed while the transfer is still running', !finished, `the transfer ended before the kill; raise LADEX_SWARM_MIB`);
    await kill(31);
    await interrupted.done;
    const partials = fs.readdirSync(path.join(n31.files, 'partial'), { recursive: true }).filter((f) => String(f).endsWith('.part'));
    check('the unfinished download is still on disk after the kill', partials.length === 1, JSON.stringify(partials));
    startNode(31, [ids[0]]);
    await listed(n31, 'big.bin');
    const resumedAt = Date.now();
    r = await run(n31, ['get', 'big.bin', '-o', outDir('resumed')]);
    check('kill -9 on the receiver: after a restart the download resumes and completes intact',
        r.code === 0 && await isIntact(outDir('resumed')), r.stderr);
    console.log(`      the resumed download finished ${seconds(r.at - resumedAt)}s after the restart (a full download takes ${seconds(t1)}s)`);

    await finish();
})().catch((e) => { console.error('test crashed:', e); for (const n of Object.keys(procs)) procs[n].kill('SIGKILL'); process.exit(2); });
