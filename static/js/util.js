// Small pure helpers for the page, unit tested in Node (tests/js/util.test.js).

(function (root) {
    'use strict';

    const UNITS = ['B', 'KB', 'MB', 'GB', 'TB'];

    function formatFileSize(bytes) {
        if (!(bytes > 0)) return '0 B';
        const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), UNITS.length - 1);
        return (bytes / Math.pow(1024, Math.max(i, 0))).toFixed(2) + ' ' + UNITS[Math.max(i, 0)];
    }

    /** "42s left", "3m 5s left", "1h 2m left" */
    function formatTimeLeft(seconds) {
        const s = Math.round(seconds);
        if (s < 60) return `${s}s left`;
        if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s left`;
        return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m left`;
    }

    /** Safe inside HTML text and inside quoted attribute values. */
    function escapeHtml(text) {
        return String(text)
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;')
            .replace(/'/g, '&#39;');
    }

    /** "Pixel 8 · Chrome" from a User-Agent string, or null when it says nothing useful. */
    function friendlyDeviceName(userAgent) {
        if (!userAgent) return null;
        const ua = userAgent;

        let os = null;
        if (/iPhone/.test(ua)) os = 'iPhone';
        else if (/iPad/.test(ua)) os = 'iPad';
        else if (/Android/.test(ua)) {
            const m = ua.match(/Android [\d.]+;\s*([^;)]+?)(?:\s+Build\/|\))/);
            const model = m && m[1] && m[1].trim();
            os = (model && model !== 'K') ? model : 'Android';
        }
        else if (/Windows/.test(ua)) os = 'Windows';
        else if (/Mac OS X/.test(ua)) os = 'Mac';
        else if (/Linux/.test(ua)) os = 'Linux';

        let browser = null;
        if (/Edg\//.test(ua)) browser = 'Edge';
        else if (/OPR\//.test(ua)) browser = 'Opera';
        else if (/Firefox\//.test(ua)) browser = 'Firefox';
        else if (/Chrome\/|CriOS\//.test(ua)) browser = 'Chrome';
        else if (/Safari\//.test(ua)) browser = 'Safari';

        if (os && browser) return `${os} · ${browser}`;
        return os || browser || null;
    }

    function randomId(prefix, bytes, cryptoSource = globalThis.crypto) {
        const random = cryptoSource.getRandomValues(new Uint8Array(bytes));
        return prefix + Array.from(random, (b) => b.toString(16).padStart(2, '0')).join('');
    }

    const api = { formatFileSize, formatTimeLeft, escapeHtml, friendlyDeviceName, randomId };
    if (typeof module !== 'undefined' && module.exports) module.exports = api;
    root.LadexUtil = api;
})(typeof window !== 'undefined' ? window : globalThis);
