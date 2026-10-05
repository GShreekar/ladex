// Checks that the page's scripts all load, in order, and that every method called on the app exists.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const STATIC = path.join(__dirname, '..', '..', 'static');
const html = fs.readFileSync(path.join(STATIC, 'index.html'), 'utf8');

const scriptsInPage = [...html.matchAll(/<script src="static\/([^"?]+)/g)].map((m) => m[1]);
// sessions.js and pairing.js are standalone panels, not part of the app class.
const standalone = ['qrcode.js', 'sessions.js', 'pairing.js'];
const ownScripts = scriptsInPage.filter((file) => !standalone.includes(file));

function loadClass() {
    const noop = () => {};
    const context = {
        window: {}, document: { addEventListener: noop }, console, qrcode: undefined,
        localStorage: {}, sessionStorage: {}, navigator: {}, crypto: globalThis.crypto,
    };
    context.window.addEventListener = noop;
    context.window = Object.assign(context.window, { document: context.document });
    context.addEventListener = noop;
    vm.createContext(context);
    for (const file of ownScripts) vm.runInContext(fs.readFileSync(path.join(STATIC, file), 'utf8'), context, { filename: file });
    return vm.runInContext('LADEXApp', context);
}

test('index.html loads every script in static/js', () => {
    const onDisk = fs.readdirSync(path.join(STATIC, 'js')).map((f) => `js/${f}`);
    for (const file of onDisk) assert.ok(scriptsInPage.includes(file), `${file} is not loaded by index.html`);
    for (const file of scriptsInPage) assert.ok(fs.existsSync(path.join(STATIC, file)), `${file} is loaded but missing`);
});

test('the app class is defined before the files that extend it', () => {
    assert.ok(scriptsInPage.indexOf('app.js') < scriptsInPage.indexOf('js/connection.js'));
    assert.ok(scriptsInPage.indexOf('js/util.js') < scriptsInPage.indexOf('app.js'));
});

test('every method called on the app exists', () => {
    const App = loadClass();
    const known = new Set(Object.getOwnPropertyNames(App.prototype));
    const sources = [...ownScripts.map((f) => [f, fs.readFileSync(path.join(STATIC, f), 'utf8')]), ['index.html', html]];

    const missing = [];
    for (const [file, text] of sources) {
        const code = text.split('\n').filter((line) => !line.trim().startsWith('//')).join('\n');
        const calls = [...code.matchAll(/\b(?:this|app)\.([A-Za-z_]\w*)\(/g)].map((m) => m[1]);
        for (const name of calls) if (!known.has(name)) missing.push(`${file}: ${name}`);
    }
    assert.deepEqual([...new Set(missing)], []);
});

test('no method is defined twice across the files', () => {
    const seen = new Map();
    const duplicates = [];
    for (const file of ownScripts) {
        const text = fs.readFileSync(path.join(STATIC, file), 'utf8');
        for (const m of text.matchAll(/LADEXApp\.prototype\.(\w+) = /g)) {
            if (seen.has(m[1])) duplicates.push(`${m[1]} (${seen.get(m[1])} and ${file})`);
            seen.set(m[1], file);
        }
    }
    assert.deepEqual(duplicates, []);
});
