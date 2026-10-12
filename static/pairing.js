// The "Pair a device" panel; built with textContent since names come from other devices.

(function () {
    'use strict';

    const POLL_INTERVAL_MS = 1000;

    const pairingBtn = document.getElementById('pairing-btn');
    const modal = document.getElementById('pairing-modal');
    const closeBtn = document.getElementById('close-pairing');
    const addressLine = document.getElementById('pairing-address');
    const windowLine = document.getElementById('pairing-window');
    const reopenBtn = document.getElementById('pairing-reopen');
    const form = document.getElementById('pairing-form');
    const targetInput = document.getElementById('pairing-target');
    const waitingList = document.getElementById('pairing-waiting');
    const recentList = document.getElementById('pairing-recent');

    let poller = null;
    // Rebuilding the waiting codes only when they change keeps their buttons steady under the cursor.
    let shownCodes = '';

    function post(url, body) {
        return fetch(url, {
            method: 'POST',
            cache: 'no-cache',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body ?? {}),
        });
    }

    async function showError(response, fallback) {
        const body = await response.json().catch(() => ({}));
        window.app?.toast(body.message || fallback, 'error');
    }

    function countdown(seconds) {
        const minutes = Math.floor(seconds / 60);
        return `${minutes}:${String(seconds % 60).padStart(2, '0')}`;
    }

    async function answer(id, accepted) {
        const response = await post(`/api/pairing/${id}`, { accepted });
        if (!response.ok) await showError(response, 'That pairing is no longer waiting');
        await refresh();
    }

    function codeRow(code) {
        const item = document.createElement('div');
        item.className = 'session-row pairing-code';

        const name = document.createElement('div');
        name.className = 'session-name';
        name.textContent = code.dialed ? `Pairing with ${code.name}` : `${code.name} wants to pair`;

        const words = document.createElement('div');
        words.className = 'pairing-words';
        for (const word of code.words) {
            const chip = document.createElement('span');
            chip.className = 'pairing-word';
            chip.textContent = word;
            words.append(chip);
        }

        const actions = document.createElement('div');
        actions.className = 'pairing-actions';
        const match = document.createElement('button');
        match.className = 'btn primary';
        match.textContent = 'They match';
        match.addEventListener('click', () => answer(code.id, true));
        const mismatch = document.createElement('button');
        mismatch.className = 'btn secondary';
        mismatch.textContent = "They don't match";
        mismatch.addEventListener('click', () => answer(code.id, false));
        actions.append(match, mismatch);

        item.append(name, words, actions);
        return item;
    }

    function outcomeRow(outcome) {
        const item = document.createElement('div');
        item.className = 'session-meta';
        item.textContent = outcome.paired ? `Paired with ${outcome.name}` : `${outcome.name}: ${outcome.message}`;
        return item;
    }

    function render(status) {
        addressLine.textContent = status.address ? `This device's address: ${status.address}` : '';
        windowLine.textContent = status.open
            ? `Accepting pairings for ${countdown(status.seconds_left)}`
            : 'Not accepting pairings from other devices.';
        reopenBtn.style.display = status.open ? 'none' : 'inline-block';

        const codes = JSON.stringify(status.waiting);
        if (codes !== shownCodes) {
            shownCodes = codes;
            waitingList.replaceChildren(...status.waiting.map(codeRow));
        }
        recentList.replaceChildren(...status.recent.map(outcomeRow));
    }

    async function refresh() {
        const response = await fetch('/api/pairing', { cache: 'no-cache' });
        if (!response.ok) return;
        render(await response.json());
    }

    async function open() {
        modal.style.display = 'block';
        await post('/api/pairing/open');
        await refresh();
        poller = setInterval(refresh, POLL_INTERVAL_MS);
    }

    // Closing the panel stops accepting pairings and declines any code still waiting.
    function close() {
        modal.style.display = 'none';
        clearInterval(poller);
        poller = null;
        shownCodes = '';
        post('/api/pairing/close');
    }

    async function dial(event) {
        event.preventDefault();
        const response = await post('/api/pairing/dial', { address: targetInput.value });
        if (!response.ok) {
            await showError(response, 'Could not start pairing');
            return;
        }
        targetInput.value = '';
        await refresh();
    }

    async function init() {
        let status;
        try {
            status = await (await fetch('/api/pairing', { cache: 'no-cache' })).json();
        } catch (_) {
            return; // not signed in, or the node is unreachable
        }
        if (!status.can_pair) return;

        pairingBtn.style.display = 'block';
        pairingBtn.addEventListener('click', open);
        closeBtn.addEventListener('click', close);
        reopenBtn.addEventListener('click', async () => {
            await post('/api/pairing/open');
            await refresh();
        });
        form.addEventListener('submit', dial);
        modal.addEventListener('click', (e) => {
            if (e.target === modal) close();
        });
    }

    init();
})();
