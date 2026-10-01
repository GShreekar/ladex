// What a receiving browser accepts from the peer that is sending it a file.
//
// The sender is another device on the network, so everything in its headers
// (names, sizes, counts) and the amount of data it sends is untrusted. The
// node can't be relied on to have cleaned any of it either. The file-name
// rules mirror src/validate.rs and are tested against the same cases
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

    const ID = /^[A-Za-z0-9_-]{1,64}$/;
    const SHA256 = /^[0-9a-f]{64}$/;
    const MIME_TOKEN = /^[A-Za-z0-9!#$&^_.+-]+$/;

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

    function sanitizeMime(mime) {
        const fallback = 'application/octet-stream';
        if (typeof mime !== 'string' || mime.length > 255) return fallback;
        const parts = mime.split('/');
        return parts.length === 2 && MIME_TOKEN.test(parts[0]) && MIME_TOKEN.test(parts[1]) ? mime : fallback;
    }

    const isId = (value) => typeof value === 'string' && ID.test(value);
    const isSize = (value) => Number.isSafeInteger(value) && value >= 0;

    // The header the sender puts in front of a single file. Returns the same
    // fields, cleaned, or null when it is not something to accept. Everything
    // the page later shows or uses comes from this, never from the raw header.
    function parseFileHeader(meta) {
        if (!meta || typeof meta !== 'object') return null;
        if (!isId(meta.fileId) || !isSize(meta.fileSize)) return null;
        const resumeFromBytes = meta.resumeFromBytes === undefined ? 0 : meta.resumeFromBytes;
        if (!isSize(resumeFromBytes) || resumeFromBytes > meta.fileSize) return null;
        return {
            fileId: meta.fileId,
            fileName: sanitizeFileName(meta.fileName),
            fileSize: meta.fileSize,
            mimeType: sanitizeMime(meta.mimeType),
            sha256: typeof meta.sha256 === 'string' && SHA256.test(meta.sha256) ? meta.sha256 : null,
            resumeFromBytes,
            totalChunks: isSize(meta.totalChunks) ? meta.totalChunks : 0,
        };
    }

    function parseFolderManifest(meta) {
        if (!meta || typeof meta !== 'object' || meta.kind !== 'folder-manifest') return null;
        if (!isId(meta.folderId) || !isSize(meta.totalBytes)) return null;
        if (!Number.isInteger(meta.totalFiles) || meta.totalFiles < 0 || meta.totalFiles > MAX_FOLDER_FILES) return null;
        return {
            kind: 'folder-manifest',
            folderId: meta.folderId,
            folderName: sanitizeFileName(meta.folderName),
            totalBytes: meta.totalBytes,
            totalFiles: meta.totalFiles,
        };
    }

    // The header that precedes each file inside a folder transfer.
    function parseFolderEntryHeader(msg) {
        if (!msg || typeof msg !== 'object' || !isSize(msg.size)) return null;
        const relativePath = sanitizeRelativePath(msg.relativePath);
        return relativePath === null ? null : { relativePath, size: msg.size };
    }

    // A resumed transfer may only continue from bytes we really have.
    function resumeOffsetOk(offset, bytesWeHave, fileSize) {
        if (offset === 0) return true;
        return Number.isSafeInteger(bytesWeHave) && offset <= bytesWeHave && offset <= fileSize;
    }

    // Counts what a sender actually sends against what it announced.
    // `accept` says how much of a chunk may be kept (a sender can't write
    // past its announced size) and remembers if it tried to.
    class ByteBudget {
        constructor(limit, alreadyReceived = 0) {
            this.limit = limit;
            this.received = alreadyReceived;
            this.exceeded = false;
        }

        accept(length) {
            const take = Math.max(0, Math.min(length, this.limit - this.received));
            if (take < length) this.exceeded = true;
            this.received += take;
            return take;
        }

        get complete() {
            return this.received >= this.limit;
        }
    }

    // The same for a folder: no more files, and no more bytes in total, than
    // the manifest announced, and no file longer than its own header said.
    class FolderBudget {
        constructor(totalFiles, totalBytes) {
            this.maxFiles = totalFiles;
            this.maxBytes = totalBytes;
            this.files = 0;
            this.declaredBytes = 0;
            this.receivedBytes = 0;
            this.file = null;
            this.violation = null;
        }

        startFile(size) {
            if (this.files >= this.maxFiles) return this._fail('more files than announced');
            if (this.declaredBytes + size > this.maxBytes) return this._fail('more data than announced');
            this.files += 1;
            this.declaredBytes += size;
            this.file = new ByteBudget(size);
            return true;
        }

        accept(length) {
            if (!this.file) return 0;
            const take = Math.max(0, Math.min(this.file.accept(length), this.maxBytes - this.receivedBytes));
            if (this.file.exceeded) this.violation = 'sent more data than announced';
            this.receivedBytes += take;
            return take;
        }

        _fail(reason) {
            this.violation = reason;
            return false;
        }
    }

    const api = {
        MAX_FOLDER_FILES,
        sanitizeFileName,
        sanitizeRelativePath,
        numberedName,
        sanitizeMime,
        parseFileHeader,
        parseFolderManifest,
        parseFolderEntryHeader,
        resumeOffsetOk,
        ByteBudget,
        FolderBudget,
    };
    if (typeof module !== 'undefined' && module.exports) module.exports = api;
    root.LadexPolicy = api;
})(typeof window !== 'undefined' ? window : globalThis);
