// ============================================================================
// LADEX — Local Area Data Exchange
//
// Shared files live on the LADEX nodes (the machines running it), not in
// browser tabs. This page uploads to the node it is connected to and
// downloads from it over plain HTTP, so the browser streams every file to
// disk itself; nodes fetch from each other behind the scenes.
// ============================================================================

class LADEXApp {
    constructor() {
        this.ws = null;
        this.sessionId = this.generateSessionId();

        // Who this page is talking to: the node's id, and whether this page
        // is on the machine running it (which may unshare anything it shared).
        this.nodeId = null;
        this.isHost = false;
        // Node id -> hostname, remembered from the devices seen so far.
        this.nodeNames = new Map();

        this.peers = new Map();
        this.messages = [];
        this.serverFiles = [];

        // F5: editable device nickname, persisted per-browser
        this.nickname = localStorage.getItem('ladex_nickname') || '';
        // F4: fileId → name for deletes this tab initiated, so the
        // 'file_removed' broadcast echo can confirm with the right message
        // instead of a generic one (or a duplicate toast).
        this._pendingDeletes = new Map();

        // Transfers in progress, for the progress panel and its Cancel buttons.
        this._progressCards = new Map();
        this._uploads = new Map();            // transferId → Set of XMLHttpRequests
        this._aborters = new Map();           // transferId → AbortController (folder downloads)
        this.cancelledTransfers = new Set();
        // Downloads just started, to disable the button for a moment.
        this.pendingDownloads = new Set();

        this.UPLOAD_RETRIES = 6;
        this.UPLOAD_CONCURRENCY = 2;
        this.FOLDER_CONCURRENCY = 3;

        // ── Toast queue ─────────────────────────────────────────────────
        this._toastContainer = null;

        this.init();
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    _randomId(prefix, bytes) {
        const random = crypto.getRandomValues(new Uint8Array(bytes));
        return prefix + Array.from(random, (b) => b.toString(16).padStart(2, '0')).join('');
    }

    // A tab keeps its id across a reload (so it still owns what it shared),
    // but two tabs never share one.
    generateSessionId() {
        try {
            const saved = sessionStorage.getItem('ladex_session');
            if (saved) return saved;
        } catch (_) { /* storage blocked */ }
        const id = this._randomId('peer_', 10);
        try { sessionStorage.setItem('ladex_session', id); } catch (_) { /* storage blocked */ }
        return id;
    }

    generateFileId() {
        return this._randomId('file_', 12);
    }

    getShortPeerId() {
        return this.sessionId.slice(-6);
    }

    formatFileSize(bytes) {
        const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
        if (bytes === 0) return '0 B';
        const i = Math.floor(Math.log(bytes) / Math.log(1024));
        return (bytes / Math.pow(1024, i)).toFixed(2) + ' ' + sizes[i];
    }

    formatSize(bytes) { return this.formatFileSize(bytes); }


    // ── Initialization ──────────────────────────────────────────────────

    init() {
        console.log('Initializing LADEX …');
        console.log('Session ID:', this.sessionId);
        this.initializePeerDisplay();
        this.connectWebSocket();
        this.setupEventListeners();
        this.setupDragAndDrop();
        this.updateDevicesList();
        this._loadNodeInfo();
    }

    // Which node this is, and whether we are on the machine running it.
    async _loadNodeInfo() {
        try {
            const status = await (await fetch('/auth-status', { cache: 'no-store' })).json();
            this.nodeId = status.node_id || null;
            this.isHost = !!status.is_host;
            this.updateFileList(this.serverFiles);
        } catch (_) { /* the page works without it; only the unshare button depends on it */ }
    }

    initializePeerDisplay() {
        const update = () => {
            const el = document.getElementById('peer-number');
            if (el) { el.textContent = `Peer: ${this.getShortPeerId()}`; return true; }
            return false;
        };
        if (!update()) setTimeout(() => { if (!update()) setTimeout(update, 1000); }, 100);
    }

    // =====================================================================
    //  F5: DEVICE NAMES
    // =====================================================================

    /** Best-effort "OS · Browser" label from a User-Agent string. Modern
     *  browsers increasingly freeze/generalize UA strings (Chrome's User-Agent
     *  Reduction, etc.), so exact phone models aren't reliably available
     *  any more — this gets the OS family plus, for Android, a device model
     *  when the browser still includes one. */
    _friendlyDeviceName(userAgent) {
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

    /**
     * Nickname (if set) > UA-derived name > short session id.
     * `peer` may be undefined (e.g. a host_id we haven't gotten PeerInfo
     * for yet) — sessionId is passed separately so the fallback still has
     * something to work with in that case.
     */
    _deviceDisplayName(sessionId, peer) {
        const isSelf = sessionId === this.sessionId;
        const nickname = isSelf ? this.nickname : peer?.nickname;
        if (nickname) return nickname;
        const friendly = this._friendlyDeviceName(peer?.user_agent);
        if (friendly) return friendly;
        return `Peer ${sessionId.slice(-6)}`;
    }

    updateDevicesList() {
        const list = document.getElementById('devices-list');
        const countEl = document.getElementById('devices-count');
        if (!list) return;

        const others = Array.from(this.peers.values()).filter(p => p.session_id !== this.sessionId && !p.left);
        const self = { session_id: this.sessionId, nickname: this.nickname };

        const chip = (peer, isSelf) => {
            const name = this.escapeHtml(this._deviceDisplayName(peer.session_id, peer));
            const host = peer.hosting_node_name ? this.escapeHtml(peer.hosting_node_name) : '';
            const rtt = (!isSelf && peer.node_rtt_ms != null) ? `${peer.node_rtt_ms}ms` : '';
            const sub = [host, rtt].filter(Boolean).join(' · ');
            const youBadge = isSelf ? ' <span class="device-chip-you">(you)</span>' : '';
            return `
                <div class="device-chip${isSelf ? ' device-chip-self' : ''}" data-session-id="${this.escapeHtml(peer.session_id)}" title="${isSelf ? 'Click to set a nickname' : 'Click to send a file, or drag one here'}">
                    <div class="device-chip-name">${name}${youBadge}</div>
                    ${sub ? `<div class="device-chip-host">${sub}</div>` : ''}
                </div>`;
        };

        list.innerHTML = chip(self, true) + others.map(p => chip(p, false)).join('');
        if (countEl) countEl.textContent = `(${1 + others.length})`;
    }

    /** Turns the "you" chip's name into an inline text input. Avoids
     *  prompt()/alert() — those block the JS event loop, which could stall
     *  an in-flight transfer while the dialog is open. */
    _editNicknameInline(chipEl) {
        const nameEl = chipEl.querySelector('.device-chip-name');
        if (!nameEl || chipEl.querySelector('.device-chip-input')) return; // already editing

        const input = document.createElement('input');
        input.type = 'text';
        input.className = 'device-chip-input';
        input.maxLength = 40;
        input.value = this.nickname || '';
        input.placeholder = 'Nickname…';

        const commit = () => {
            const nickname = input.value.trim().slice(0, 40);
            this.nickname = nickname;
            localStorage.setItem('ladex_nickname', nickname);
            if (nickname) {
                this.sendWS({ type: 'set_nickname', session_id: this.sessionId, nickname });
            }
            this.updateDevicesList();
        };
        input.addEventListener('keydown', (e) => {
            if (e.key === 'Enter') input.blur();
            if (e.key === 'Escape') { input.value = this.nickname || ''; input.blur(); }
        });
        input.addEventListener('blur', commit, { once: true });

        nameEl.replaceWith(input);
        input.focus();
        input.select();
    }

    // =====================================================================
    //  TOAST NOTIFICATIONS (replaces alert())
    // =====================================================================

    _ensureToastContainer() {
        if (this._toastContainer) return;
        this._toastContainer = document.createElement('div');
        this._toastContainer.id = 'toast-container';
        document.body.appendChild(this._toastContainer);
    }

    /**
     * Show a small toast notification.
     * @param {string} message
     * @param {'info'|'success'|'error'|'warning'} type
     * @param {number} durationMs — auto-dismiss after this many ms
     */
    /**
     * @param {string} message
     * @param {'info'|'success'|'error'|'warning'} type
     * @param {number} durationMs — auto-dismiss after this many ms
     * @param {{label: string, onClick: () => void}} [action] — F7: e.g. a
     *   "Retry" button on an integrity-check-failed toast
     */
    toast(message, type = 'info', durationMs = 4000, action = null) {
        this._ensureToastContainer();
        const el = document.createElement('div');
        el.className = `toast toast-${type}`;
        const icons = { info: 'ℹ️', success: '✅', error: '❌', warning: '⚠️' };
        el.innerHTML = `<span class="toast-icon">${icons[type] || ''}</span><span class="toast-msg">${this.escapeHtml(message)}</span>`;
        if (action) {
            const btn = document.createElement('button');
            btn.className = 'toast-action';
            btn.textContent = action.label;
            btn.addEventListener('click', () => { action.onClick(); dismiss(); });
            el.appendChild(btn);
        }
        this._toastContainer.appendChild(el);
        // Trigger CSS enter animation
        requestAnimationFrame(() => el.classList.add('toast-visible'));
        const dismiss = () => {
            el.classList.remove('toast-visible');
            el.classList.add('toast-exit');
            el.addEventListener('transitionend', () => el.remove());
            // Fallback if transitionend doesn't fire
            setTimeout(() => el.remove(), 500);
        };
        setTimeout(dismiss, durationMs);
    }

    // =====================================================================
    //  WEBSOCKET — signaling only
    // =====================================================================

    connectWebSocket() {
        const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
        const url   = `${proto}//${location.host}/ws`;
        this.ws = new WebSocket(url);

        this.ws.onopen = () => {
            console.log('WS connected');
            this.updateConnectionStatus(true);
            this.sendWS({
                type: 'join',
                session_id: this.sessionId,
                user_agent: navigator.userAgent,
                nickname: this.nickname || null,
            });
        };

        this.ws.onmessage = (e) => {
            try {
                this.handleServerMessage(JSON.parse(e.data));
            } catch (err) {
                console.error('Failed to parse WS message:', err);
            }
        };

        this.ws.onclose = () => {
            console.log('WS disconnected — reconnecting in 3 s');
            this.updateConnectionStatus(false);
            setTimeout(() => this._reconnectOrSignIn(), 3000);
        };

        this.ws.onerror = (err) => console.error('WS error:', err);
    }

    // Reconnect, unless this device was signed out (or the node restarted and
    // forgot its sessions): the node would just refuse, so go to the login page.
    async _reconnectOrSignIn() {
        try {
            const status = await (await fetch('/auth-status', { cache: 'no-cache' })).json();
            if (status.auth_required && !status.authenticated) {
                window.location.href = '/login';
                return;
            }
        } catch (_) { /* node unreachable — fall through and retry */ }
        this.connectWebSocket();
    }

    sendWS(msg) {
        if (this.ws && this.ws.readyState === WebSocket.OPEN) {
            this.ws.send(JSON.stringify(msg));
        }
    }

    // kept for compatibility with the text-message prototype methods below
    sendMessage(msg) { this.sendWS(msg); }

    // ── Server message dispatcher ────────────────────────────────────────

    handleServerMessage(msg) {
        switch (msg.type) {
            // ── peer presence ───────────────────────────────────────────
            case 'peer_joined':
                this._rememberNode(msg.peer);
                this.peers.set(msg.peer.session_id, msg.peer);
                this.updatePeerStatus(msg.total_peers);
                this.updateDevicesList();
                this.updateFileList(this.serverFiles);
                break;
            case 'peer_left':
                this.peers.delete(msg.session_id);
                this.updatePeerStatus(msg.total_peers);
                this.updateDevicesList();
                break;
            // Phase 6: incremental peer list update (RTT changes, nicknames, ...)
            case 'peer_sync':
                if (msg.peers) {
                    for (const peer of msg.peers) {
                        if (peer.hosting_node_id == null) {
                            // Departure tombstone
                            this.peers.delete(peer.session_id);
                        } else {
                            // Merge: update fields without overwriting others
                            this._rememberNode(peer);
                            const existing = this.peers.get(peer.session_id) || {};
                            this.peers.set(peer.session_id, { ...existing, ...peer });
                        }
                    }
                }
                this.updateDevicesList();
                this.updateFileList(this.serverFiles);
                break;

            // ── file catalog ────────────────────────────────────────────
            case 'file_list_update':
                this.serverFiles = msg.files || [];
                this.updateFileList(this.serverFiles);
                break;

            // F4: someone unshared a file (possibly us — deleteFile() records
            // the name here first so this can confirm with it, instead of
            // double-toasting alongside an optimistic message there)
            case 'file_removed': {
                const pendingName = this._pendingDeletes.get(msg.file_id);
                if (pendingName !== undefined) {
                    this._pendingDeletes.delete(msg.file_id);
                    this.toast(`Unshared ${pendingName}`, 'success');
                } else {
                    this.toast('A shared file was removed', 'info');
                }
                break;
            }

            // F3: someone is pointing us at a file
            case 'incoming_file_offer':
                this._showFileOfferDialog(msg);
                break;
            case 'file_offer_declined': {
                const name = this._deviceDisplayName(msg.from_session_id, this.peers.get(msg.from_session_id));
                this.toast(`${name} declined your file offer`, 'warning');
                break;
            }

            // ── misc ────────────────────────────────────────────────────
            // Phase 10: AP isolation diagnostic
            case 'no_peers_warning':
                this._showApIsolationBanner(msg.message);
                break;
            case 'error':
                this.toast(msg.message, 'error', 6000);
                break;
            case 'pong':
                break;
            case 'text_message':
                this.handleTextMessage(msg);
                break;
            case 'message_history':
                this.handleMessageHistory(msg);
                break;
        }
    }

    _rememberNode(peer) {
        if (peer && peer.hosting_node_id && peer.hosting_node_name) {
            this.nodeNames.set(peer.hosting_node_id, peer.hosting_node_name);
        }
    }

    _nodeName(nodeId) {
        return this.nodeNames.get(nodeId) || `node ${String(nodeId).slice(-6)}`;
    }

    // Names of the nodes that currently have the whole file.
    _holderNames(file) {
        return Object.entries(file.holders || {})
            .filter(([, holder]) => holder && holder.present)
            .map(([nodeId]) => this._nodeName(nodeId));
    }

    // =====================================================================
    //  FILE UPLOAD — the file goes to this node's disk (and stays there)
    //
    //  The request body is a slice of the File, which the browser streams
    //  from disk, so size doesn't matter. If the connection drops, the node
    //  keeps what it received and says where to continue (the upload-status
    //  endpoint), so the upload resumes instead of starting over.
    // =====================================================================

    async handleFileUpload(files, isFolder) {
        if (!files || files.length === 0) return;
        try {
            if (isFolder) {
                await this.handleFolderUpload(Array.from(files));
                return;
            }
            const list = Array.from(files);
            let shared = 0;
            await this._runLimited(list, this.UPLOAD_CONCURRENCY, async (file) => {
                try {
                    await this.uploadFile(file);
                    shared++;
                } catch (err) {
                    if (err.message !== 'cancelled') this.toast(`Could not share ${file.name}: ${err.message}`, 'error', 8000);
                }
            });
            if (shared > 0) this.toast(`${shared} file${shared > 1 ? 's' : ''} shared`, 'success');
        } catch (err) {
            this.toast(`Upload failed: ${err.message}`, 'error');
        }
    }

    // Runs `work` over `items`, at most `limit` at a time.
    async _runLimited(items, limit, work) {
        let next = 0;
        const worker = async () => {
            while (next < items.length) {
                const index = next++;
                await work(items[index], index);
            }
        };
        await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
    }

    _sleep(ms) {
        return new Promise((resolve) => setTimeout(resolve, ms));
    }

    // How much of this upload the node already has (0 if none).
    async _uploadStatus(fileId, size) {
        const response = await fetch(`/api/files/${fileId}/upload?size=${size}`, { cache: 'no-store' });
        if (response.status === 401) { window.location.href = '/login'; throw new Error('signed out'); }
        return response.json();
    }

    // One attempt at sending the file from `offset`. Resolves, never rejects:
    // { ok, status, body } from the node, or { network: true } if the
    // connection failed, or { aborted: true } if it was cancelled.
    _putFile(group, fileId, file, offset, parent, onProgress) {
        return new Promise((resolve) => {
            const xhr = new XMLHttpRequest();
            xhr.open('PUT', `/api/files/${fileId}`);
            xhr.setRequestHeader('X-Ladex-Session', this.sessionId);
            xhr.setRequestHeader('X-Ladex-Size', String(file.size));
            xhr.setRequestHeader('X-Ladex-Offset', String(offset));
            xhr.setRequestHeader('X-Ladex-Name', encodeURIComponent(file.name));
            if (parent) xhr.setRequestHeader('X-Ladex-Parent', parent);
            xhr.setRequestHeader('Content-Type', file.type || 'application/octet-stream');
            xhr.upload.onprogress = (e) => { if (e.lengthComputable) onProgress(offset + e.loaded); };
            xhr.onload = () => {
                let body = {};
                try { body = JSON.parse(xhr.responseText); } catch (_) { /* not JSON */ }
                resolve({ ok: xhr.status >= 200 && xhr.status < 300, status: xhr.status, body });
            };
            xhr.onerror = () => resolve({ network: true, status: 0, body: {} });
            xhr.ontimeout = xhr.onerror;
            xhr.onabort = () => resolve({ aborted: true, status: 0, body: {} });

            if (!this._uploads.has(group)) this._uploads.set(group, new Set());
            this._uploads.get(group).add(xhr);
            xhr.send(offset > 0 ? file.slice(offset) : file);
        });
    }

    /**
     * Uploads one file, resuming after interruptions. `group` is the transfer
     * id whose Cancel button covers it; `report(bytesSoFar)` is called as it
     * goes. Resolves with the node's catalog entry.
     */
    async uploadFile(file, { parent = null, group = null, report = null } = {}) {
        const fileId = this.generateFileId();
        const ownCard = group === null;
        group = group || `up:${fileId}`;
        const started = Date.now();
        let lastShown = 0;
        const progress = (bytes) => {
            if (report) { report(bytes); return; }
            const now = Date.now();
            if (now - lastShown < 100 && bytes < file.size) return;
            lastShown = now;
            const speed = bytes / Math.max(0.001, (now - started) / 1000);
            const remaining = speed > 0 ? (file.size - bytes) / speed : null;
            this.showProgress(group, `Uploading ${file.name}`, Math.round((bytes / Math.max(1, file.size)) * 100), speed, remaining, bytes, file.size);
        };
        if (ownCard) this.showProgress(group, `Uploading ${file.name}`, 0, null, null, 0, file.size);

        try {
            for (let attempt = 0; ; attempt++) {
                if (this.cancelledTransfers.has(group)) throw new Error('cancelled');
                let offset = 0;
                try {
                    const status = await this._uploadStatus(fileId, file.size);
                    if (status.complete) return { id: fileId };
                    offset = status.offset || 0;
                } catch (err) {
                    if (err.message === 'signed out') throw err;
                    if (attempt >= this.UPLOAD_RETRIES) throw new Error('the node could not be reached');
                    await this._sleep(Math.min(1000 * 2 ** attempt, 15000));
                    continue;
                }

                const result = await this._putFile(group, fileId, file, offset, parent, progress);
                if (result.aborted) throw new Error('cancelled');
                if (result.ok) {
                    progress(file.size);
                    return result.body.file || { id: fileId };
                }
                if (result.status === 401) { window.location.href = '/login'; throw new Error('signed out'); }
                // The node asked for an earlier offset: loop, which asks it again.
                const retryable = result.network || (result.status === 409 && typeof result.body.offset === 'number');
                if (!retryable || attempt >= this.UPLOAD_RETRIES) {
                    throw new Error(result.body.message || (result.network ? 'the connection was lost' : `the node answered ${result.status}`));
                }
                if (result.network) await this._sleep(Math.min(1000 * 2 ** attempt, 15000));
            }
        } finally {
            this._uploads.get(group)?.clear();
            if (ownCard) this.hideProgress(group);
        }
    }

    /**
     * F6: a folder is uploaded file by file (each one resumable), then
     * published as one entry listing them. The files are hidden from the
     * list until then (they carry the folder as their parent).
     */
    async handleFolderUpload(files) {
        const folderId = this.generateFileId();
        const folderName = files[0].webkitRelativePath?.split('/')[0] || 'folder';
        if (files.length > LadexPolicy.MAX_FOLDER_FILES) {
            throw new Error(`a folder can hold at most ${LadexPolicy.MAX_FOLDER_FILES} files`);
        }
        const entries = files.map((file) => ({ path: file.webkitRelativePath || file.name, file }));
        const totalBytes = entries.reduce((sum, e) => sum + e.file.size, 0);
        const group = `up:${folderId}`;
        const started = Date.now();

        // Bytes confirmed per file so far, for one progress card over the whole folder.
        const sent = new Array(entries.length).fill(0);
        let lastShown = 0;
        const show = () => {
            const now = Date.now();
            if (now - lastShown < 100) return;
            lastShown = now;
            const bytes = sent.reduce((a, b) => a + b, 0);
            const speed = bytes / Math.max(0.001, (now - started) / 1000);
            this.showProgress(group, `Uploading ${folderName}`, Math.round((bytes / Math.max(1, totalBytes)) * 100), speed, speed > 0 ? (totalBytes - bytes) / speed : null, bytes, totalBytes);
        };
        this.showProgress(group, `Uploading ${folderName}`, 0, null, null, 0, totalBytes);

        const children = new Array(entries.length);
        try {
            await this._runLimited(entries, this.FOLDER_CONCURRENCY, async (entry, index) => {
                if (this.cancelledTransfers.has(group)) throw new Error('cancelled');
                const done = await this.uploadFile(entry.file, {
                    parent: folderId,
                    group,
                    report: (bytes) => { sent[index] = bytes; show(); },
                });
                sent[index] = entry.file.size;
                children[index] = { path: entry.path, file_id: done.id };
            });

            const response = await fetch(`/api/folders/${folderId}`, {
                method: 'PUT',
                headers: { 'Content-Type': 'application/json', 'X-Ladex-Session': this.sessionId },
                body: JSON.stringify({ name: folderName, children }),
            });
            if (!response.ok) {
                const body = await response.json().catch(() => ({}));
                throw new Error(body.message || `the node answered ${response.status}`);
            }
            this.toast(`Folder shared: ${folderName} (${entries.length} file${entries.length > 1 ? 's' : ''})`, 'success');
        } catch (err) {
            // Don't leave half a folder behind: the files that did arrive are hidden but take up space.
            for (const child of children) {
                if (child) this.sendWS({ type: 'delete_file', session_id: this.sessionId, file_id: child.file_id });
            }
            if (err.message !== 'cancelled') this.toast(`Could not share ${folderName}: ${err.message}`, 'error', 8000);
        } finally {
            this.hideProgress(group);
            this._uploads.delete(group);
        }
    }

    // =====================================================================
    //  DRAG-AND-DROP
    // =====================================================================

    setupDragAndDrop() {
        const body = document.body;
        let dragDepth = 0;
        // F3 also uses drag-and-drop internally (a file row onto a device
        // chip), which bubbles up to these same body listeners — only react
        // to drags actually carrying OS files, not that internal one.
        const isFileDrag = (e) => e.dataTransfer.types.includes('Files');

        body.addEventListener('dragenter', (e) => {
            if (!isFileDrag(e)) return;
            e.preventDefault();
            dragDepth++;
            body.classList.add('drag-over');
        });

        body.addEventListener('dragleave', (e) => {
            if (!isFileDrag(e)) return;
            e.preventDefault();
            dragDepth--;
            if (dragDepth <= 0) {
                dragDepth = 0;
                body.classList.remove('drag-over');
            }
        });

        body.addEventListener('dragover', (e) => {
            if (!isFileDrag(e)) return;
            e.preventDefault();
            e.dataTransfer.dropEffect = 'copy';
        });

        body.addEventListener('drop', (e) => {
            if (!isFileDrag(e)) return;
            e.preventDefault();
            dragDepth = 0;
            body.classList.remove('drag-over');
            const files = e.dataTransfer.files;
            if (files && files.length > 0) {
                this.handleFileUpload(files, false);
            }
        });
    }

    // =====================================================================
    //  DOWNLOAD — a plain HTTP GET to this node, so the browser's own
    //  download manager streams it to disk (any browser, any size). If the
    //  node doesn't have the file yet it fetches it from the other nodes
    //  while the download is already running.
    // =====================================================================

    async downloadFile(fileId) {
        const file = this.serverFiles.find((f) => f.id === fileId);
        if (!file) return;
        if (file.is_folder) return this.downloadFolder(file);
        if (this.pendingDownloads.has(fileId)) return;

        this.pendingDownloads.add(fileId);
        this.updateFileList(this.serverFiles);
        try {
            // A HEAD request first: it tells us right away if the file can't be
            // had (and starts the node fetching it), which a download link can't.
            const probe = await fetch(`/api/files/${encodeURIComponent(fileId)}`, { method: 'HEAD', cache: 'no-store' });
            if (probe.status === 401) { window.location.href = '/login'; return; }
            if (!probe.ok) {
                this.toast(this._downloadProblem(probe.status), 'error', 7000);
                return;
            }
            this._saveViaBrowser(`/api/files/${encodeURIComponent(fileId)}`, file.name);
            this.toast(`Downloading ${file.name} — your browser is saving it`, 'info');
        } catch (_) {
            this.toast('Could not reach the node', 'error');
        } finally {
            setTimeout(() => {
                this.pendingDownloads.delete(fileId);
                this.updateFileList(this.serverFiles);
            }, 1500);
        }
    }

    _downloadProblem(status) {
        if (status === 404) return 'That file has been removed.';
        if (status === 503) return 'No device that has this file is online right now.';
        if (status === 507) return 'This node does not have room to fetch that file.';
        return `The node could not provide that file (error ${status}).`;
    }

    // Hands a URL to the browser's download manager.
    _saveViaBrowser(url, name) {
        const a = document.createElement('a');
        a.href = url;
        a.download = name;
        a.style.display = 'none';
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
    }

    /**
     * F6: a folder. With a folder picker (Chrome, Edge) its files are written
     * into a real directory, one by one, streamed. Everything else gets one
     * zip streamed by the node.
     */
    async downloadFolder(folder) {
        if (this.pendingDownloads.has(folder.id)) return;
        if (!window.showDirectoryPicker) {
            this._saveViaBrowser(`/api/folders/${encodeURIComponent(folder.id)}.zip`, `${folder.name}.zip`);
            this.toast(`Downloading ${folder.name} as a zip`, 'info');
            return;
        }

        let dirHandle;
        try {
            dirHandle = await window.showDirectoryPicker({ mode: 'readwrite' });
        } catch (err) {
            if (err.name !== 'AbortError') this.toast('Could not open that folder', 'error');
            return;
        }

        const group = `dl:${folder.id}`;
        const aborter = new AbortController();
        this._aborters.set(group, aborter);
        this.pendingDownloads.add(folder.id);
        this.updateFileList(this.serverFiles);
        try {
            const listingResponse = await fetch(`/api/files/${encodeURIComponent(folder.id)}`, { cache: 'no-store', signal: aborter.signal });
            if (!listingResponse.ok) throw new Error(this._downloadProblem(listingResponse.status));
            const listing = await listingResponse.json();
            // The paths come from another device: clean them again before creating anything.
            const children = (Array.isArray(listing.children) ? listing.children : []).flatMap((child) => {
                const path = LadexPolicy.sanitizeRelativePath(child.path);
                return path && /^[A-Za-z0-9_-]{1,64}$/.test(child.file_id) && Number.isSafeInteger(child.size)
                    ? [{ path, fileId: child.file_id, size: child.size }] : [];
            });
            const totalBytes = children.reduce((sum, c) => sum + c.size, 0);
            const done = new Array(children.length).fill(0);
            const started = Date.now();
            let lastShown = 0;
            const show = () => {
                const now = Date.now();
                if (now - lastShown < 100) return;
                lastShown = now;
                const bytes = done.reduce((a, b) => a + b, 0);
                const speed = bytes / Math.max(0.001, (now - started) / 1000);
                this.showProgress(group, `Downloading ${folder.name}`, Math.round((bytes / Math.max(1, totalBytes)) * 100), speed, speed > 0 ? (totalBytes - bytes) / speed : null, bytes, totalBytes);
            };
            this.showProgress(group, `Downloading ${folder.name}`, 0, null, null, 0, totalBytes);

            await this._runLimited(children, 2, async (child, index) => {
                const response = await fetch(`/api/files/${encodeURIComponent(child.fileId)}`, { signal: aborter.signal });
                if (!response.ok) throw new Error(`${child.path}: ${this._downloadProblem(response.status)}`);
                const handle = await this._resolveFolderFileHandle(dirHandle, child.path);
                const writable = await handle.createWritable();
                const counter = new TransformStream({
                    transform(chunk, controller) {
                        done[index] += chunk.byteLength;
                        show();
                        controller.enqueue(chunk);
                    },
                });
                await response.body.pipeThrough(counter).pipeTo(writable);
            });
            this.toast(`Downloaded folder ${folder.name} (${children.length} file${children.length === 1 ? '' : 's'})`, 'success', 5000);
        } catch (err) {
            if (!this.cancelledTransfers.has(group)) this.toast(`Folder download failed: ${err.message}`, 'error', 8000);
        } finally {
            this.hideProgress(group);
            this._aborters.delete(group);
            this.cancelledTransfers.delete(group);
            this.pendingDownloads.delete(folder.id);
            this.updateFileList(this.serverFiles);
        }
    }

    /**
     * Resolves (creating as needed) the file handle for a path inside a picked
     * directory. A received folder never replaces anything already in the
     * folder the user picked: on a name clash the new file becomes "name (1).ext".
     */
    async _resolveFolderFileHandle(dirHandle, relativePath) {
        const parts = relativePath.split('/').filter((p) => p && p !== '.' && p !== '..');
        let dir = dirHandle;
        for (let i = 0; i < parts.length - 1; i++) {
            dir = await dir.getDirectoryHandle(parts[i], { create: true });
        }
        const leaf = parts[parts.length - 1] || 'unnamed';
        for (let n = 0; n <= 100; n++) {
            const candidate = n === 0 ? leaf : LadexPolicy.numberedName(leaf, n);
            try {
                await dir.getFileHandle(candidate);
            } catch (err) {
                if (err.name === 'NotFoundError') return dir.getFileHandle(candidate, { create: true });
                if (err.name !== 'TypeMismatchError') throw err;
            }
        }
        throw new Error(`too many files named ${leaf}`);
    }

    // =====================================================================
    //  F4: UNSHARE / DELETE
    // =====================================================================

    /** Only the uploader can delete — the server enforces this too; this
     *  is just so the button doesn't even appear for anyone else. The
     *  confirmation toast fires from the 'file_removed' broadcast echo
     *  (see handleServerMessage), not here, so we don't double-toast. */
    deleteFile(fileId) {
        const f = this.serverFiles.find(x => x.id === fileId);
        this._pendingDeletes.set(fileId, f ? f.name : 'File');
        this.sendWS({ type: 'delete_file', session_id: this.sessionId, file_id: fileId });
    }

    // =====================================================================
    //  F3: SEND-TO-PERSON
    //  Push a file straight to one device instead of publishing it to the
    //  catalog for anyone to find — drag a file row onto a device chip, or
    //  click a chip to pick a file. The target gets a consent prompt
    //  (handleServerMessage 'incoming_file_offer'); accepting reuses the
    //  normal download, from this node.
    // =====================================================================

    offerFileToPeer(fileId, targetSessionId) {
        const file = this.serverFiles.find((f) => f.id === fileId);
        if (!file) {
            this.toast('That file is no longer available', 'error');
            return;
        }
        this.sendWS({
            type: 'offer_file_to',
            session_id: this.sessionId,
            target_session_id: targetSessionId,
            file_id: fileId,
        });
        const name = this._deviceDisplayName(targetSessionId, this.peers.get(targetSessionId));
        this.toast(`Offered "${file.name}" to ${name}`, 'info');
    }

    _showSendFilePicker(anchorEl, targetSessionId) {
        document.querySelector('.send-file-popover')?.remove();
        // What this device has shared (folders included, not the files inside them).
        const options = this.serverFiles
            .filter((f) => f.uploader_id === this.sessionId && !f.parent)
            .map((f) => ({ id: f.id, name: f.name, icon: f.is_folder ? '📁' : '📄' }));
        if (options.length === 0) {
            this.toast('You have nothing to send — upload something first', 'info');
            return;
        }

        const targetName = this.escapeHtml(this._deviceDisplayName(targetSessionId, this.peers.get(targetSessionId)));
        const popover = document.createElement('div');
        popover.className = 'send-file-popover';
        popover.innerHTML = `
            <div class="send-file-popover-title">Send to ${targetName}</div>
            ${options.map(o => `
                <button class="send-file-option" data-file-id="${this.escapeHtml(o.id)}">${o.icon} ${this.escapeHtml(o.name)}</button>
            `).join('')}
        `;
        document.body.appendChild(popover);

        const rect = anchorEl.getBoundingClientRect();
        popover.style.left = `${Math.min(rect.left, window.innerWidth - popover.offsetWidth - 16)}px`;
        popover.style.top = `${rect.bottom + 6}px`;

        popover.addEventListener('click', (e) => {
            const btn = e.target.closest('.send-file-option');
            if (!btn) return;
            this.offerFileToPeer(btn.dataset.fileId, targetSessionId);
            popover.remove();
        });

        // Close on outside click — deferred so this same click doesn't fire it
        setTimeout(() => {
            const closeHandler = (e) => {
                if (!popover.contains(e.target)) {
                    popover.remove();
                    document.removeEventListener('click', closeHandler);
                }
            };
            document.addEventListener('click', closeHandler);
        }, 0);
    }

    /**
     * F3: someone pointed us at a file. Accepting just downloads it.
     */
    _showFileOfferDialog(msg) {
        const file = this.serverFiles.find((f) => f.id === msg.file_id);
        const senderName = this.escapeHtml(this._deviceDisplayName(msg.from_session_id, this.peers.get(msg.from_session_id)));
        const fileName = file ? this.escapeHtml(file.name) : 'a file';
        const sizeStr = file ? this.formatFileSize(file.is_folder ? file.folder_bytes : file.size) : '';

        document.getElementById('file-offer-dialog')?.remove();
        const dialog = document.createElement('div');
        dialog.id = 'file-offer-dialog';
        dialog.className = 'incoming-file-dialog';
        dialog.innerHTML = `
            <div class="ifd-inner">
                <div class="ifd-icon">📨</div>
                <div class="ifd-title"><strong>${senderName}</strong> wants to send you a file</div>
                <div class="ifd-name">&ldquo;${fileName}&rdquo;</div>
                <div class="ifd-size">${sizeStr}</div>
                <div class="ifd-actions">
                    <button id="fod-accept" class="btn btn-primary">Accept</button>
                    <button id="fod-decline" class="btn btn-danger">Decline</button>
                </div>
            </div>
        `;
        document.body.appendChild(dialog);

        document.getElementById('fod-accept').addEventListener('click', () => {
            dialog.remove();
            this.downloadFile(msg.file_id);
        });
        document.getElementById('fod-decline').addEventListener('click', () => {
            dialog.remove();
            this.sendWS({
                type: 'decline_file_offer',
                session_id: this.sessionId,
                target_session_id: msg.from_session_id,
                file_id: msg.file_id,
            });
        });
    }

    // =====================================================================
    //  UI
    // =====================================================================

    updateConnectionStatus(connected) {
        const el = document.getElementById('connection-status');
        if (!el) return;
        el.textContent = connected ? 'Connected' : 'Disconnected';
        el.className   = connected ? 'status-connected' : 'status-disconnected';
    }

    updatePeerStatus(count) {
        const ps = document.getElementById('peer-status');
        const pn = document.getElementById('peer-number');
        if (ps) ps.textContent = `Connected peers: ${count}`;
        if (pn) pn.textContent = `Peer: ${this.getShortPeerId()}`;
    }

    setupEventListeners() {
        document.getElementById('upload-files-btn').addEventListener('click', () => {
            document.getElementById('file-input-single').click();
        });
        document.getElementById('upload-folder-btn').addEventListener('click', () => {
            document.getElementById('file-input').click();
        });
        document.getElementById('file-input-single').addEventListener('change', (e) => {
            this.handleFileUpload(e.target.files, false);
            e.target.value = '';
        });
        document.getElementById('file-input').addEventListener('change', (e) => {
            this.handleFileUpload(e.target.files, true);
            e.target.value = '';
        });
        document.getElementById('cancel-all-transfers').addEventListener('click', () => {
            this.cancelActiveTransfer();
        });
        document.getElementById('send-message-btn').addEventListener('click', () => {
            this.sendTextMessage();
        });
        document.getElementById('message-input').addEventListener('keydown', (e) => {
            if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); this.sendTextMessage(); }
        });
        document.getElementById('message-input').addEventListener('input', (e) => {
            this.autoResizeTextarea(e.target);
            this.updateSendButton();
        });
        document.getElementById('close-messages').addEventListener('click', () => {
            this.hideMessageModal();
        });
        document.getElementById('copy-message-btn').addEventListener('click', () => {
            this.copyMessageContent();
        });
        document.getElementById('messages-modal').addEventListener('click', (e) => {
            if (e.target.id === 'messages-modal') this.hideMessageModal();
        });
        document.getElementById('add-device-btn').addEventListener('click', () => {
            this.showAddDeviceModal();
        });
        document.getElementById('close-add-device').addEventListener('click', () => {
            this.hideAddDeviceModal();
        });
        document.getElementById('add-device-modal').addEventListener('click', (e) => {
            if (e.target.id === 'add-device-modal') this.hideAddDeviceModal();
        });
        // BUG-05 fix: delegated handler for the file/message list, which is
        // rebuilt (and re-populated with peer-controlled data) on every
        // updateFileList() call — data-action attributes instead of inline
        // onclick="app.foo('${id}')" strings.
        document.getElementById('files-list').addEventListener('click', (e) => {
            const btn = e.target.closest('[data-action]');
            if (!btn) return;
            if (btn.dataset.action === 'download-file') {
                this.downloadFile(btn.dataset.fileId);
            } else if (btn.dataset.action === 'view-message') {
                this.viewMessage(btn.dataset.messageId);
            } else if (btn.dataset.action === 'delete-file') {
                this.deleteFile(btn.dataset.fileId);
            }
        });
        // F3: drag a file row onto a device chip to send it directly
        document.getElementById('files-list').addEventListener('dragstart', (e) => {
            const row = e.target.closest('tr.file-row');
            if (!row) return;
            e.dataTransfer.setData('text/plain', row.dataset.fileId);
            e.dataTransfer.effectAllowed = 'copy';
        });
        // F3 / F5: devices strip — click self to rename, click a peer (or
        // drop a file row on one) to send them a file
        const devicesList = document.getElementById('devices-list');
        devicesList.addEventListener('click', (e) => {
            const chip = e.target.closest('.device-chip');
            if (!chip) return;
            if (chip.dataset.sessionId === this.sessionId) {
                this._editNicknameInline(chip);
            } else {
                this._showSendFilePicker(chip, chip.dataset.sessionId);
            }
        });
        devicesList.addEventListener('dragover', (e) => {
            const chip = e.target.closest('.device-chip:not(.device-chip-self)');
            if (!chip) return;
            e.preventDefault();
            chip.classList.add('device-chip-dragover');
        });
        devicesList.addEventListener('dragleave', (e) => {
            e.target.closest('.device-chip')?.classList.remove('device-chip-dragover');
        });
        devicesList.addEventListener('drop', (e) => {
            const chip = e.target.closest('.device-chip:not(.device-chip-self)');
            if (!chip) return;
            e.preventDefault();
            chip.classList.remove('device-chip-dragover');
            const fileId = e.dataTransfer.getData('text/plain');
            if (fileId) this.offerFileToPeer(fileId, chip.dataset.sessionId);
        });
        this.updateSendButton();
    }

