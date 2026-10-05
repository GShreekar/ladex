// The "Trusted nodes" panel; built with textContent since names come from other devices.

(function () {
    'use strict';

    const trustBtn = document.getElementById('trust-btn');
    const modal = document.getElementById('trust-modal');
    const closeBtn = document.getElementById('close-trust');
    const hint = document.getElementById('trust-hint');
    const list = document.getElementById('trust-list');

    const HOW_TRUSTED = { passphrase: 'joined with the passphrase', pairing: 'paired' };

    function ago(ms) {
        const seconds = Math.max(0, Math.round((Date.now() - ms) / 1000));
        if (seconds < 60) return 'just now';
        if (seconds < 3600) return `${Math.round(seconds / 60)} min ago`;
        if (seconds < 86400) return `${Math.round(seconds / 3600)} h ago`;
        return `${Math.round(seconds / 86400)} d ago`;
    }

    async function revoke(nodeId) {
        const response = await fetch(`/api/trust/${encodeURIComponent(nodeId)}/revoke`, { method: 'POST', cache: 'no-cache' });
        if (!response.ok) {
            const body = await response.json().catch(() => ({}));
            window.app?.toast(body.message || 'Could not revoke that node', 'error');
        }
        await refresh();
    }

    // A first click only arms the button, since a revocation can't be undone from here.
    function revokeButton(node) {
        const button = document.createElement('button');
        button.className = 'btn secondary';
        button.textContent = 'Revoke';
        button.addEventListener('click', () => {
            if (button.dataset.armed) {
                button.disabled = true;
                revoke(node.node_id);
                return;
            }
            button.dataset.armed = 'yes';
            button.classList.add('danger');
            button.textContent = 'Revoke for good?';
        });
        return button;
    }

    function row(node) {
        const item = document.createElement('div');
        item.className = 'session-row' + (node.revoked ? ' revoked' : '');

        const info = document.createElement('div');
        info.className = 'session-info';
        const name = document.createElement('div');
        name.className = 'session-name';
        name.textContent = node.name || node.node_id;
        const meta = document.createElement('div');
        meta.className = 'session-meta';
        const standing = node.revoked
            ? `revoked by ${node.revoked_by}`
            : `${HOW_TRUSTED[node.trusted_via] || node.trusted_via} · ${node.connected ? 'connected' : `last seen ${ago(node.last_seen_ms)}`}`;
        meta.textContent = `${node.node_id} · ${standing}`;
        info.append(name, meta);

        item.append(info);
        if (!node.revoked) item.append(revokeButton(node));
        return item;
    }

    async function refresh() {
        const response = await fetch('/api/trust', { cache: 'no-cache' });
        if (!response.ok) return;
        const data = await response.json();
        if (!data.secured && data.nodes.length === 0) {
            hint.textContent = 'This node is open to anyone, so it trusts no one in particular and has no one to revoke.';
        } else {
            hint.textContent = 'Revoking a node disconnects it and tells every node in the mesh to refuse its key from now on.';
        }
        list.replaceChildren(...data.nodes.map(row));
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
            status = await (await fetch('/api/trust', { cache: 'no-cache' })).json();
        } catch (_) {
            return; // not signed in, or the node is unreachable
        }
        if (!status.can_manage) return;

        trustBtn.style.display = 'block';
        trustBtn.addEventListener('click', open);
        closeBtn.addEventListener('click', close);
        modal.addEventListener('click', (e) => {
            if (e.target === modal) close();
        });
    }

    init();
})();
