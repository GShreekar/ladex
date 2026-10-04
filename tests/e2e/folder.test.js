// End-to-end test against real LADEX processes. Needs a built binary
// (cargo build; override with LADEX_BIN) and Node 22+. Run: node tests/e2e/folder.test.js
const { spawn, execFileSync } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const BIN = process.env.LADEX_BIN || path.join(__dirname, '..', '..', 'target', 'debug', 'ladex');
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), 'ladex-e2e-folder-')); const PASS = 'correct-horse-battery'; const MiB = 1048576;
const procs = [];
function start(n, extra = []) {
    fs.mkdirSync(path.join(ROOT, `n${n}/home`), { recursive: true });
    const port = 9200 + n * 10; const log = fs.openSync(path.join(ROOT, `n${n}.log`), 'w');
    procs.push(spawn(BIN, [PASS, '--port', String(port), '--no-discovery', '--no-keychain', '--data-dir', path.join(ROOT, `n${n}/files`), '--stall-timeout-secs', '10', ...extra], { env: { ...process.env, HOME: path.join(ROOT, `n${n}/home`) }, stdio: ['ignore', log, log] }));
    return `localhost:${port + 1}`;
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sha = (b) => crypto.createHash('sha256').update(b).digest('hex');
const login = async (b) => (await fetch(`http://${b}/auth`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ passphrase: PASS }) })).headers.get('set-cookie').match(/auth=([0-9a-f]+)/)[1];
function join(base, token, session) {
    return new Promise((resolve, reject) => {
        const ws = new WebSocket(`ws://${base}/ws`, { headers: { Cookie: `auth=${token}`, Origin: `http://${base}` } });
        const received = []; ws.onmessage = (e) => received.push(JSON.parse(e.data));
        const c = { ws, received, send: (m) => ws.send(JSON.stringify(m)), files: () => [...received].reverse().find((m) => m.type === 'file_list_update')?.files ?? [],
            wait: async (p, ms = 6000) => { const end = Date.now() + ms; while (Date.now() < end) { const h = received.find(p); if (h) return h; await sleep(25); } return null; } };
        ws.onopen = () => { c.send({ type: 'join', session_id: session }); resolve(c); }; ws.onerror = () => reject(new Error('ws'));
    });
}
const put = async (base, token, session, id, buf, extra = {}) => { const r = await fetch(`http://${base}/api/files/${id}`, { method: 'PUT', headers: { Cookie: `auth=${token}`, Origin: `http://${base}`, 'X-Ladex-Session': session, 'X-Ladex-Size': String(buf.length), 'X-Ladex-Name': encodeURIComponent(extra.name || id), ...(extra.parent ? { 'X-Ladex-Parent': extra.parent } : {}) }, body: buf }); return { status: r.status, body: await r.json() }; };
let failures = 0; const check = (n, ok, d = '') => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${n}${ok ? '' : '  ' + d}`); if (!ok) failures++; };

(async () => {
    fs.rmSync(ROOT, { recursive: true, force: true });
    const b1 = start(1); await sleep(1200); const b2 = start(2, ['--peer', '127.0.0.1:9210']); await sleep(2500);
    const [t1, t2] = [await login(b1), await login(b2)];
    const c1 = await join(b1, t1, 'peer_one'); const c2 = await join(b2, t2, 'peer_two'); await sleep(400);

    const files = { 'photos/a.jpg': crypto.randomBytes(3 * MiB + 11), 'photos/sub/b.bin': crypto.randomBytes(1000), 'notes/ünï cøde.txt': Buffer.from('hello'), 'empty.dat': Buffer.alloc(0) };
    const children = [];
    for (const [p, buf] of Object.entries(files)) {
        const id = 'file_' + crypto.randomBytes(8).toString('hex');
        const r = await put(b1, t1, 'peer_one', id, buf, { name: path.basename(p), parent: 'folder_x' });
        check(`child ${p} uploaded`, r.status === 201 && r.body.file.parent === 'folder_x', JSON.stringify(r));
        children.push({ path: p, file_id: id });
    }
    await sleep(300);
    check('children are in the catalog but marked with their parent (the page hides them)', c2.files().filter((f) => f.parent === 'folder_x').length === 4);

    const publish = async (id, body, token = t1, session = 'peer_one', base = b1) => (await fetch(`http://${base}/api/folders/${id}`, { method: 'PUT', headers: { Cookie: `auth=${token}`, Origin: `http://${base}`, 'X-Ladex-Session': session, 'Content-Type': 'application/json' }, body: JSON.stringify(body) }));
    let r = await publish('folder_x', { name: 'my folder', children: [...children, { path: 'x', file_id: 'file_nope' }] });
    check('a folder naming a file that was not uploaded for it is refused', r.status === 409, r.status);
    r = await publish('folder_x', { name: 'my folder', children: [children[0], { path: 'PHOTOS/A.JPG', file_id: children[1].file_id }] });
    check('two files with the same path (ignoring case) are refused', r.status === 400, r.status);
    r = await publish('folder_x', { name: 'my folder', children: [{ path: '../../etc/passwd', file_id: children[0].file_id }, ...children.slice(1)] });
    const published = await r.json();
    check('a path that tries to climb out is cleaned, not obeyed', r.status === 201, JSON.stringify(published));
    r = await publish('folder_y', { name: 'x', children: children });
    check('the children belong to one folder only', r.status === 409, r.status);

    const folder = published.file;
    check('the folder entry has its counts', folder.is_folder && folder.folder_files === 4 && folder.folder_bytes === Object.values(files).reduce((a, b) => a + b.length, 0), JSON.stringify(folder));
    check('node 2 lists the folder', !!(await c2.wait((m) => m.type === 'file_list_update' && m.files.some((f) => f.id === 'folder_x' && f.is_folder))));

    // the listing and the zip, from node 2 (which has nothing yet)
    const listing = await (await fetch(`http://${b2}/api/files/folder_x`, { headers: { Cookie: `auth=${t2}` } })).json();
    check('the listing is served like any file', listing.v === 1 && listing.children.length === 4 && listing.children.some((c) => c.path === 'etc/passwd'), JSON.stringify(listing).slice(0, 200));

    const t0 = Date.now();
    const z = await fetch(`http://${b2}/api/folders/folder_x.zip`, { headers: { Cookie: `auth=${t2}` } });
    const zipBytes = Buffer.from(await z.arrayBuffer());
    check('the zip comes from node 2 (fetching from node 1 as it streams)', z.status === 200 && z.headers.get('content-type') === 'application/zip', z.status);
    check('its announced length is exact', Number(z.headers.get('content-length')) === zipBytes.length, `${z.headers.get('content-length')} vs ${zipBytes.length}`);
    fs.writeFileSync(path.join(ROOT, 'out.zip'), zipBytes);
    const check_py = `
import zipfile, hashlib, json, sys
z = zipfile.ZipFile(sys.argv[1])
assert z.testzip() is None, "bad crc"
print(json.dumps({n: hashlib.sha256(z.read(n)).hexdigest() for n in z.namelist()}))`;
    let listed = {};
    try { listed = JSON.parse(execFileSync('python3', ['-c', check_py, path.join(ROOT, 'out.zip')]).toString()); } catch (e) { console.log(String(e.stderr || e)); }
    const expected = { 'photos/a.jpg': files['photos/a.jpg'], 'photos/sub/b.bin': files['photos/sub/b.bin'], 'notes/ünï cøde.txt': files['notes/ünï cøde.txt'], 'empty.dat': files['empty.dat'] };
    delete expected['photos/a.jpg']; expected['etc/passwd'] = files['photos/a.jpg'];
    check('python verifies every CRC and reads the same files back', Object.keys(expected).length === Object.keys(listed).length && Object.entries(expected).every(([n, b]) => listed[n] === sha(b)), JSON.stringify(Object.keys(listed)));
    try { execFileSync('unzip', ['-tq', path.join(ROOT, 'out.zip')]); check('unzip -t agrees', true); } catch (e) { check('unzip -t agrees', String(e.message).includes('not found') || String(e.code) === 'ENOENT', String(e.stdout || e.message)); }

    const head = await fetch(`http://${b1}/api/folders/folder_x.zip`, { method: 'HEAD', headers: { Cookie: `auth=${t1}` } });
    check('HEAD gives the same length without a body', Number(head.headers.get('content-length')) === zipBytes.length);
    const noAuth = await fetch(`http://${b1}/api/folders/folder_x.zip`);
    check('the zip needs a login', noAuth.status === 401);
    const missing = await fetch(`http://${b1}/api/folders/folder_nope.zip`, { headers: { Cookie: `auth=${t1}` } });
    check('an unknown folder is a 404', missing.status === 404);

    // unsharing the folder removes its files everywhere
    c1.send({ type: 'delete_file', session_id: 'peer_one', file_id: 'folder_x' });
    await sleep(1500);
    const left = fs.readdirSync(path.join(ROOT, 'n1/files')).filter((f) => f.endsWith('.data'));
    const left2 = fs.readdirSync(path.join(ROOT, 'n2/files')).filter((f) => f.endsWith('.data'));
    check('deleting the folder deletes its files on both nodes', left.length === 0 && left2.length === 0, `${left} | ${left2}`);
    check('and they leave the catalog', c2.files().filter((f) => f.parent === 'folder_x' || f.id === 'folder_x').length === 0);

    procs.forEach((p) => p.kill('SIGKILL'));
    console.log(failures === 0 ? '\nALL PASSED' : `\n${failures} FAILED`); fs.rmSync(ROOT, { recursive: true, force: true });
    process.exit(failures ? 1 : 0);
})().catch((e) => { console.error('crashed', e); procs.forEach((p) => p.kill('SIGKILL')); process.exit(2); });
