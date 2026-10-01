'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const policy = require('../../static/receive-policy.js');

const cases = JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'filename_cases.json'), 'utf8'));

test('file names match the fixture shared with the Rust side', () => {
    for (const { input, expected } of cases.file_names) {
        assert.equal(policy.sanitizeFileName(input), expected, `input: ${JSON.stringify(input)}`);
    }
});

test('relative paths match the fixture', () => {
    for (const { input, expected } of cases.relative_paths) {
        assert.equal(policy.sanitizeRelativePath(input), expected, `input: ${JSON.stringify(input)}`);
    }
});

test('long names are cut to 255 bytes and keep their extension', () => {
    const name = policy.sanitizeFileName('a'.repeat(400) + '.mp4');
    assert.equal(Buffer.byteLength(name), 255);
    assert.ok(name.endsWith('.mp4'));

    const accented = policy.sanitizeFileName('é'.repeat(300));
    assert.ok(Buffer.byteLength(accented) <= 255);
    assert.ok([...accented].every((c) => c === 'é'));

    const longExtension = policy.sanitizeFileName('a.' + 'b'.repeat(300));
    assert.ok(Buffer.byteLength(longExtension) <= 255);
});

test('lone surrogates cannot reach a file name', () => {
    assert.equal(policy.sanitizeFileName('a\uD800b.txt'), 'a_b.txt');
    assert.equal(policy.sanitizeFileName('a\uDC00b.txt'), 'a_b.txt');
    assert.equal(policy.sanitizeFileName('😀.txt'), '😀.txt');
});

test('sanitizing is idempotent', () => {
    for (const input of ['con.txt', 'a\\b/c', 'trailing. .', 'photo\u202Egpj.exe', '']) {
        const once = policy.sanitizeFileName(input);
        assert.equal(policy.sanitizeFileName(once), once);
    }
});

test('paths that are too deep or too long are refused', () => {
    assert.equal(policy.sanitizeRelativePath(Array(33).fill('d').join('/')), null);
    assert.notEqual(policy.sanitizeRelativePath(Array(32).fill('d').join('/')), null);
    assert.equal(policy.sanitizeRelativePath(Array(5).fill('x'.repeat(255)).join('/')), null);
    assert.equal(policy.sanitizeRelativePath(42), null);
});

test('numbered names keep the extension and stay within the limit', () => {
    assert.equal(policy.numberedName('report.pdf', 1), 'report (1).pdf');
    assert.equal(policy.numberedName('.bashrc', 2), '.bashrc (2)');
    assert.equal(policy.numberedName('archive.tar.gz', 3), 'archive.tar (3).gz');
    const longName = 'a'.repeat(251) + '.txt';
    assert.ok(Buffer.byteLength(policy.numberedName(longName, 12)) <= 255);
    assert.ok(policy.numberedName(longName, 12).endsWith(' (12).txt'));
});

test('mime types fall back to octet-stream', () => {
    assert.equal(policy.sanitizeMime('image/png'), 'image/png');
    assert.equal(policy.sanitizeMime('inode/directory'), 'inode/directory');
    for (const bad of ['', 'png', 'text/<script>', 'a/b/c', 'text/', '/plain', 5, null, 'a/'.repeat(200)]) {
        assert.equal(policy.sanitizeMime(bad), 'application/octet-stream');
    }
});

const goodHeader = () => ({
    fileId: 'file_abc123_1700000000000',
    fileName: '../x/CON.txt',
    fileSize: 1000,
    mimeType: 'text/plain',
    totalChunks: 1,
    sha256: 'ab01'.repeat(16),
    resumeFromBytes: 0,
});

test('a file header is cleaned, not passed through', () => {
    const header = policy.parseFileHeader(goodHeader());
    assert.equal(header.fileName, '.._x_CON.txt');
    assert.equal(header.fileSize, 1000);
    assert.equal(header.sha256, 'ab01'.repeat(16));
});

