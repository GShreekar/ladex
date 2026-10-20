// What the app does, done from a terminal alone, against real LADEX processes; needs `cargo build` (or LADEX_BIN) and Node 22+.
// Run: node tests/e2e/parity.test.js
const { spawn } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const BIN = process.env.LADEX_BIN || path.join(__dirname, '..', '..', 'target', 'debug', 'ladex');
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), 'ladex-parity-'));
const MiB = 1024 * 1024;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const nodes = {};
const procs = [];
function node(n) {
    const dir = path.join(ROOT, `n${n}`);
    fs.mkdirSync(path.join(dir, 'home'), { recursive: true });
    const port = 9800 + n * 10;
    return nodes[n] = { n, port, base: `localhost:${port + 1}`, files: path.join(dir, 'files'), home: path.join(dir, 'home') };
}

// Starts the node with no passphrase on the command line, so it runs with the one saved in its data folder.
function startNode(node, extraArgs = []) {
    const args = ['--port', String(node.port), '--no-discovery', '--no-keychain', '--data-dir', node.files, ...extraArgs];
    const log = fs.openSync(path.join(ROOT, `n${node.n}.log`), 'a');
    procs.push(spawn(BIN, args, { env: { ...process.env, HOME: node.home }, stdio: ['ignore', log, log] }));
}

// Starts a command; `done` resolves with its exit code and what it printed. `input` is written to its standard input.
function start(node, args, { cwd = ROOT, input } = {}) {
    const child = spawn(BIN, [...args, '--data-dir', node.files], { cwd, env: { ...process.env, HOME: node.home }, stdio: ['pipe', 'pipe', 'pipe'] });
    const out = { stdout: '', stderr: '' };
    child.stdout.on('data', (d) => { out.stdout += d; });
    child.stderr.on('data', (d) => { out.stderr += d; });
    child.stdin.end(input ?? '');
    const done = new Promise((resolve) => child.on('close', (code) => resolve({ code, ...out })));
    procs.push(child);
    return { child, out, done };
}
const run = (node, args, options) => start(node, args, options).done;
const runJson = async (node, args, options) => { const r = await run(node, [...args, '--json'], options); return { ...r, json: JSON.parse(r.stdout.trim().split('\n').pop() || 'null') }; };
const within = (promise, ms) => Promise.race([promise, sleep(ms).then(() => null)]);
const until = async (pred, ms = 10000) => { const end = Date.now() + ms; while (Date.now() < end) { const hit = await pred(); if (hit) return hit; await sleep(100); } return null; };

