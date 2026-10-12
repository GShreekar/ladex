// The command-line commands, against real LADEX processes; needs `cargo build` (or LADEX_BIN) and Node 22+. Run: node tests/e2e/cli.test.js
const { spawn } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const BIN = process.env.LADEX_BIN || path.join(__dirname, '..', '..', 'target', 'debug', 'ladex');
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), 'ladex-cli-'));
const PASS = 'correct-horse-battery';
const MiB = 1024 * 1024;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const procs = {};
function startNode(n, extraArgs = []) {
    const dir = path.join(ROOT, `n${n}`);
    fs.mkdirSync(path.join(dir, 'home'), { recursive: true });
    const port = 9500 + n * 10;
    const args = [PASS, '--port', String(port), '--no-discovery', '--no-keychain', '--data-dir', path.join(dir, 'files'), ...extraArgs];
    const log = fs.openSync(path.join(ROOT, `n${n}.log`), 'a');
    procs[n] = spawn(BIN, args, { env: { ...process.env, HOME: path.join(dir, 'home') }, stdio: ['ignore', log, log] });
    return { base: `localhost:${port + 1}`, files: path.join(dir, 'files') };
}

// Starts a command against a node; `done` resolves with its exit code and what it printed.
function start(node, args, cwd = ROOT) {
    const child = spawn(BIN, [...args, '--data-dir', node.files], { cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    const out = { stdout: '', stderr: '' };
    child.stdout.on('data', (d) => { out.stdout += d; });
    child.stderr.on('data', (d) => { out.stderr += d; });
    const done = new Promise((resolve) => child.on('close', (code) => resolve({ code, ...out })));
    return { child, out, done };
}
const run = (node, args, cwd) => start(node, args, cwd).done;
const runJson = async (node, args, cwd) => { const r = await run(node, [...args, '--json'], cwd); return { ...r, json: JSON.parse(r.stdout || 'null') }; };
const until = async (pred, ms = 10000) => { const end = Date.now() + ms; while (Date.now() < end) { const hit = await pred(); if (hit) return hit; await sleep(100); } return null; };

async function login(base) {
    const r = await fetch(`http://${base}/auth`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ passphrase: PASS }) });
    return r.headers.get('set-cookie').match(/auth=([0-9a-f]+)/)[1];
}

function join(base, token, session, nickname) {
    return new Promise((resolve, reject) => {
        const ws = new WebSocket(`ws://${base}/ws`, { headers: { Cookie: `auth=${token}`, Origin: `http://${base}` } });
        const received = [];
        ws.onmessage = (e) => received.push(JSON.parse(e.data));
        const tab = {
            received,
            send: (m) => ws.send(JSON.stringify({ ...m, session_id: session })),
            wait: (pred, ms = 8000) => until(() => received.find(pred), ms),
        };
        ws.onopen = () => { tab.send({ type: 'join', nickname }); resolve(tab); };
        ws.onerror = () => reject(new Error('ws error'));
    });
}

async function fetchFile(base, token, id, device) {
    const r = await fetch(`http://${base}/api/files/${id}?to=${device}`, { headers: { Cookie: `auth=${token}` } });
    return { status: r.status, buf: r.ok ? Buffer.from(await r.arrayBuffer()) : null };
}