test('a file header with bad numbers or ids is refused', () => {
    const bad = (change) => policy.parseFileHeader({ ...goodHeader(), ...change });
    assert.equal(bad({ fileId: '<img src=x onerror=alert(1)>' }), null);
    assert.equal(bad({ fileId: undefined }), null);
    assert.equal(bad({ fileSize: '1000' }), null);
    assert.equal(bad({ fileSize: -1 }), null);
    assert.equal(bad({ fileSize: 1.5 }), null);
    assert.equal(bad({ fileSize: Number.MAX_SAFE_INTEGER + 2 }), null);
    assert.equal(bad({ fileSize: Infinity }), null);
    assert.equal(bad({ resumeFromBytes: 2000 }), null);
    assert.equal(bad({ resumeFromBytes: -5 }), null);
    assert.equal(policy.parseFileHeader(null), null);
    assert.equal(policy.parseFileHeader('header'), null);
});

test('an odd hash or mime type is dropped rather than trusted', () => {
    const header = policy.parseFileHeader({ ...goodHeader(), sha256: 'NOTHEX', mimeType: 'x' });
    assert.equal(header.sha256, null);
    assert.equal(header.mimeType, 'application/octet-stream');
});

test('a folder manifest is cleaned, and its counts must be plain integers', () => {
    const manifest = { kind: 'folder-manifest', folderId: 'file_f1', folderName: 'photos/../x', totalBytes: 10, totalFiles: 2 };
    assert.deepEqual(policy.parseFolderManifest(manifest), { ...manifest, folderName: 'photos_.._x' });

    // totalFiles is shown in the consent dialog; a string would be HTML injection.
    for (const change of [
        { totalFiles: '<img src=x onerror=alert(1)>' },
        { totalFiles: '2' },
        { totalFiles: 1.5 },
        { totalFiles: -1 },
        { totalFiles: policy.MAX_FOLDER_FILES + 1 },
        { totalBytes: '10' },
        { totalBytes: -1 },
        { folderId: 'bad id' },
        { kind: 'file' },
    ]) {
        assert.equal(policy.parseFolderManifest({ ...manifest, ...change }), null, JSON.stringify(change));
    }
});

test('a folder entry header gets a safe path', () => {
    assert.deepEqual(policy.parseFolderEntryHeader({ relativePath: '../../etc/passwd', size: 5 }), { relativePath: 'etc/passwd', size: 5 });
    assert.equal(policy.parseFolderEntryHeader({ relativePath: '..', size: 5 }), null);
    assert.equal(policy.parseFolderEntryHeader({ relativePath: 'a', size: -1 }), null);
    assert.equal(policy.parseFolderEntryHeader({ relativePath: 'a', size: '5' }), null);
});

test('a resume may only continue from bytes we really have', () => {
    assert.equal(policy.resumeOffsetOk(0, undefined, 100), true);
    assert.equal(policy.resumeOffsetOk(50, 50, 100), true);
    assert.equal(policy.resumeOffsetOk(40, 50, 100), true);
    assert.equal(policy.resumeOffsetOk(60, 50, 100), false);
    assert.equal(policy.resumeOffsetOk(50, undefined, 100), false);
    assert.equal(policy.resumeOffsetOk(150, 150, 100), false);
});

test('a byte budget stops a sender writing past its announced size', () => {
    const budget = new policy.ByteBudget(10);
    assert.equal(budget.accept(6), 6);
    assert.equal(budget.exceeded, false);
    assert.equal(budget.accept(6), 4);
    assert.equal(budget.exceeded, true);
    assert.equal(budget.complete, true);
    assert.equal(budget.accept(1), 0);

    const resumed = new policy.ByteBudget(10, 8);
    assert.equal(resumed.accept(2), 2);
    assert.equal(resumed.exceeded, false);
});

test('a folder budget enforces the manifest and each file header', () => {
    const budget = new policy.FolderBudget(2, 10);
    assert.equal(budget.startFile(6), true);
    assert.equal(budget.accept(4), 4);
    assert.equal(budget.accept(4), 2);
    assert.ok(budget.violation);

    const tooMany = new policy.FolderBudget(1, 100);
    assert.equal(tooMany.startFile(1), true);
    assert.equal(tooMany.startFile(1), false);
    assert.match(tooMany.violation, /files/);

    const tooBig = new policy.FolderBudget(5, 10);
    assert.equal(tooBig.startFile(6), true);
    assert.equal(tooBig.startFile(6), false);
    assert.match(tooBig.violation, /data/);

    const noHeader = new policy.FolderBudget(1, 10);
    assert.equal(noHeader.accept(5), 0);
});
