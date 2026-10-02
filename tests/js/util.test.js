const test = require('node:test');
const assert = require('node:assert/strict');
const util = require('../../static/js/util.js');

test('file sizes are shown in the largest sensible unit', () => {
    assert.equal(util.formatFileSize(0), '0 B');
    assert.equal(util.formatFileSize(1), '1.00 B');
    assert.equal(util.formatFileSize(1536), '1.50 KB');
    assert.equal(util.formatFileSize(5 * 1024 ** 3), '5.00 GB');
});

test('odd sizes never print "undefined"', () => {
    assert.equal(util.formatFileSize(-5), '0 B');
    assert.equal(util.formatFileSize(NaN), '0 B');
    assert.equal(util.formatFileSize(0.5), '0.50 B');
    assert.equal(util.formatFileSize(3 * 1024 ** 5), '3072.00 TB');
});

test('time left reads naturally at every scale', () => {
    assert.equal(util.formatTimeLeft(42), '42s left');
    assert.equal(util.formatTimeLeft(185), '3m 5s left');
    assert.equal(util.formatTimeLeft(3725), '1h 2m left');
});

test('escaping is safe in text and in attribute values', () => {
    assert.equal(util.escapeHtml(`<img src=x onerror="a('b')">&`), '&lt;img src=x onerror=&quot;a(&#39;b&#39;)&quot;&gt;&amp;');
    assert.equal(util.escapeHtml(42), '42');
});

test('devices are named from their User-Agent', () => {
    const ua = {
        pixel: 'Mozilla/5.0 (Linux; Android 14; Pixel 8 Build/UP1A) AppleWebKit/537.36 Chrome/120.0 Mobile Safari/537.36',
        iphone: 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 Version/17.0 Mobile Safari/604.1',
        edge: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0 Safari/537.36 Edg/120.0',
        firefox: 'Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0',
        mac: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Version/17.0 Safari/605.1.15',
    };
    assert.equal(util.friendlyDeviceName(ua.pixel), 'Pixel 8 · Chrome');
    assert.equal(util.friendlyDeviceName(ua.iphone), 'iPhone · Safari');
    assert.equal(util.friendlyDeviceName(ua.edge), 'Windows · Edge');
    assert.equal(util.friendlyDeviceName(ua.firefox), 'Linux · Firefox');
    assert.equal(util.friendlyDeviceName(ua.mac), 'Mac · Safari');
    assert.equal(util.friendlyDeviceName(''), null);
    assert.equal(util.friendlyDeviceName('curl/8.0'), null);
});

test('random ids have the prefix, the right length and do not repeat', () => {
    const a = util.randomId('peer_', 10);
    assert.match(a, /^peer_[0-9a-f]{20}$/);
    assert.notEqual(a, util.randomId('peer_', 10));
});