    // ── File + message list ──────────────────────────────────────────────

    updateFileList(files) {
        const tbody = document.getElementById('files-list');
        const allItems = [];

        // The files inside a folder are listed by the folder, not on their own.
        if (files && files.length > 0) {
            files.filter(f => !f.parent).forEach(f => allItems.push({ type: 'file', data: f, timestamp: new Date(f.uploaded_at) }));
        }
        this.messages.forEach(m => allItems.push({ type: 'message', data: m, timestamp: new Date(m.timestamp) }));
        allItems.sort((a, b) => b.timestamp - a.timestamp);

        if (allItems.length === 0) {
            tbody.innerHTML = '<tr class="no-files"><td colspan="5">No files or messages shared yet!</td></tr>';
            return;
        }

        // BUG-05 fix: file/message ids, sender/host ids and content all
        // originate from other peers on the LAN (or, before the BUG-06 CSWSH
        // fix, potentially from an arbitrary website) — none of it is
        // trustworthy. Everything interpolated below is escaped, and the
        // two actions that used to be inline onclick="app.foo('${id}')"
        // handlers (a nested HTML-attribute-inside-JS-string context that's
        // easy to break out of even with escaping) are now data-action
        // attributes read by a single delegated listener in
        // setupEventListeners() instead.
        tbody.innerHTML = allItems.map(item => {
            if (item.type === 'file') {
                const f = item.data;
                const holders = this._holderNames(f);
                const isDownloading = this.pendingDownloads.has(f.id);
                // The device that shared it, or the machine running the node it was shared through.
                const canDelete = f.uploader_id === this.sessionId || (this.isHost && f.uploader_node === this.nodeId);
                const icon = f.is_folder ? '📁' : '📄';
                const typeLabel = f.is_folder ? `Folder · ${f.folder_files} file${f.folder_files === 1 ? '' : 's'}` : f.mime_type;
                const size = f.is_folder ? f.folder_bytes : f.size;
                return `
                    <tr class="file-row" draggable="true" data-file-id="${this.escapeHtml(f.id)}">
                        <td class="file-name">${icon} ${this.escapeHtml(f.name)}</td>
                        <td class="file-type">${this.escapeHtml(typeLabel)}</td>
                        <td class="file-size">${this.formatSize(size)}</td>
                        <td>
                            <div class="file-hosts">
                                ${holders.map(name => `<span class="host-badge">${this.escapeHtml(name)}</span>`).join('')}
                            </div>
                        </td>
                        <td class="file-actions">
                            ${holders.length > 0
                                ? `<button class="btn download${isDownloading ? ' downloading' : ''}" data-action="download-file" data-file-id="${this.escapeHtml(f.id)}" ${isDownloading ? 'disabled' : ''}>${isDownloading ? '⏳ Starting…' : '⬇️ Download'}</button>`
                                : '<span style="color:#a0aec0;" title="Every device that has this file is offline">Unavailable</span>'}
                            ${canDelete ? `<button class="btn secondary delete-file-btn" data-action="delete-file" data-file-id="${this.escapeHtml(f.id)}" title="Unshare">🗑️</button>` : ''}
                        </td>
                    </tr>`;
            } else {
                const m = item.data;
                const who = m.sender_id === this.sessionId ? 'You' : this.escapeHtml(this._deviceDisplayName(m.sender_id, this.peers.get(m.sender_id)));
                const preview = m.content.length > 50 ? m.content.substring(0, 50) + '…' : m.content;
                return `
                    <tr class="message-row">
                        <td class="file-name">💬 ${this.escapeHtml(preview)}</td>
                        <td class="file-type">Text Message</td>
                        <td class="file-size">${m.content.length} chars</td>
                        <td><span class="host-badge">${who}</span></td>
                        <td class="file-actions">
                            <button class="btn download" data-action="view-message" data-message-id="${this.escapeHtml(m.id)}">View</button>
                        </td>
                    </tr>`;
            }
        }).join('');
    }

