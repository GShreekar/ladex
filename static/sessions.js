// Logout, and the list of signed-in devices; built with textContent since names come from other devices.

(function () {
    'use strict';

    const logoutBtn = document.getElementById('logout-btn');
    const sessionsBtn = document.getElementById('sessions-btn');
    const modal = document.getElementById('sessions-modal');
    const closeBtn = document.getElementById('close-sessions');
    const hint = document.getElementById('sessions-hint');
    const list = document.getElementById('sessions-list');

    async function api(url, options) {
        return fetch(url, { cache: 'no-cache', ...options });
    }

    function ago(isoTime) {
        const seconds = Math.max(0, Math.round((Date.now() - Date.parse(isoTime)) / 1000));
        if (seconds < 60) return 'just now';
        if (seconds < 3600) return `${Math.round(seconds / 60)} min ago`;
        if (seconds < 86400) return `${Math.round(seconds / 3600)} h ago`;
        return `${Math.round(seconds / 86400)} d ago`;
    }

    function goToLogin() {
        window.location.href = '/login';
    }

    async function signOut(id, isThisDevice) {
        const response = await api(`/api/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' });
        if (isThisDevice && (response.ok || response.status === 401)) {
            goToLogin();
            return;
        }
        if (!response.ok) {
            const body = await response.json().catch(() => ({}));
            window.app?.toast(body.message || 'Could not sign that device out', 'error');
        }
        await refresh();
    }

    function row(session, isThisDevice) {
        const item = document.createElement('div');
        item.className = 'session-row';

        const info = document.createElement('div');
        info.className = 'session-info';
        const name = document.createElement('div');
        name.className = 'session-name';
        name.textContent = session.label + (isThisDevice ? ' (this device)' : '');
        const meta = document.createElement('div');
        meta.className = 'session-meta';
        meta.textContent = `${session.ip} · signed in ${ago(session.created_at)} · active ${ago(session.last_seen)}`;
        info.append(name, meta);

        const button = document.createElement('button');
        button.className = 'btn secondary';
        button.textContent = 'Sign out';
        button.addEventListener('click', () => signOut(session.id, isThisDevice));

        item.append(info, button);
        return item;
    }

    async function refresh() {
        const response = await api('/api/sessions');
        if (response.status === 401) return goToLogin();
        const data = await response.json();
        hint.textContent = data.can_manage
            ? 'Signing a device out ends its session; it must enter the passphrase again.'
            : 'You can sign this device out. To manage the other devices, open LADEX on the machine that is running it, at http://localhost.';
        list.textContent = '';
        for (const session of data.sessions) {
            list.append(row(session, session.id === data.current));
        }
    }

    function open() {
        modal.style.display = 'block';
        refresh();
    }

    function close() {
        modal.style.display = 'none';
    }

    async function init() {
        let status;
        try {
            status = await (await api('/auth-status')).json();
        } catch (_) {
            return; // node unreachable; the app's own reconnect logic handles that
        }
        if (!status.auth_required || !status.authenticated) return;

        logoutBtn.style.display = 'block';
        sessionsBtn.style.display = 'block';

        logoutBtn.addEventListener('click', () => {
            api('/logout', { method: 'POST' }).finally(goToLogin);
        });
        sessionsBtn.addEventListener('click', open);
        closeBtn.addEventListener('click', close);
        modal.addEventListener('click', (e) => {
            if (e.target === modal) close();
        });
    }

    init();
})();
