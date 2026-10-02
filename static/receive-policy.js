// How this page treats names that come from other devices when it writes a
// downloaded folder to disk: file and folder names are made safe on every
// platform, and a path can never climb out of the folder. The node cleans
// names too, but can't be relied on to have done it. The rules mirror
// src/validate.rs and are tested against the same cases
// (tests/filename_cases.json).

(function (root) {
    'use strict';

    const MAX_NAME_BYTES = 255;
    const MAX_EXTENSION_BYTES = 16;
    const MAX_PATH_BYTES = 1024;
    const MAX_PATH_DEPTH = 32;
    const MAX_FOLDER_FILES = 100000;

    const WINDOWS_RESERVED = new Set([
        'CON', 'PRN', 'AUX', 'NUL',
        'COM1', 'COM2', 'COM3', 'COM4', 'COM5', 'COM6', 'COM7', 'COM8', 'COM9',
        'LPT1', 'LPT2', 'LPT3', 'LPT4', 'LPT5', 'LPT6', 'LPT7', 'LPT8', 'LPT9',
    ]);
    const FORBIDDEN_IN_NAMES = new Set(['<', '>', ':', '"', '/', '\\', '|', '?', '*']);
    // Control characters plus invisible and bidirectional-override characters
    // that let "photo\u202Egpj.exe" display as "photoexe.jpg".
    const STRIPPED = /[\u0000-\u001F\u007F-\u009F\u00AD\u061C\u200B-\u200F\u2028\u2029\u202A-\u202E\u2060-\u206F\uFEFF\uFFF9-\uFFFB]/;

    function utf8Length(ch) {
        const code = ch.codePointAt(0);
        return code < 0x80 ? 1 : code < 0x800 ? 2 : code < 0x10000 ? 3 : 4;
    }

    function byteLength(text) {
        let total = 0;
        for (const ch of text) total += utf8Length(ch);
        return total;
    }

    function truncateToBytes(name) {
        if (byteLength(name) <= MAX_NAME_BYTES) return name;
        const dot = name.lastIndexOf('.');
        const extension = dot > 0 && byteLength(name.slice(dot)) <= MAX_EXTENSION_BYTES ? name.slice(dot) : '';
        const stem = name.slice(0, name.length - extension.length);
        const extensionBytes = byteLength(extension);
        let kept = '';
        let keptBytes = 0;
        for (const ch of stem) {
            const bytes = utf8Length(ch);
            if (keptBytes + bytes + extensionBytes > MAX_NAME_BYTES) break;
            kept += ch;
            keptBytes += bytes;
        }
        kept = kept.replace(/[ .]+$/, '');
        return (kept || 'unnamed') + extension;
    }

    // Safe as a single file name on Windows, macOS and Linux.
    function sanitizeFileName(input) {
        let cleaned = '';
        for (const ch of String(input ?? '')) {
            const code = ch.codePointAt(0);
            if (code >= 0xD800 && code <= 0xDFFF) { cleaned += '_'; continue; } // lone surrogate
            if (STRIPPED.test(ch)) continue;
            cleaned += FORBIDDEN_IN_NAMES.has(ch) ? '_' : ch;
        }
        cleaned = cleaned.replace(/^ +/, '').replace(/[ .]+$/, '');
        let name = cleaned || 'unnamed';

        // Windows treats "CON.txt" as the device too, so only the part before the first dot counts.
        const base = name.split('.')[0].replace(/ +$/, '').replace(/[a-z]/g, (c) => c.toUpperCase());
        if (WINDOWS_RESERVED.has(base)) name = '_' + name;
        return truncateToBytes(name);
    }

    // A path inside a received folder: "a/b/c.txt". Empty, "." and ".." parts
    // are dropped, so it can never point outside the folder. null if nothing usable is left.
    function sanitizeRelativePath(path) {
        if (typeof path !== 'string') return null;
        const segments = [];
        for (const part of path.split(/[\\/]/)) {
            if (part === '' || part === '.' || part === '..') continue;
            segments.push(sanitizeFileName(part));
        }
        if (segments.length === 0 || segments.length > MAX_PATH_DEPTH) return null;
        const joined = segments.join('/');
        return byteLength(joined) > MAX_PATH_BYTES ? null : joined;
    }

    // "report.pdf" -> "report (1).pdf", ".bashrc" -> ".bashrc (1)"
    function numberedName(name, n) {
        const dot = name.lastIndexOf('.');
        const stem = dot > 0 ? name.slice(0, dot) : name;
        const tail = ` (${n})${dot > 0 ? name.slice(dot) : ''}`;
        // Shorten the stem, not the tail, so the number always survives.
        const room = MAX_NAME_BYTES - byteLength(tail);
        let kept = '';
        let keptBytes = 0;
        for (const ch of stem) {
            const bytes = utf8Length(ch);
            if (keptBytes + bytes > room) break;
            kept += ch;
            keptBytes += bytes;
        }
        return sanitizeFileName(kept + tail);
    }

    const api = {
        MAX_FOLDER_FILES,
        sanitizeFileName,
        sanitizeRelativePath,
        numberedName,
    };
    if (typeof module !== 'undefined' && module.exports) module.exports = api;
    root.LadexPolicy = api;
})(typeof window !== 'undefined' ? window : globalThis);