    // =====================================================================
    //  PROGRESS PANEL  (BUG-12 fix)
    //
    //  Used to be one global modal shared by every send/receive — a second
    //  concurrent transfer would overwrite the first's numbers, and either
    //  one finishing/failing would call hideProgress() with no id and hide
    //  it out from under the other. Now it's a docked panel holding one
    //  card per transferId, so concurrent transfers each get their own
    //  progress bar, byte counter, and Cancel button that only cancels
    //  that one transfer.
    //
    //  transferId convention: `send:${fileId}:${targetSessionId}` for an
    //  upload (peer-specific — sending the same file to two peers at once
    //  is genuinely two transfers), `dl:${fileId}` for a download (NOT
    //  peer-specific — a retry can pick a different host for the same
    //  logical download, and the card should follow the download).
    // =====================================================================

    showProgress(transferId, filename, percentage, speedBytesPerSec, etaSeconds, transferredBytes, totalBytes) {
        const list = document.getElementById('progress-list');
        const panel = document.getElementById('progress-panel');
        if (!list || !panel) return;

        let card = this._progressCards.get(transferId);
        if (!card) {
            card = document.createElement('div');
            card.className = 'progress-card';
            card.innerHTML = `
                <div class="progress-info">
                    <span class="progress-filename"></span>
                    <span class="progress-percentage"></span>
                </div>
                <div class="progress-bar"><div class="progress-fill"></div></div>
                <div class="progress-stats">
                    <span class="progress-speed"></span>
                    <span class="progress-eta"></span>
                </div>
                <div class="progress-bytes"></div>
                <button class="btn secondary progress-cancel">Cancel</button>
            `;
            card.querySelector('.progress-cancel').addEventListener('click', () => {
                this.cancelTransfer(transferId);
            });
            list.appendChild(card);
            this._progressCards.set(transferId, card);
        }

        card.querySelector('.progress-filename').textContent = filename;
        card.querySelector('.progress-percentage').textContent = `${percentage}%`;
        card.querySelector('.progress-fill').style.width = `${percentage}%`;
        card.querySelector('.progress-speed').textContent = speedBytesPerSec != null
            ? `${this.formatFileSize(speedBytesPerSec)}/s`
            : '';

        const etaEl = card.querySelector('.progress-eta');
        if (etaSeconds != null && isFinite(etaSeconds)) {
            const s = Math.round(etaSeconds);
            if (s < 60) etaEl.textContent = `${s}s left`;
            else if (s < 3600) etaEl.textContent = `${Math.floor(s/60)}m ${s%60}s left`;
            else etaEl.textContent = `${Math.floor(s/3600)}h ${Math.floor((s%3600)/60)}m left`;
        } else {
            etaEl.textContent = 'Calculating…';
        }

        card.querySelector('.progress-bytes').textContent = totalBytes > 0
            ? `${this.formatFileSize(transferredBytes || 0)} / ${this.formatFileSize(totalBytes)}`
            : '';

        panel.classList.add('visible');
    }

