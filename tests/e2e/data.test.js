// End-to-end test against real LADEX processes. Needs a built binary
// (cargo build; override with LADEX_BIN) and Node 22+. Run: node tests/e2e/data.test.js
const { spawn } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');

const os = require('node:os');
const BIN = process.env.LADEX_BIN || path.join(__dirname, '..', '..', 'target', 'debug', 'ladex');
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), 'ladex-e2e-'));
const PASS = 'correct-horse-battery';
const MiB = 1024 * 1024;

const procs = {};
function startNode(n, extraArgs = []) {
    const dir = path.join(ROOT, `n${n}`);
    fs.mkdirSync(path.join(dir, 'home'), { recursive: true });
    const port = 9000 + n * 10;
    const args = [PASS, '--port', String(port), '--no-discovery', '--data-dir', path.join(dir, 'files'), '--stall-timeout-secs', '8', ...extraArgs];
    const log = fs.openSync(path.join(ROOT, `n${n}.log`), 'a');
    procs[n] = spawn(BIN, args, { env: { ...process.env, HOME: path.join(dir, 'home') }, stdio: ['ignore', log, log] });
    return { port, base: `localhost:${port + 1}` };
}
const stopNode = async (n) => { procs[n].kill('SIGKILL'); await new Promise((r) => procs[n].once('exit', r)); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sha = (buf) => crypto.createHash('sha256').update(buf).digest('hex');

async function login(base) {
    const r = await fetch(`http://${base}/auth`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ passphrase: PASS }) });
    return r.headers.get('set-cookie').match(/auth=([0-9a-f]+)/)[1];
}

function join(base, token, session) {
    return new Promise((resolve, reject) => {
        const ws = new WebSocket(`ws://${base}/ws`, { headers: { Cookie: `auth=${token}`, Origin: `http://${base}` } });
        const received = [];
        ws.onmessage = (e) => received.push(JSON.parse(e.data));
        const c = {
            ws, received,
            send: (m) => ws.send(JSON.stringify(m)),
            files: () => [...received].reverse().find((m) => m.type === 'file_list_update')?.files ?? [],
            wait: async (pred, ms = 5000) => { const end = Date.now() + ms; while (Date.now() < end) { const h = received.find(pred); if (h) return h; await sleep(25); } return null; },
        };
        ws.onopen = () => { c.send({ type: 'join', session_id: session }); resolve(c); };
        ws.onerror = () => reject(new Error('ws error'));
    });
}

const headersFor = (token, session, base, extra = {}) => ({
    Cookie: `auth=${token}`, Origin: `http://${base}`, 'X-Ladex-Session': session, ...extra,
});

async function upload(base, token, session, id, buf, { name = 'data.bin', offset = 0, parent } = {}) {
    const headers = headersFor(token, session, base, {
        'X-Ladex-Size': String(buf.length), 'X-Ladex-Offset': String(offset), 'X-Ladex-Name': encodeURIComponent(name),
        'Content-Type': 'application/octet-stream', ...(parent ? { 'X-Ladex-Parent': parent } : {}),
    });
    const r = await fetch(`http://${base}/api/files/${id}`, { method: 'PUT', headers, body: buf.subarray(offset) });
    return { status: r.status, body: await r.json().catch(() => ({})) };
}

async function download(base, token, id, range) {
    const t0 = Date.now();
    const r = await fetch(`http://${base}/api/files/${id}`, { headers: { Cookie: `auth=${token}`, ...(range ? { Range: range } : {}) } });
    if (!r.ok && r.status !== 206) return { status: r.status, body: await r.text() };
    try {
        const buf = Buffer.from(await r.arrayBuffer());
        return { status: r.status, headers: r.headers, buf, ms: Date.now() - t0 };
    } catch (e) {
        return { status: r.status, headers: r.headers, error: String(e), ms: Date.now() - t0 };
    }
}

