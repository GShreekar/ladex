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