    hideProgress(transferId) {
        const card = this._progressCards.get(transferId);
        if (card) {
            card.remove();
            this._progressCards.delete(transferId);
        }
        const panel = document.getElementById('progress-panel');
        if (panel && !this._progressCards.size) panel.classList.remove('visible');
    }

    /** Cancel one transfer by id — used by each card's own Cancel button. */
    cancelTransfer(transferId) {
        this.cancelledTransfers.add(transferId);
        for (const xhr of this._uploads.get(transferId) ?? []) xhr.abort();
        this._aborters.get(transferId)?.abort();
        this.hideProgress(transferId);
        this.toast('Transfer cancelled', 'info');
    }

    /** Cancel every in-flight transfer — bound to the panel's "Cancel all" button. */
    cancelActiveTransfer() {
        const ids = Array.from(this._progressCards.keys());
        for (const id of ids) this.cancelTransfer(id);
        if (ids.length === 0) this.toast('No transfers in progress', 'info');
    }

    /**
     * Phase 10 §10.4: Show a sticky AP isolation diagnostic banner.
     * Non-dismissible initially; user can close after reading.
     */
    _showApIsolationBanner(message) {
        if (document.getElementById('ap-isolation-banner')) return; // already shown
        const banner = document.createElement('div');
        banner.id = 'ap-isolation-banner';
        banner.className = 'system-banner system-banner-error';
        banner.innerHTML = `
            <div class="banner-content">
                <strong>⚠️ No other LADEX nodes found on this network.</strong>
                <span>If you expect other devices to be present, check:</span>
                <ul>
                    <li>All devices are on the <strong>same Wi-Fi network</strong></li>
                    <li><strong>AP/client isolation</strong> is disabled on your router (common on guest networks and mobile hotspots)</li>
                    <li>No firewall is blocking <strong>UDP port 7878</strong> or <strong>TCP port 8080</strong></li>
                </ul>
            </div>
            <button class="banner-dismiss" onclick="this.parentElement.remove()">✕</button>
        `;
        document.body.prepend(banner);
        console.warn('AP isolation diagnostic:', message);
    }