let failures = 0;
const check = (name, ok, detail = '') => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok ? '' : '  ' + detail}`); if (!ok) failures++; };

(async () => {
    const n1 = startNode(1);
    await sleep(1200);
    const n2 = startNode(2, ['--peer', '127.0.0.1:9510']);
    await sleep(2500);

    const src = path.join(ROOT, 'src');
    fs.mkdirSync(path.join(src, 'album', 'disc 1'), { recursive: true });
    const big = crypto.randomBytes(3 * MiB + 99);
    fs.writeFileSync(path.join(src, 'big.bin'), big);
    fs.writeFileSync(path.join(src, 'album', 'cover.txt'), 'cover');
    fs.writeFileSync(path.join(src, 'album', 'disc 1', 'track.txt'), 'track');

    let r = await run(n1, ['ls']);
    check('ls on a node sharing nothing says so', r.code === 0 && /Nothing is shared/.test(r.stdout), r.stdout + r.stderr);

    r = await runJson(n1, ['send', 'src/big.bin', 'src/album']);
    const [sharedFile, sharedFolder] = r.json?.files || [];
    check('send shares a file and a folder, given relative paths', r.code === 0 && sharedFile?.name === 'big.bin' && sharedFile.size === big.length && sharedFolder?.is_folder && sharedFolder.folder_files === 2, r.stdout + r.stderr);

    const listed = await until(async () => { const l = await runJson(n2, ['ls']); return l.json?.files.length === 2 && l.json.files.every((f) => f.hosts.length > 0) ? l : null; });
    check('the other node lists both, without the files inside the folder', !!listed && listed.json.files.map((f) => f.name).join() === 'album,big.bin', JSON.stringify(listed?.json));
    r = await runJson(n2, ['ls', '--all']);
    check('ls --all also lists the files inside the folder', r.json?.files.length === 4, r.stdout);
    r = await run(n2, ['ls']);
    check('ls prints a table for people', /NAME\s+KIND\s+SIZE/.test(r.stdout) && /album\s+folder/.test(r.stdout), r.stdout);

    const out = path.join(ROOT, 'out');
    fs.mkdirSync(out);
    r = await runJson(n2, ['get', 'big.bin'], out);
    check('get saves a file by name into the current directory', r.code === 0 && r.json?.path === path.join(fs.realpathSync(out), 'big.bin') && fs.readFileSync(path.join(out, 'big.bin')).equals(big), r.stdout + r.stderr);
    r = await runJson(n2, ['get', sharedFile.id, '-o', out]);
    check('getting it again by id keeps the first copy', r.code === 0 && path.basename(r.json?.path || '') === 'big (1).bin', r.stdout + r.stderr);
    r = await run(n2, ['get', 'album', '-o', path.join(out, 'new dir')]);
    const track = path.join(out, 'new dir', 'album', 'disc 1', 'track.txt');
    check('get saves a folder as real directories', r.code === 0 && fs.existsSync(track) && fs.readFileSync(track, 'utf8') === 'track', r.stdout + r.stderr);
    r = await runJson(n2, ['get', 'no-such-file']);
    check('getting something that is not shared fails with a reason', r.code === 1 && /nothing called/.test(r.json?.error || ''), r.stdout + r.stderr);

    const token = await login(n2.base);
    const bob = await join(n2.base, token, 'peer_bob', 'Bob');
    const seen = await until(async () => { const d = await runJson(n1, ['devices']); return d.json?.devices.some((x) => x.id === 'peer_bob') ? d : null; });
    const bobSeen = seen?.json.devices.find((x) => x.id === 'peer_bob');
    check('devices lists a device on another node, by name', bobSeen?.name === 'Bob' && bobSeen.is_here === false && seen.json.nodes.length === 1, JSON.stringify(seen?.json));

    let sending = start(n1, ['send', 'src/big.bin', '--to', 'bob', '--json']);
    let offer = await bob.wait((m) => m.type === 'incoming_file_offer' && m.pushed);
    check('send --to asks the device, named without regard to case', offer?.pushed?.name === 'big.bin' && offer.pushed.size === big.length, JSON.stringify(offer));
    check('the file is not listed for everyone', (await runJson(n2, ['ls', '--all'])).json.files.length === 4);
    bob.send({ type: 'accept_file_offer', file_id: offer.file_id });
    const got = await until(async () => { const f = await fetchFile(n2.base, token, offer.file_id, 'peer_bob'); return f.status === 200 ? f : null; });
    check('once accepted, the device gets the file intact', !!got && got.buf.equals(big));
    r = await sending.done;
    check('and the command ends, reporting the delivery', r.code === 0 && JSON.parse(r.stdout).delivered.join() === 'big.bin', r.stdout + r.stderr);

    sending = start(n1, ['send', 'src/album/cover.txt', '--to', 'peer_bob']);
    offer = await bob.wait((m) => m.type === 'incoming_file_offer' && m.pushed?.name === 'cover.txt');
    bob.send({ type: 'decline_file_offer', target_session_id: offer?.from_session_id, file_id: offer?.file_id });
    r = await sending.done;
    check('a declined file makes the command fail', r.code === 1 && /declined/.test(r.stderr), r.stdout + r.stderr);
    r = await runJson(n1, ['devices']);
    check('the command line is no longer listed as a device afterwards', r.json.devices.length === 1, r.stdout);
    r = await run(n1, ['send', 'src/album', '--to', 'bob']);
    check('a folder cannot be sent to one device', r.code === 1 && /only files/.test(r.stderr), r.stderr);
    r = await run(n1, ['send', 'src/big.bin', '--to', 'nobody']);
    check('nor anything to a device that is not there', r.code === 1 && /no device called/.test(r.stderr), r.stderr);
    r = await run(n1, ['send', 'src/missing.bin']);
    check('a path that does not exist is refused', r.code === 1 && /could not find/.test(r.stderr), r.stderr);

    const watched = path.join(src, 'watched');
    fs.mkdirSync(watched);
    fs.writeFileSync(path.join(watched, 'one.txt'), 'one');
    const seeding = start(n1, ['seed', 'src/watched', '--watch', '--json']);
    const firstLine = await until(() => seeding.out.stdout.includes('\n') && seeding.out.stdout);
    const first = JSON.parse(firstLine.split('\n')[0]).files[0];
    check('seed shares the folder', first?.name === 'watched' && first.folder_files === 1, firstLine);
    fs.writeFileSync(path.join(watched, 'two.txt'), 'two');
    const secondLine = await until(() => seeding.out.stdout.trim().split('\n').length > 1 && seeding.out.stdout, 20000);
    const second = secondLine && JSON.parse(secondLine.trim().split('\n')[1]).files[0];
    check('seed --watch shares it again when a file is added', second?.folder_files === 2 && second.id !== first.id, seeding.out.stdout + seeding.out.stderr);
    const names = (await runJson(n1, ['ls'])).json.files.filter((f) => f.name === 'watched');
    check('and takes the earlier version down', names.length === 1 && names[0].id === second?.id, JSON.stringify(names));
    seeding.child.kill('SIGINT');
    await seeding.done;
    check('the folder stays shared after the watching stops', (await runJson(n1, ['ls'])).json.files.some((f) => f.id === second?.id));

    r = await runJson(n1, ['invite']);
    check('invite gives the address, the certificate fingerprint and the --peer address', /^https:\/\/.+:9510$/.test(r.json?.url || '') && /^([0-9A-F]{2}:){31}[0-9A-F]{2}$/.test(r.json.certificate_fingerprint || '') && r.json.needs_passphrase === true && /:9510$/.test(r.json.peer_address || ''), r.stdout);
    r = await run(n1, ['invite']);
    check('and draws a QR code for people', /Scan to open/.test(r.stdout) && /█|▀|▄/.test(r.stdout));

    r = await runJson(n1, ['doctor']);
    const verdicts = Object.fromEntries((r.json?.checks || []).map((c) => [c.name, c.verdict]));
    check('doctor finds a running, reachable node with a peer', r.code === 0 && verdicts['Node'] === 'ok' && verdicts['Page for other devices'] === 'ok' && verdicts['Other nodes'] === 'ok', r.stdout);
    check('and warns that discovery is off', verdicts['Discovery'] === 'warning', r.stdout);

    const endpoint = path.join(n1.files, 'control.json');
    check('the control token is readable only by its owner', process.platform === 'win32' || (fs.statSync(endpoint).mode & 0o777) === 0o600);
    fs.mkdirSync(path.join(ROOT, 'stranger'));
    fs.writeFileSync(path.join(ROOT, 'stranger', 'control.json'), JSON.stringify({ ...JSON.parse(fs.readFileSync(endpoint)), token: '00'.repeat(32) }));
    r = await run({ files: path.join(ROOT, 'stranger') }, ['ls']);
    check('a command with the wrong token is refused', r.code === 1 && /may not control/.test(r.stderr), r.stderr);

    procs[1].kill('SIGINT');
    await until(() => !fs.existsSync(endpoint));
    r = await run(n1, ['ls']);
    check('with the node stopped, a command says none is running', r.code === 1 && /no LADEX node is running/.test(r.stderr), r.stderr);
    r = await runJson(n1, ['doctor']);
    check('and doctor reports that as a problem', r.code === 1 && r.json?.checks[0].verdict === 'problem', r.stdout);
    procs[2].kill('SIGKILL');
    await sleep(300);
    r = await run(n2, ['ls']);
    check('a node that was killed is also reported as not running', r.code === 1 && /no LADEX node is running/.test(r.stderr), r.stderr);

    for (const n of Object.keys(procs)) procs[n].kill('SIGKILL');
    console.log(failures === 0 ? '\nALL PASSED' : `\n${failures} FAILED`);
    if (!process.env.LADEX_KEEP) fs.rmSync(ROOT, { recursive: true, force: true }); else console.log('kept', ROOT);
    process.exit(failures ? 1 : 0);
})().catch((e) => { console.error('test crashed:', e); for (const n of Object.keys(procs)) procs[n].kill('SIGKILL'); process.exit(2); });