let failures = 0;
const check = (name, ok, detail = '') => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok ? '' : '  ' + detail}`); if (!ok) failures++; };

(async () => {
    for (const n of [1, 2, 3]) { fs.rmSync(path.join(ROOT, `n${n}`), { recursive: true, force: true }); fs.rmSync(path.join(ROOT, `n${n}.log`), { force: true }); }
    const n1 = startNode(1);
    await sleep(1200);
    const n2 = startNode(2, ['--peer', '127.0.0.1:9010']);
    const n3 = startNode(3, ['--peer', '127.0.0.1:9010', '--peer', '127.0.0.1:9020']);
    await sleep(2500);

    const [t1, t2, t3] = [await login(n1.base), await login(n2.base), await login(n3.base)];
    const [c1, c2, c3] = [await join(n1.base, t1, 'peer_one'), await join(n2.base, t2, 'peer_two'), await join(n3.base, t3, 'peer_three')];
    await sleep(500);

    // ---- upload to node 1
    const fileA = crypto.randomBytes(24 * MiB + 12345);
    let r = await upload(n1.base, t1, 'peer_one', 'file_a', fileA, { name: 'photo ünï.bin' });
    check('upload to node 1 is accepted', r.status === 201 && r.body.file && r.body.file.id === 'file_a', JSON.stringify(r));
    const rootA = r.body.file && r.body.file.manifest_root;
    check('the entry names node 1 as holder and has a manifest root', r.body.file && Object.keys(r.body.file.holders).length === 1 && /^[0-9a-f]{64}$/.test(r.body.file.manifest_root));
    check('node 2 and node 3 list it', !!(await c2.wait((m) => m.type === 'file_list_update' && m.files.some((f) => f.id === 'file_a'))) && !!(await c3.wait((m) => m.type === 'file_list_update' && m.files.some((f) => f.id === 'file_a'))));
    r = await upload(n1.base, t1, 'peer_one', 'file_a', fileA, { name: 'photo ünï.bin' });
    check('uploading the same file again is harmless', r.status === 200, JSON.stringify(r.body));

    // ---- download from node 1 itself
    let d = await download(n1.base, t1, 'file_a');
    check('node 1 serves its own file intact', d.status === 200 && sha(d.buf) === sha(fileA));
    check('download headers are safe', d.headers.get('content-type') === 'application/octet-stream' && d.headers.get('x-content-type-options') === 'nosniff' && d.headers.get('content-security-policy') === 'sandbox' && /^attachment; filename="photo _n_.bin"; filename\*=UTF-8''photo%20%C3%BCn%C3%AF\.bin$/.test(d.headers.get('content-disposition')), d.headers.get('content-disposition'));

    // ---- download from node 2: it has nothing yet and streams it through while fetching
    d = await download(n2.base, t2, 'file_a');
    check('node 2 streams the file through while fetching it from node 1', d.status === 200 && sha(d.buf) === sha(fileA), d.error || '');
    check('content length is exact', d.buf && d.buf.length === fileA.length);
    check('node 2 keeps a copy on disk', fs.existsSync(path.join(ROOT, 'n2/files/file_a.data')));
    const listed = await c1.wait((m) => m.type === 'file_list_update' && m.files.some((f) => f.id === 'file_a' && Object.values(f.holders).filter((h) => h.present).length === 2), 8000);
    check('everyone learns node 2 is now a holder', !!listed);

    // ---- ranges, from node 3 (cold)
    d = await download(n3.base, t3, 'file_a', 'bytes=10000000-10000099');
    check('a range request on a cold node returns exactly those bytes', d.status === 206 && d.buf.equals(fileA.subarray(10000000, 10000100)) && d.headers.get('content-range') === `bytes 10000000-10000099/${fileA.length}`, `${d.status} ${d.headers && d.headers.get('content-range')}`);
    d = await download(n3.base, t3, 'file_a', 'bytes=-1000');
    check('a suffix range returns the tail', d.status === 206 && d.buf.equals(fileA.subarray(fileA.length - 1000)));
    d = await download(n3.base, t3, 'file_a', `bytes=${fileA.length}-`);
    check('an unsatisfiable range is 416', d.status === 416, d.status);
    const headA = await fetch(`http://${n3.base}/api/files/file_a`, { method: 'HEAD', headers: { Cookie: `auth=${t3}` } });
    check('downloads carry the manifest root as a strong ETag', headA.headers.get('etag') === `"${rootA}"`, headA.headers.get('etag'));
    const conditional = async (ifRange) => { const rr = await fetch(`http://${n3.base}/api/files/file_a`, { headers: { Cookie: `auth=${t3}`, Range: 'bytes=0-9', 'If-Range': ifRange } }); const buf = Buffer.from(await rr.arrayBuffer()); return { status: rr.status, length: buf.length }; };
    let cr = await conditional(`"${rootA}"`);
    check('a conditional range for the unchanged file is honoured (so browsers can resume)', cr.status === 206 && cr.length === 10, JSON.stringify(cr));
    cr = await conditional('"something-else"');
    check('a conditional range for a different version sends the whole file instead', cr.status === 200 && cr.length === fileA.length, JSON.stringify(cr));
    d = await download(n3.base, t3, 'file_a');
    check('node 3 then gets the whole file', d.status === 200 && sha(d.buf) === sha(fileA));

    // ---- the file outlives its uploader's node
    await stopNode(1);
    await sleep(1500);
    // a fourth reader has to take it from the surviving nodes: drop node 3's copy knowledge by asking via node 2 after node 1 is gone
    d = await download(n2.base, t2, 'file_a');
    check('with node 1 gone, node 2 still serves the file', d.status === 200 && sha(d.buf) === sha(fileA));
    d = await download(n3.base, t3, 'file_a', 'bytes=100-199');
    check('and so does node 3', d.status === 206 && d.buf.equals(fileA.subarray(100, 200)));

    // ---- a file that only the missing node has
    const n1b = startNode(1);
    await sleep(2500);
    const t1b = await login(n1b.base);
    const c1b = await join(n1b.base, t1b, 'peer_one');
    const fileB = crypto.randomBytes(6 * MiB + 1);
    r = await upload(n1b.base, t1b, 'peer_one', 'file_b', fileB, { name: 'only-here.bin' });
    check('node 1 (restarted) accepts a new upload', r.status === 201, JSON.stringify(r.body));
    check('node 1 still lists the earlier file after its restart (from disk)', c1b.files().some((f) => f.id === 'file_a') || !!(await c1b.wait((m) => m.type === 'file_list_update' && m.files.some((f) => f.id === 'file_a'))));
    await sleep(800);
    await stopNode(1);
    await sleep(1500);
    d = await download(n2.base, t2, 'file_b');
    check('a file whose only holder is offline is reported as unavailable (503)', d.status === 503, `${d.status}`);

    // ---- resume an interrupted upload
    const n1c = startNode(1);
    await sleep(2500);
    const t1c = await login(n1c.base);
    const c1c = await join(n1c.base, t1c, 'peer_one');
    const fileC = crypto.randomBytes(9 * MiB + 77);
    await new Promise((resolve) => {
        const req = http.request({ host: 'localhost', port: n1c.port + 1, path: '/api/files/file_c', method: 'PUT', headers: { ...headersFor(t1c, 'peer_one', n1c.base), 'X-Ladex-Size': String(fileC.length), 'X-Ladex-Name': 'resume.bin', 'Content-Length': String(fileC.length), 'Content-Type': 'application/octet-stream' } });
        req.on('error', () => resolve());
        req.write(fileC.subarray(0, 4 * MiB + 500));
        setTimeout(() => { req.destroy(); resolve(); }, 600);
    });
    await sleep(300);
    const st = await (await fetch(`http://${n1c.base}/api/files/file_c/upload?size=${fileC.length}`, { headers: { Cookie: `auth=${t1c}` } })).json();
    check('the node reports how much of the interrupted upload it kept', st.offset === 4 * MiB && st.complete === false, JSON.stringify(st));
    r = await upload(n1c.base, t1c, 'peer_one', 'file_c', fileC, { name: 'resume.bin', offset: 9 * MiB });
    check('skipping ahead of what the node has is refused, with the right offset', r.status === 409 && r.body.offset === 4 * MiB, JSON.stringify(r));
    r = await upload(n1c.base, t1c, 'peer_one', 'file_c', fileC, { name: 'resume.bin', offset: st.offset });
    check('resuming from that offset completes the upload', r.status === 201, JSON.stringify(r));
    d = await download(n1c.base, t1c, 'file_c');
    check('and the resumed file is byte-for-byte intact', d.status === 200 && sha(d.buf) === sha(fileC));

    // ---- who may do what
    r = await upload(n1c.base, 'deadbeef', 'peer_one', 'file_x', fileC.subarray(0, 100), {});
    check('upload without a login is refused', r.status === 401);
    r = await upload(n1c.base, t1c, 'peer_nobody', 'file_x', fileC.subarray(0, 100), {});
    check('upload for a device that is not connected is refused', r.status === 403, JSON.stringify(r));
    const noOrigin = await fetch(`http://${n1c.base}/api/files/file_x`, { method: 'PUT', headers: { Cookie: `auth=${t1c}`, 'X-Ladex-Size': '1', 'X-Ladex-Session': 'peer_one' }, body: Buffer.from('x') });
    check('upload without an Origin header is refused', noOrigin.status === 403, noOrigin.status);
    r = await upload(n1c.base, t1c, 'peer_one', '../evil', Buffer.from('x'), {});
    check('a path-like file id is refused', r.status === 400 || r.status === 404, r.status);
    d = await download(n1c.base, 'deadbeef', 'file_c');
    check('download without a login is refused', d.status === 401);
    d = await download(n1c.base, t1c, 'file_nope');
    check('an unknown file is a 404', d.status === 404);

    // ---- unsharing deletes everywhere
    c1c.send({ type: 'delete_file', session_id: 'peer_one', file_id: 'file_a' });
    await sleep(1500);
    check('unsharing removes the file from node 2 and node 3 disks', !fs.existsSync(path.join(ROOT, 'n2/files/file_a.data')) && !fs.existsSync(path.join(ROOT, 'n3/files/file_a.data')));
    d = await download(n2.base, t2, 'file_a');
    check('and it can no longer be downloaded', d.status === 404, d.status);

    // ---- a corrupted copy on disk is never passed on
    const fileD = crypto.randomBytes(4 * MiB);
    r = await upload(n1c.base, t1c, 'peer_one', 'file_d', fileD, { name: 'd.bin' });
    check('upload for the corruption test', r.status === 201);
    await sleep(500);
    const fd = fs.openSync(path.join(ROOT, 'n1/files/file_d.data'), 'r+');
    fs.writeSync(fd, Buffer.from([fileD[2 * MiB + 5] ^ 0xff]), 0, 1, 2 * MiB + 5); // flip a byte in chunk 2
    fs.closeSync(fd);
    d = await download(n3.base, t3, 'file_d');
    check('a corrupted source never delivers corrupted bytes', !(d.buf && d.buf.length === fileD.length && sha(d.buf) !== sha(fileD)), `status ${d.status} len ${d.buf && d.buf.length}`);
    d = await download(n1c.base, t1c, 'file_d');
    check('a node will not hand its own damaged chunk to a browser either', !(d.buf && d.buf.length === fileD.length && sha(d.buf) !== sha(fileD)) && (d.error !== undefined || (d.buf && d.buf.length < fileD.length) || d.status !== 200), `status ${d.status} len ${d.buf && d.buf.length}`);
    check('the download failed instead (stalled/aborted)', d.error !== undefined || (d.buf && d.buf.length < fileD.length) || d.status !== 200, `status ${d.status}`);

    for (const n of [1, 2, 3]) await stopNode(n).catch(() => {});
    console.log(failures === 0 ? '\nALL PASSED' : `\n${failures} FAILED`);
    fs.rmSync(ROOT, { recursive: true, force: true });
    process.exit(failures ? 1 : 0);
})().catch(async (e) => { console.error('test crashed:', e); for (const n of Object.keys(procs)) procs[n].kill('SIGKILL'); process.exit(2); });