    showError(message) {
        console.error(message);
        this.toast(message, 'error', 6000);
    }
}

// =====================================================================
//  Bootstrap
// =====================================================================

document.addEventListener('DOMContentLoaded', () => {
    try {
        window.app = new LADEXApp();
    } catch (err) {
        // BUG-01 fix: if construction throws, window.app is never assigned
        // and every inline onclick="app.…" handler in the DOM fails with an
        // opaque "app is not defined" — surface the real cause instead.
        console.error('LADEX failed to start:', err);
        const banner = document.createElement('div');
        banner.style.cssText = 'position:fixed;top:0;left:0;right:0;z-index:9999;' +
            'background:#c0392b;color:#fff;padding:12px 16px;font:14px sans-serif;';
        banner.textContent = `LADEX failed to start: ${err.message || err}. ` +
            'Check the browser console for details, then reload the page.';
        document.body.appendChild(banner);
    }
});

window.addEventListener('beforeunload', (e) => {
    if (!window.app) return;
    // Leaving mid-upload stops it (the node keeps what it got, but the page does the sending).
    if ([...window.app._progressCards.keys()].some((id) => id.startsWith('up:'))) {
        e.preventDefault();
        e.returnValue = '';
    }
    if (window.app.ws) window.app.ws.close();
});