let failures = 0;
const check = (name, ok, detail = '') => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok ? '' : '  ' + detail}`); if (!ok) failures++; };

(async () => {
    const [n1, n2] = [node(1), node(2)];

    // The first-run wizard's mesh choice: make up a passphrase on one machine and type it on the other.
    let r = await runJson(n1, ['passphrase', 'set', '--generate', '--no-keychain']);
    check('passphrase set --generate saves a passphrase before any node runs', r.code === 0 && /Saved for/.test(r.json?.message || ''), r.stdout + r.stderr);
    r = await runJson(n1, ['passphrase', '--no-keychain']);
    const passphrase = r.json?.passphrase;
    check('passphrase shows the saved one', r.code === 0 && typeof passphrase === 'string' && passphrase.length > 10, r.stdout + r.stderr);
    r = await run(n2, ['passphrase', 'set', '--no-keychain'], { input: `${passphrase}\n` });
    check('passphrase set reads it from standard input, to join the same mesh', r.code === 0, r.stdout + r.stderr);

    startNode(n1);
    await sleep(1200);
    startNode(n2, ['--peer', `127.0.0.1:${n1.port}`]);
    await sleep(2500);
    const auth = await fetch(`http://${n1.base}/auth`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ passphrase }) });
    check('a node started without one runs with the saved passphrase', auth.status === 200, auth.status);
    r = await runJson(n2, ['devices']);
    const isMeshed = await until(async () => (await runJson(n2, ['devices'])).json?.nodes.length === 1);
    check('and the two nodes, given the same passphrase, form a mesh', !!isMeshed, r.stdout);
    r = await runJson(n1, ['passphrase']);
    check('passphrase asks the running node when there is one', r.json?.passphrase === passphrase, r.stdout);

    // Receiving files sent to this device, as the app's window does.
    const out = path.join(ROOT, 'received');
    const receiving = start(n2, ['receive', '--name', 'lab-pc', '-o', out]);
    await until(() => /Receiving as/.test(receiving.out.stderr));
    const isListed = await until(async () => (await runJson(n1, ['devices'])).json?.devices.some((d) => d.name === 'lab-pc'));
    check('receive joins as a device the other node lists by name', !!isListed, receiving.out.stderr);

    const gift = crypto.randomBytes(2 * MiB + 7);
    fs.writeFileSync(path.join(ROOT, 'gift.bin'), gift);
    r = await run(n1, ['send', 'gift.bin', '--to', 'lab-pc']);
    check('with nobody at a terminal and the default rule, a file sent to it is declined', r.code === 1 && /declined/.test(r.stderr), r.stdout + r.stderr);
    check('and receive says why', /nobody is at a terminal/.test(receiving.out.stderr), receiving.out.stderr);

    r = await run(n2, ['settings', '--auto-accept', 'mesh', '--no-size-limit']);
    check('the incoming files rule is set from the terminal', r.code === 0, r.stderr);
    r = await within(run(n1, ['send', 'gift.bin', '--to', 'lab-pc']), 30000);
    check('then a file sent to the device is accepted without asking', r?.code === 0 && /1 delivered/.test(r.stdout), JSON.stringify(r) + receiving.out.stdout + receiving.out.stderr);
    const saved = path.join(out, 'gift.bin');
    check('and saved intact where receive was told', fs.existsSync(saved) && fs.readFileSync(saved).equals(gift));
    receiving.child.kill('SIGINT');
    await receiving.done;
    const isGone = await until(async () => !(await runJson(n1, ['devices'])).json?.devices.some((d) => d.name === 'lab-pc'));
    check('stopping receive takes the device out of the mesh', !!isGone);

    // Chat.
    r = await run(n1, ['chat', 'lunch at one?']);
    check('chat posts a message', r.code === 0, r.stderr);
    const heard = await until(async () => (await runJson(n2, ['chat'])).json?.messages.find((m) => m.content === 'lunch at one?'));
    check('which the other node shows', !!heard && heard.sender_name.length > 0, JSON.stringify(heard));
    r = await run(n2, ['chat']);
    check('chat prints the room for people', /lunch at one\?/.test(r.stdout), r.stdout);

    // Unsharing.
    fs.writeFileSync(path.join(ROOT, 'notes.txt'), 'notes');
    await run(n1, ['send', 'notes.txt']);
    await until(async () => (await runJson(n2, ['ls'])).json?.files.some((f) => f.name === 'notes.txt'));
    r = await run(n2, ['unshare', 'notes.txt']);
    check('a node cannot unshare what another node shared', r.code === 1 && /only that node/.test(r.stderr), r.stderr);
    r = await run(n1, ['unshare', 'notes.txt']);
    check('unshare stops sharing a file by name', r.code === 0 && /Stopped sharing notes.txt/.test(r.stdout), r.stdout + r.stderr);
    const isUnlisted = await until(async () => !(await runJson(n2, ['ls'])).json?.files.some((f) => f.name === 'notes.txt'));
    check('and the other node no longer lists it', !!isUnlisted);

    // Guests.
    r = await runJson(n1, ['guests']);
    check('guests lists nobody before anyone joins', r.code === 0 && r.json?.guests.length === 0 && r.json.uploads.length === 0, r.stdout);
    const code = (await runJson(n1, ['invite'])).json.guest.code;
    const joined = await fetch(`http://${n1.base}/join`, { method: 'POST', headers: { 'Content-Type': 'application/json', Origin: `http://${n1.base}` }, body: JSON.stringify({ code }) });
    check('a guest joins with the invite code', joined.ok, joined.status);
    r = await runJson(n1, ['guests']);
    const visitor = r.json?.guests[0];
    check('guests lists the guest', r.json?.guests.length === 1 && visitor.permission === 'read-only', r.stdout);
    r = await run(n1, ['guests', 'sign-out', visitor?.id ?? '']);
    check('guests sign-out signs it out', r.code === 0 && (await runJson(n1, ['guests'])).json.guests.length === 0, r.stdout + r.stderr);

    // Changing the passphrase on a running node.
    r = await run(n1, ['passphrase', 'set', '--no-keychain'], { input: 'another-long-mesh-secret\n' });
    check('passphrase set on a running node saves it for its next start', r.code === 0 && /once it restarts/.test(r.stdout), r.stdout + r.stderr);

    // Pairing and trust, as on the app's Network page. Nobody is at a terminal here, so the codes are refused.
    const listening = start(n1, ['pair']);
    await until(() => /Waiting for the other device/.test(listening.out.stderr));
    const dialing = await within(run(n2, ['pair', `127.0.0.1:${n1.port}`]), 30000);
    const listened = await within(listening.done, 30000);
    check('pair shows both sides the same six words', !!dialing && /these same words/.test(dialing.stderr) && /these same words/.test(listened?.stderr ?? ''), JSON.stringify(dialing) + JSON.stringify(listened));
    const words = (text) => text.match(/these same words:\s+([a-z ]+)/)?.[1].trim();
    check('and they are the same on both', !!dialing && words(dialing.stderr) === words(listened?.stderr ?? ''), JSON.stringify(dialing));
    check('with nobody to compare them, pairing is refused and both commands fail', dialing?.code === 1 && listened?.code === 1 && /Not paired/.test(dialing.stderr), JSON.stringify(dialing) + JSON.stringify(listened));

    r = await runJson(n1, ['trust']);
    const other = r.json?.nodes?.[0];
    check('trust lists the other node, trusted through the passphrase', r.json?.nodes.length === 1 && other.trusted_via === 'passphrase' && other.connected === true, r.stdout);
    r = await run(n1, ['trust', 'revoke', other?.node_id ?? '']);
    check('trust revoke revokes it', r.code === 0 && /Revoked/.test(r.stdout), r.stdout + r.stderr);
    const isDropped = await until(async () => (await runJson(n1, ['devices'])).json?.nodes.length === 0);
    check('and the revoked node is dropped from the mesh', !!isDropped);
    r = await runJson(n1, ['trust']);
    check('trust then shows it revoked by this device', r.json?.nodes[0]?.revoked_by === 'this device', r.stdout);

    for (const proc of procs) proc.kill('SIGKILL');
    console.log(failures === 0 ? '\nALL PASSED' : `\n${failures} FAILED`);
    if (!process.env.LADEX_KEEP) fs.rmSync(ROOT, { recursive: true, force: true }); else console.log('kept', ROOT);
    process.exit(failures ? 1 : 0);
})().catch((e) => { console.error('test crashed:', e); for (const proc of procs) proc.kill('SIGKILL'); process.exit(2); });