// =====================================================================
//  Text-message methods (prototype extensions — kept separate for clarity)
// =====================================================================

LADEXApp.prototype.sendTextMessage = function() {
    const input = document.getElementById('message-input');
    const content = input.value.trim();
    if (!content) return;
    this.sendWS({ type: 'text_message', session_id: this.sessionId, content });
    input.value = '';
    this.autoResizeTextarea(input);
    this.updateSendButton();
};

LADEXApp.prototype.handleTextMessage = function(msg) {
    this.messages.push(msg.message);
    this.updateFileList(this.serverFiles);
};

LADEXApp.prototype.handleMessageHistory = function(msg) {
    this.messages = msg.messages;
    this.updateFileList(this.serverFiles);
};

LADEXApp.prototype.viewMessage = function(messageId) {
    const m = this.messages.find(x => x.id === messageId);
    if (!m) return;
    const who = m.sender_id === this.sessionId ? 'You' : `User ${m.sender_id.slice(-6)}`;
    document.getElementById('modal-sender').textContent = who;
    document.getElementById('modal-time').textContent = new Date(m.timestamp).toLocaleString();
    document.getElementById('modal-message-text').textContent = m.content;
    document.getElementById('messages-modal').style.display = 'block';
};

LADEXApp.prototype.hideMessageModal = function() {
    document.getElementById('messages-modal').style.display = 'none';
};

// F1: QR code so a phone can join by scanning instead of typing the URL
LADEXApp.prototype.showAddDeviceModal = function() {
    const url = location.href;
    document.getElementById('qr-code-container').innerHTML = this._renderQrSvg(url);
    document.getElementById('add-device-url').textContent = url;
    document.getElementById('add-device-modal').style.display = 'block';
};

LADEXApp.prototype.hideAddDeviceModal = function() {
    document.getElementById('add-device-modal').style.display = 'none';
};

// Renders a QR code as inline SVG using the vendored qrcode.js encoder
LADEXApp.prototype._renderQrSvg = function(text, moduleSize = 6) {
    if (typeof qrcode === 'undefined') {
        return '<p style="color:#c0392b;padding:20px;">QR library not loaded.</p>';
    }
    const qr = qrcode(0, 'M'); // type 0 = auto-pick the smallest size that fits
    qr.addData(text);
    qr.make();
    const count = qr.getModuleCount();
    const size = count * moduleSize;
    let modules = '';
    for (let row = 0; row < count; row++) {
        for (let col = 0; col < count; col++) {
            if (qr.isDark(row, col)) {
                modules += `<rect x="${col * moduleSize}" y="${row * moduleSize}" width="${moduleSize}" height="${moduleSize}"/>`;
            }
        }
    }
    return `<svg viewBox="0 0 ${size} ${size}" xmlns="http://www.w3.org/2000/svg">` +
        `<rect width="${size}" height="${size}" fill="#fff"/>` +
        `<g fill="#16213e">${modules}</g></svg>`;
};

LADEXApp.prototype.copyMessageContent = function() {
    const text = `${document.getElementById('modal-sender').textContent} - ${document.getElementById('modal-time').textContent}\n${document.getElementById('modal-message-text').textContent}`;
    if (navigator.clipboard) {
        navigator.clipboard.writeText(text).then(() => this.showCopyFeedback()).catch(() => this.fallbackCopy(text));
    } else {
        this.fallbackCopy(text);
    }
};

LADEXApp.prototype.fallbackCopy = function(text) {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.cssText = 'position:fixed;left:-9999px';
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand('copy'); this.showCopyFeedback(); } catch (e) { /* */ }
    document.body.removeChild(ta);
};

LADEXApp.prototype.showCopyFeedback = function() {
    const btn = document.getElementById('copy-message-btn');
    btn.title = 'Copied!';
    btn.style.background = 'rgba(120,219,226,0.3)';
    btn.style.borderColor = 'rgba(120,219,226,0.6)';
    setTimeout(() => { btn.title = 'Copy Message'; btn.style.background = ''; btn.style.borderColor = ''; }, 1500);
};

LADEXApp.prototype.autoResizeTextarea = function(ta) {
    ta.style.height = 'auto';
    ta.style.height = Math.min(ta.scrollHeight, 100) + 'px';
};

LADEXApp.prototype.updateSendButton = function() {
    const btn = document.getElementById('send-message-btn');
    btn.disabled = !document.getElementById('message-input').value.trim();
};

// BUG-05 fix: the previous implementation (textContent → innerHTML
// round-trip through a scratch <div>) only escapes characters that are
// special in HTML *text* content (&, <, >). It leaves " and ' untouched,
// which is fine for text nodes but unsafe wherever escaped output lands
// inside an HTML attribute value (e.g. data-file-id="${...}") — an
// attacker-controlled id containing `"` could close the attribute early
// and inject new ones. This version is safe in both contexts.
LADEXApp.prototype.escapeHtml = function(text) {
    return String(text)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
};