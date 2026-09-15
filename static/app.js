// ============================================================================
// LADEX — Local Area Data Exchange
// Pure P2P file transfer over WebRTC DataChannels.
// The server is only a signaling relay + file catalog.  Zero bytes of file
// data ever touch the server.
// ============================================================================

// Above this size we skip client-side hashing entirely — reading the whole
// file into RAM just to hash it would defeat the O(1)-memory streaming
// design (see streamFileOverDC). The receiver still gets the transfer; it
// just has no sha256 to verify against (sha256 is sent as null and checked
// lazily — see the wire protocol comment on streamFileOverDC).
const HASH_SIZE_LIMIT = 200 * 1024 * 1024; // 200 MB

class LADEXApp {
    constructor() {
        this.ws = null;
        this.sessionId = this.generateSessionId();

        // File *references* (File objects) stored by fileId.
        // A File object is a handle to the on-disk blob — costs ~0 RAM
        // regardless of file size.  We only read slices on demand.
        this.files = new Map();
        // F6: folders we're hosting, kept separate from single files since
        // a folder isn't one File object — folderId → { name, entries:
        // [{relativePath, file}], totalSize }. See handleFolderUpload.
        this.folders = new Map();

        this.peers = new Map();
        this.messages = [];
        this.serverFiles = [];
        // Phase 6: RTT-aware peer map (same data as this.peers but refreshed via peer_sync)
        // node_rtt_ms is stored on each PeerInfo when received from the server.

        // F5: editable device nickname, persisted per-browser
        this.nickname = localStorage.getItem('ladex_nickname') || '';
        // F3: peers whose next incoming DataChannel we already have consent
        // for — set when the user accepts an IncomingFileOffer, consumed by
        // _handleIncomingDCWithConsent so it doesn't ask a second time.
        this._preAcceptedTransfers = new Set();
        // F4: fileId → name for deletes this tab initiated, so the
        // 'file_removed' broadcast echo can confirm with the right message
        // instead of a generic one (or a duplicate toast).
        this._pendingDeletes = new Map();
        // F8: fileId → { fileHandle, bytesReceived } for downloads in
        // progress or interrupted — kept across retries (within this page
        // session) so a dropped connection resumes instead of restarting
        // from byte zero. Cleared on completion, cancel, or giving up.
        this._resumeState = new Map();
        // F7: fileId → 'verified' | 'failed', for files this tab downloaded
        // and checked against the sender's sha256 — drives the small badge
        // in the file list.
        this._integrityStatus = new Map();

        // ── WebRTC state ────────────────────────────────────────────────
        // Active RTCPeerConnections keyed by remote sessionId.
        this.rtcConnections = new Map();
        // ICE candidates that arrived before the remote description was set.
        this.pendingCandidates = new Map();
        // Track ongoing sends/receives for progress UI.
        this.activeTransfers = new Map();
        // BUG-12 fix: one progress-panel card, and one known RTC peer, per
        // transferId — see the "PROGRESS PANEL" section below.
        this._progressCards = new Map();
        this._transferPeers = new Map();

        this.RTC_CHUNK_SIZE = 256 * 1024; // Phase 9: 256 KB (up from 64 KB)

        // ── Retry / resilience ──────────────────────────────────────────
        this.MAX_RETRIES = 3;
        this.RETRY_BASE_DELAY = 1000; // ms — exponential back-off base
        this.ICE_TIMEOUT = 15000;     // ms — give up if no ICE connection
        // Downloads currently in-flight (prevents double-click issues)
        this.pendingDownloads = new Set();
        // Cancelled transfer ids (so async loops can bail out)
        this.cancelledTransfers = new Set();

        // ── Toast queue ─────────────────────────────────────────────────
        this._toastContainer = null;

        // ── Phase 11.3: SHA-256 integrity worker ────────────────────────
        // One persistent worker — reused for all hash requests.
        // Map from fileId → resolve/reject callbacks for pending hash ops.
        this._hashWorker = null;
        this._hashPending = new Map(); // fileId → { resolve, reject }
        // sha256 values we've computed for our own shared files
        // (also embedded in the DC header for immediate receiver-side check)
        this._localSha256 = new Map(); // fileId → hex string

        this.init();
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    generateSessionId() {
        return 'peer_' + Math.random().toString(36).substr(2, 9) + '_' + Date.now();
    }

    generateFileId() {
        return 'file_' + Math.random().toString(36).substr(2, 12) + '_' + Date.now();
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

    storeFile(fileId, file) { this.files.set(fileId, file); }
    getFile(fileId)         { return this.files.get(fileId) || null; }

    // ── Initialization ──────────────────────────────────────────────────

    init() {
        console.log('Initializing LADEX …');
        console.log('Session ID:', this.sessionId);
        this.initializePeerDisplay();
        this.connectWebSocket();
        this.setupEventListeners();
        this.setupDragAndDrop();
        this.updateDevicesList();
        // Phase 8: warn non-FSAA browsers once on load
        if (!window.showSaveFilePicker) {
            this._showFsaaBanner();
        }
        // Phase 11.3: start SHA-256 Web Worker
        this._initHashWorker();
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
    //  PHASE 11.3: SHA-256 INTEGRITY WORKER  (BUG-01 fix)
    //  These two methods were called from the constructor/init() and
    //  uploadFile() but never defined — every `new LADEXApp()` threw a
    //  ReferenceError before `window.app` could be assigned, so every
    //  inline onclick="app.…" handler in the DOM threw too. The whole UI
    //  was non-functional as a result.
    // =====================================================================

    _initHashWorker() {
        try {
            this._hashWorker = new Worker('static/sha256-worker.js');
        } catch (err) {
            console.warn('SHA-256 worker unavailable — integrity checks disabled:', err);
            this._hashWorker = null;
            return;
        }

        this._hashWorker.onmessage = (event) => {
            const { cmd, fileId, sha256, error } = event.data;
            const pending = this._hashPending.get(fileId);
            if (!pending) return; // already resolved, or an unknown/stale fileId
            this._hashPending.delete(fileId);
            if (cmd === 'hash_result') {
                pending.resolve(sha256);
            } else {
                pending.reject(new Error(error || 'hash failed'));
            }
        };

        this._hashWorker.onerror = (err) => {
            console.error('SHA-256 worker crashed:', err.message || err);
            // Reject everything still pending — callers fall back to "no hash".
            for (const pending of this._hashPending.values()) {
                pending.reject(new Error('worker crashed'));
            }
            this._hashPending.clear();
            this._hashWorker = null;
        };
    }

    /**
     * Hash a locally-shared file off the main thread, then push the result
     * to the server so peers can verify integrity after download.
     * Best-effort: failures are logged, never surfaced to the user — a
     * transfer without a checksum still works, it's just unverified.
     */
    async _hashFileAsync(fileId, file) {
        if (!this._hashWorker) return;
        if (file.size === 0 || file.size > HASH_SIZE_LIMIT) return;

        try {
            const buffer = await file.arrayBuffer();
            const sha256 = await new Promise((resolve, reject) => {
                this._hashPending.set(fileId, { resolve, reject });
                this._hashWorker.postMessage({ cmd: 'hash_file', fileId, buffer }, [buffer]);
            });
            this._localSha256.set(fileId, sha256);
            // Patch the catalog entry so peers who already fetched the
            // metadata (and anyone downloading after this point) can verify.
            this.sendWS({
                type: 'file_checksum_update',
                session_id: this.sessionId,
                file_id: fileId,
                sha256,
            });
            console.log(`[hash] ${file.name}: ${sha256.slice(0, 12)}…`);
        } catch (err) {
            console.warn(`[hash] failed for ${file.name}:`, err);
        }
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
            // Re-register locally-hosted files after reconnect so the
            // catalog is accurate.
            for (const [fileId, file] of this.files.entries()) {
                this.sendWS({
                    type: 'file_upload',
                    session_id: this.sessionId,
                    file: {
                        id: fileId,
                        name: file.name,
                        size: file.size,
                        mime_type: file.type || 'application/octet-stream',
                        uploader_id: this.sessionId,
                        hosts: [this.sessionId],
                        uploaded_at: new Date().toISOString(),
                    }
                });
            }
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
            setTimeout(() => this.connectWebSocket(), 3000);
        };

        this.ws.onerror = (err) => console.error('WS error:', err);
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
                this.peers.set(msg.peer.session_id, msg.peer);
                this.updatePeerStatus(msg.total_peers);
                this.updateDevicesList();
                break;
            case 'peer_left':
                this.peers.delete(msg.session_id);
                this.updatePeerStatus(msg.total_peers);
                // Clean up any RTC connection to that peer
                this.closeRTC(msg.session_id);
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
                            const existing = this.peers.get(peer.session_id) || {};
                            this.peers.set(peer.session_id, { ...existing, ...peer });
                        }
                    }
                }
                this.updateDevicesList();
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

            // ── download orchestration ──────────────────────────────────
            case 'download_request':
                this.handleDownloadRequest(msg);
                break;

            // Phase 6: host not reachable — let client retry with different host
            case 'host_unreachable':
                this.pendingDownloads.delete(msg.file_id);
                this.toast(
                    `Host ${msg.host_peer_id.slice(-6)} unreachable — choose another or retry`,
                    'warning', 5000
                );
                this.hideProgress(`dl:${msg.file_id}`);
                break;

            // Phase 5: remote receiver declined the file
            case 'transfer_declined':
                this.toast(`Receiver declined the file transfer`, 'warning', 5000);
                this.hideProgress(`send:${msg.file_id}:${msg.from_session_id}`);
                break;

            // F3: someone is offering to send us a file directly
            case 'incoming_file_offer':
                this._showFileOfferDialog(msg);
                break;
            case 'file_offer_declined': {
                const name = this._deviceDisplayName(msg.from_session_id, this.peers.get(msg.from_session_id));
                this.toast(`${name} declined your file offer`, 'warning');
                break;
            }

            // ── WebRTC signaling ────────────────────────────────────────
            case 'webrtc_offer':
                this.handleWebRTCOffer(msg);
                break;
            case 'webrtc_answer':
                this.handleWebRTCAnswer(msg);
                break;
            case 'ice_candidate':
                this.handleICECandidate(msg);
                break;

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

    // =====================================================================
    //  FILE UPLOAD  — register metadata only (file stays on disk)
    // =====================================================================

    async handleFileUpload(files, isFolder) {
        if (!files || files.length === 0) return;
        try {
            if (isFolder) {
                await this.handleFolderUpload(files);
            } else {
                for (const file of files) await this.uploadFile(file);
                const count = files.length;
                this.toast(`${count} file${count > 1 ? 's' : ''} shared`, 'success');
            }
        } catch (err) {
            this.toast(`Upload failed: ${err.message}`, 'error');
        }
    }

    /**
     * F6: real folder transfer. Used to zip the whole folder into one file
     * at upload time (RAM-bound, and it turned "share a folder" into
     * "share a .zip" — no directory structure on the other end, no
     * per-file progress). Now a folder is its own catalog entry
     * (is_folder: true); the actual files stream individually, in order,
     * over the DataChannel when someone downloads it — see
     * streamFolderOverDC. JSZip is still used, but only as the
     * *receiving* end's fallback for browsers without showDirectoryPicker
     * (see _receiveFolderToZip) — it never touches the sending side, or a
     * browser that can write directly to a real folder, any more.
     */
    async handleFolderUpload(files) {
        const folderId = this.generateFileId();
        const folderName = files[0].webkitRelativePath?.split('/')[0] || 'folder';
        const entries = Array.from(files).map(f => ({
            relativePath: f.webkitRelativePath || f.name,
            file: f,
        }));
        const totalSize = entries.reduce((sum, e) => sum + e.file.size, 0);

        this.folders.set(folderId, { name: folderName, entries, totalSize });

        this.sendWS({
            type: 'file_upload',
            session_id: this.sessionId,
            file: {
                id: folderId,
                name: folderName,
                size: totalSize,
                mime_type: 'inode/directory',
                uploader_id: this.sessionId,
                hosts: [this.sessionId],
                uploaded_at: new Date().toISOString(),
                is_folder: true,
            },
        });
        console.log(`Shared folder: ${folderName} (${entries.length} files, ${this.formatFileSize(totalSize)})`);
        this.toast(`Folder shared: ${folderName} (${entries.length} file${entries.length > 1 ? 's' : ''})`, 'success');
    }

    async _zipToDisk(zip, zipName) {
        let fileHandle;
        try {
            fileHandle = await window.showSaveFilePicker({
                suggestedName: zipName,
                types: [{ description: 'Zip archive', accept: { 'application/zip': ['.zip'] } }],
            });
        } catch (err) {
            if (err.name === 'AbortError') return null; // user cancelled the save dialog
            console.warn('showSaveFilePicker failed, falling back to in-memory zip:', err);
            return this._zipToMemory(zip, zipName);
        }

        const writable = await fileHandle.createWritable();
        let failure = null;

        // generateInternalStream + streamFiles avoids buffering the whole
        // archive; pausing the stream while each chunk is written to disk
        // gives real backpressure, so only one chunk is ever in memory.
        await new Promise((resolve) => {
            const stream = zip.generateInternalStream({ type: 'uint8array', streamFiles: true });
            stream.on('data', (chunk) => {
                stream.pause();
                writable.write(chunk)
                    .then(() => stream.resume())
                    .catch((err) => { failure = err; resolve(); });
            });
            stream.on('error', (err) => { failure = err; resolve(); });
            stream.on('end', resolve);
            stream.resume();
        });

        if (failure) {
            await writable.close().catch(() => {});
            throw failure;
        }
        await writable.close();
        return fileHandle.getFile();
    }

    async _zipToMemory(zip, zipName) {
        const blob = await zip.generateAsync({ type: 'blob' });
        return new File([blob], zipName, { type: 'application/zip' });
    }

    async uploadFile(file) {
        const fileId = this.generateFileId();
        // Store the File reference locally (zero RAM — it's a disk handle)
        this.storeFile(fileId, file);

        // Tell the server about this file (metadata only, no bytes)
        this.sendWS({
            type: 'file_upload',
            session_id: this.sessionId,
            file: {
                id: fileId,
                name: file.name,
                size: file.size,
                mime_type: file.type || 'application/octet-stream',
                uploader_id: this.sessionId,
                hosts: [this.sessionId],
                uploaded_at: new Date().toISOString(),
            }
        });
        console.log(`Shared: ${file.name} (${this.formatFileSize(file.size)})`);
        // Phase 11.3: hash the file asynchronously; patch catalog when done
        this._hashFileAsync(fileId, file);
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
    //  DOWNLOAD REQUEST — with retry
    // =====================================================================

    /** User clicks "Download" — Phase 6: client selects host, sends request_download_from. */
    downloadFile(fileId) {
        if (this.pendingDownloads.has(fileId)) {
            this.toast('Download already in progress', 'warning');
            return;
        }
        // F6: if we're hosting this folder ourselves, there's nothing to
        // download — it's already wherever the user picked it from.
        if (this.folders.has(fileId)) {
            this.toast('You already have this folder', 'info');
            return;
        }
        // If WE already have the file locally, just save it.
        const localFile = this.getFile(fileId);
        if (localFile) {
            this.saveFileToDisk(localFile, localFile.name);
            this.toast(`Saved ${localFile.name}`, 'success');
            return;
        }
        this.pendingDownloads.add(fileId);
        this._requestDownloadWithRetry(fileId, 0);
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
    //  normal request_download_from flow, which is how the actual transfer
    //  happens (see _handleIncomingDCWithConsent's pre-accept bypass).
    // =====================================================================

    offerFileToPeer(fileId, targetSessionId) {
        // F6: a folder isn't in this.files (see handleFolderUpload) — check
        // both so offering a folder works the same way as offering a file.
        const file = this.getFile(fileId) || this.folders.get(fileId);
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
        // F6: offer folders alongside files — see handleFolderUpload.
        const options = [
            ...Array.from(this.files.entries()).map(([id, f]) => ({ id, name: f.name, icon: '📄' })),
            ...Array.from(this.folders.entries()).map(([id, f]) => ({ id, name: f.name, icon: '📁' })),
        ];
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
     * Phase 6: Pick the best host from the merged peer list using RTT.
     * Falls back to random if RTT data isn't available yet.
     * Returns null if no suitable host is found.
     */
    _pickBestHost(fileId) {
        const fileMeta = this.serverFiles.find(f => f.id === fileId);
        if (!fileMeta) return null;

        const hosts = Array.isArray(fileMeta.hosts)
            ? fileMeta.hosts
            : Array.from(fileMeta.hosts || []);

        // Exclude ourselves (we don't have it locally or we'd have returned early)
        const candidates = hosts.filter(h => h !== this.sessionId);
        if (candidates.length === 0) return null;

        // Sort by RTT: peers with known RTT first (ascending), then unknown.
        candidates.sort((a, b) => {
            const peerA = this.peers.get(a);
            const peerB = this.peers.get(b);
            const rttA = peerA?.node_rtt_ms ?? Infinity;
            const rttB = peerB?.node_rtt_ms ?? Infinity;
            return rttA - rttB;
        });

        return candidates[0]; // Best known host
    }

    _requestDownloadWithRetry(fileId, attempt) {
        if (attempt >= this.MAX_RETRIES) {
            this.pendingDownloads.delete(fileId);
            this._resumeState.delete(fileId); // give up — don't resume a dead attempt later
            this.toast(`Download failed after ${this.MAX_RETRIES} attempts`, 'error', 6000);
            this.hideProgress(`dl:${fileId}`);
            return;
        }
        if (attempt > 0) {
            const name = this._fileNameFromCatalog(fileId) || fileId.slice(-8);
            this.toast(`Retrying download of ${name} (attempt ${attempt + 1}/${this.MAX_RETRIES})…`, 'warning');
        }

        // Phase 6: client picks the host explicitly
        const chosenHost = this._pickBestHost(fileId);
        if (!chosenHost) {
            // No eligible host known yet — fall back to server-side selection
            this.sendWS({ type: 'request_download', session_id: this.sessionId, file_id: fileId });
            return;
        }

        // F8: resume from wherever we already got to, if anywhere — any
        // host serves identical bytes for this fileId, so it doesn't
        // matter if a retry picks a different one than last time.
        const resumeFromBytes = this._resumeState.get(fileId)?.bytesReceived || 0;
        console.log(`Requesting ${fileId} from host ${chosenHost.slice(-6)} (attempt ${attempt + 1}${resumeFromBytes ? `, resuming from ${this.formatFileSize(resumeFromBytes)}` : ''})`);
        this.activeTransfers.set(`retry:${fileId}`, { attempt, fileId });
        this.sendWS({
            type: 'request_download_from',
            session_id: this.sessionId,
            file_id: fileId,
            host_peer_id: chosenHost,
            resume_from_bytes: resumeFromBytes,
        });
    }

    /** Look up a file's display name from the server catalog. */
    _fileNameFromCatalog(fileId) {
        const f = this.serverFiles.find(f => f.id === fileId);
        return f ? f.name : null;
    }

    /**
     * Called when an incoming transfer fails (RTC/DC error).
     * Retries with exponential back-off.
     *
     * BUG-09 fix: this used to schedule the retry unconditionally.
     * cancelActiveTransfer() marks the transfer cancelled and closes the
     * RTC connection, which is exactly what triggers the onclose/onerror
     * handlers that call _retryDownload() in the first place — so
     * cancelling a download made it retry itself, and since neither this
     * function nor _requestDownloadWithRetry() ever checked for
     * cancellation, the exponential-backoff chain (up to MAX_RETRIES
     * attempts) ran to completion regardless of what the user asked for.
     * pendingDownloads is the authoritative "still wanted" set — cleared by
     * cancelActiveTransfer() and by a completed/exhausted download — so an
     * absent entry here means "don't resurrect this."
     */
    _retryDownload(fileId) {
        if (!this.pendingDownloads.has(fileId)) return;

        const retryInfo = this.activeTransfers.get(`retry:${fileId}`);
        const attempt = retryInfo ? retryInfo.attempt + 1 : 1;
        this.activeTransfers.delete(`retry:${fileId}`);

        const delay = this.RETRY_BASE_DELAY * Math.pow(2, attempt - 1);
        setTimeout(() => {
            if (!this.pendingDownloads.has(fileId)) return; // cancelled while waiting
            this._requestDownloadWithRetry(fileId, attempt);
        }, delay);
    }

    /**
     * Server tells us: "you are a host — send file X to requester Y".
     * We (the host) initiate a WebRTC connection and stream the file.
     */
    handleDownloadRequest(msg) {
        const { file_id, requester_session_id, resume_from_bytes } = msg;

        // F6: folders live in a separate map from single files (see
        // handleFolderUpload) — same request/response protocol, different
        // stream once the DataChannel is open.
        const bundle = this.folders.get(file_id);
        if (bundle) {
            console.log(`Initiating folder transfer of ${bundle.name} → ${requester_session_id.slice(-6)}`);
            this.initiateFolderSend(requester_session_id, file_id, bundle);
            return;
        }

        const file = this.getFile(file_id);
        if (!file) {
            console.error('Download request for file we don\'t have:', file_id);
            return;
        }
        console.log(`Initiating WebRTC transfer of ${file.name} → ${requester_session_id.slice(-6)}`);
        this.initiateWebRTCSend(requester_session_id, file_id, file, resume_from_bytes || 0);
    }

    // =====================================================================
    //  WEBRTC — PeerConnection + DataChannel manager
    // =====================================================================

    getRTCConfig() {
        // Phase 0 (LAN-only): STUN/TURN removed.
        //
        // On a single Wi-Fi/hotspot subnet, all peers are L2-adjacent —
        // ICE resolves to "typ host" local-IP candidates directly.
        // STUN adds an unnecessary external round-trip; TURN is dead weight
        // (we don't have NAT traversal problems on a LAN).
        //
        // If connection establishment fails in testing, the first step is to
        // confirm both devices are on the same subnet.  Do NOT re-add STUN
        // as a workaround without first ruling out AP/client isolation
        // (see ROADMAP.md §1.4).
        return {
            iceServers: [],         // LAN-only: host candidates suffice
            iceCandidatePoolSize: 0,
        };
    }

    // ── Sender side (host) ──────────────────────────────────────────────

    /**
     * Shared RTCPeerConnection + DataChannel bring-up for the sender side —
     * used by both a single-file send (initiateWebRTCSend) and a folder
     * send (F6, initiateFolderSend). Calls onOpen(dc) once the channel is
     * open; the caller decides what protocol to stream over it.
     */
    async _createSenderConnection(targetSessionId, dcLabel, transferId, onOpen) {
        // Clean up any previous connection to this peer
        this.closeRTC(targetSessionId);

        const pc = new RTCPeerConnection(this.getRTCConfig());
        this.rtcConnections.set(targetSessionId, pc);

        // ICE timeout — if we never connect, abort
        const iceTimer = setTimeout(() => {
            if (pc.connectionState !== 'connected') {
                console.warn('ICE timeout (sender) — aborting');
                this.closeRTC(targetSessionId);
            }
        }, this.ICE_TIMEOUT);

        const dc = pc.createDataChannel(dcLabel, { ordered: true });
        dc.binaryType = 'arraybuffer';

        dc.onopen = () => {
            clearTimeout(iceTimer);
            onOpen(dc);
        };

        dc.onclose = () => {
            console.log('DataChannel closed (sender side)');
        };

        dc.onerror = (err) => {
            clearTimeout(iceTimer);
            console.error('DataChannel error (sender):', err);
            this.activeTransfers.delete(transferId);
        };

        // ICE candidates → relay via server
        pc.onicecandidate = (e) => {
            if (e.candidate) {
                this.sendWS({
                    type: 'ice_candidate',
                    session_id: this.sessionId,
                    target_session_id: targetSessionId,
                    candidate: JSON.stringify(e.candidate),
                });
            }
        };

        pc.onconnectionstatechange = () => {
            if (pc.connectionState === 'connected') {
                clearTimeout(iceTimer);
            }
            if (pc.connectionState === 'failed' || pc.connectionState === 'disconnected') {
                clearTimeout(iceTimer);
                console.warn('RTC connection failed/disconnected (sender)');
                this.activeTransfers.delete(transferId);
                this.closeRTC(targetSessionId);
            }
        };

        // Create offer
        try {
            const offer = await pc.createOffer();
            await pc.setLocalDescription(offer);
        } catch (err) {
            clearTimeout(iceTimer);
            console.error('Failed to create offer:', err);
            this.closeRTC(targetSessionId);
            return;
        }

        this.sendWS({
            type: 'webrtc_offer',
            session_id: this.sessionId,
            target_session_id: targetSessionId,
            sdp: JSON.stringify(pc.localDescription),
        });

        // Flush any ICE candidates that arrived early
        this.flushPendingCandidates(targetSessionId, pc);
    }

    async initiateWebRTCSend(targetSessionId, fileId, file, resumeFromBytes = 0) {
        const transferId = `send:${fileId}:${targetSessionId}`;
        await this._createSenderConnection(targetSessionId, `file:${fileId}`, transferId, (dc) => {
            console.log(`DataChannel open → streaming ${file.name}`);
            this.streamFileOverDC(dc, fileId, file, targetSessionId, resumeFromBytes);
        });
    }

    // F6: folder send — see streamFolderOverDC for the wire protocol.
    async initiateFolderSend(targetSessionId, folderId, bundle) {
        const transferId = `send:${folderId}:${targetSessionId}`;
        await this._createSenderConnection(targetSessionId, `folder:${folderId}`, transferId, (dc) => {
            console.log(`DataChannel open → streaming folder ${bundle.name}`);
            this.streamFolderOverDC(dc, folderId, bundle, targetSessionId);
        });
    }

    /**
     * Stream a File over a DataChannel using file.slice() — never loads the
     * entire file into RAM.
     *
     * Wire protocol (per DataChannel message):
     *   Message 0:  UTF-8 JSON header
     *       { "fileId", "fileName", "fileSize", "mimeType", "totalChunks", "resumeFromBytes" }
     *   Messages 1..N:  raw ArrayBuffer chunks (binary, no base64), starting
     *                   at resumeFromBytes instead of byte 0 when resuming
     *                   (see F8 in _setupFSAAReceive / setupReceiverDC).
     */
    async streamFileOverDC(dc, fileId, file, targetSessionId, resumeFromBytes = 0) {
        const totalChunks = Math.ceil(file.size / this.RTC_CHUNK_SIZE);
        // F8: resume on a chunk boundary — the receiver only ever confirms
        // whole chunks written, so resumeFromBytes is already aligned in
        // practice; this floor is just defense in depth.
        const startChunk = Math.min(Math.floor(resumeFromBytes / this.RTC_CHUNK_SIZE), totalChunks);
        const resumeOffset = startChunk * this.RTC_CHUNK_SIZE;
        const transferId = `send:${fileId}:${targetSessionId}`;

        // 1. Send metadata header as a text message
        // Phase 11.3: include sha256 if we've already computed it
        const sha256 = this._localSha256.get(fileId) || null;
        try {
            dc.send(JSON.stringify({
                fileId,
                fileName: file.name,
                fileSize: file.size,
                mimeType: file.type || 'application/octet-stream',
                totalChunks,
                sha256, // null if hash not yet ready (large file), receiver verifies lazily
                resumeFromBytes: resumeOffset,
            }));
        } catch (err) {
            console.error('Failed to send file header:', err);
            this.closeRTC(targetSessionId);
            return;
        }

        this.activeTransfers.set(transferId, { startTime: Date.now(), bytesSent: resumeOffset, totalBytes: file.size });
        this._transferPeers.set(transferId, targetSessionId);
        // BUG-12 fix: each transfer gets its own progress card, keyed by
        // transferId — see showProgress()/hideProgress().
        this.showProgress(transferId, `Sending ${file.name}`, Math.round((resumeOffset / file.size) * 100), null, null, resumeOffset, file.size);

        // 2. Stream binary chunks using file.slice() (disk → DataChannel),
        // starting from the resume point if there is one
        for (let i = startChunk; i < totalChunks; i++) {
            // Check if this transfer was cancelled
            if (this.cancelledTransfers.has(transferId)) {
                this.cancelledTransfers.delete(transferId);
                this.activeTransfers.delete(transferId);
                console.log('Transfer cancelled (sender):', file.name);
                this.closeRTC(targetSessionId);
                return;
            }
            // Check DC is still open
            if (dc.readyState !== 'open') {
                console.warn('DataChannel closed mid-transfer (sender)');
                this.activeTransfers.delete(transferId);
                this.hideProgress(transferId);
                return;
            }

            const start = i * this.RTC_CHUNK_SIZE;
            const end   = Math.min(start + this.RTC_CHUNK_SIZE, file.size);
            const blob  = file.slice(start, end);
            const buf   = await blob.arrayBuffer();

            // Back-pressure: wait if the DC buffer is getting full
            let bpAttempts = 0;
            while (dc.bufferedAmount > 4 * 1024 * 1024) {
                await new Promise(r => setTimeout(r, 20));
                bpAttempts++;
                // Safety valve — if we're stuck for > 10 s something is wrong
                if (bpAttempts > 500) {
                    console.warn('Back-pressure timeout (sender)');
                    this.activeTransfers.delete(transferId);
                    this.hideProgress(transferId);
                    this.closeRTC(targetSessionId);
                    return;
                }
            }

            try {
                dc.send(buf);
            } catch (err) {
                console.error('DC send error:', err);
                this.activeTransfers.delete(transferId);
                this.hideProgress(transferId);
                this.closeRTC(targetSessionId);
                return;
            }

            // Progress
            const transfer = this.activeTransfers.get(transferId);
            if (transfer) {
                transfer.bytesSent = end;
                const pct = Math.round((end / file.size) * 100);
                const elapsed = (Date.now() - transfer.startTime) / 1000;
                const speed = elapsed > 0 ? end / elapsed : 0;
                const remaining = speed > 0 ? (file.size - end) / speed : 0;
                this.showProgress(transferId, `Sending ${file.name}`, pct, speed, remaining, end, file.size);
            }
        }

        this.activeTransfers.delete(transferId);
        this.hideProgress(transferId);
        this.toast(`Sent ${file.name}`, 'success');
        console.log(`File ${file.name} sent to ${targetSessionId.slice(-6)}`);

        // Keep the connection open briefly so the last chunk flushes, then close
        setTimeout(() => this.closeRTC(targetSessionId), 2000);
    }

    /**
     * F6: streams a whole folder over one DataChannel — a manifest, then
     * for each file (in order, one at a time — same bounded-memory
     * approach as a single-file send) a header followed by its chunks.
     *
     * Wire protocol:
     *   1. Text: { kind: 'folder-manifest', folderId, folderName, files:
     *      [{relativePath, size, mimeType}], totalBytes, totalFiles }
     *   2. Per file, in order:
     *        Text:   { kind: 'file-header', relativePath, size, mimeType }
     *        Binary: chunks (RTC_CHUNK_SIZE each)
     *   3. Text: { kind: 'folder-done' }
     */
    async streamFolderOverDC(dc, folderId, bundle, targetSessionId) {
        const transferId = `send:${folderId}:${targetSessionId}`;
        const { entries, totalSize, name } = bundle;

        try {
            dc.send(JSON.stringify({
                kind: 'folder-manifest',
                folderId,
                folderName: name,
                files: entries.map(e => ({
                    relativePath: e.relativePath,
                    size: e.file.size,
                    mimeType: e.file.type || 'application/octet-stream',
                })),
                totalBytes: totalSize,
                totalFiles: entries.length,
            }));
        } catch (err) {
            console.error('Failed to send folder manifest:', err);
            this.closeRTC(targetSessionId);
            return;
        }

        this.activeTransfers.set(transferId, { startTime: Date.now(), bytesSent: 0, totalBytes: totalSize });
        this._transferPeers.set(transferId, targetSessionId);
        this.showProgress(transferId, `Sending ${name}`, 0, null, null, 0, totalSize);

        let bytesSentTotal = 0;
        const startTime = Date.now();

        for (const entry of entries) {
            if (this.cancelledTransfers.has(transferId)) {
                this.cancelledTransfers.delete(transferId);
                this.activeTransfers.delete(transferId);
                console.log('Folder transfer cancelled (sender):', name);
                this.closeRTC(targetSessionId);
                return;
            }
            if (dc.readyState !== 'open') {
                console.warn('DataChannel closed mid-transfer (sender, folder)');
                this.activeTransfers.delete(transferId);
                this.hideProgress(transferId);
                return;
            }

            const file = entry.file;
            try {
                dc.send(JSON.stringify({
                    kind: 'file-header',
                    relativePath: entry.relativePath,
                    size: file.size,
                    mimeType: file.type || 'application/octet-stream',
                }));
            } catch (err) {
                console.error('Failed to send file header (folder):', err);
                this.closeRTC(targetSessionId);
                return;
            }

            const totalChunks = Math.ceil(file.size / this.RTC_CHUNK_SIZE);
            for (let i = 0; i < totalChunks; i++) {
                if (this.cancelledTransfers.has(transferId)) {
                    this.cancelledTransfers.delete(transferId);
                    this.activeTransfers.delete(transferId);
                    this.closeRTC(targetSessionId);
                    return;
                }
                if (dc.readyState !== 'open') {
                    this.activeTransfers.delete(transferId);
                    this.hideProgress(transferId);
                    return;
                }

                const start = i * this.RTC_CHUNK_SIZE;
                const end = Math.min(start + this.RTC_CHUNK_SIZE, file.size);
                const buf = await file.slice(start, end).arrayBuffer();

                let bpAttempts = 0;
                while (dc.bufferedAmount > 4 * 1024 * 1024) {
                    await new Promise(r => setTimeout(r, 20));
                    bpAttempts++;
                    if (bpAttempts > 500) {
                        console.warn('Back-pressure timeout (sender, folder)');
                        this.activeTransfers.delete(transferId);
                        this.hideProgress(transferId);
                        this.closeRTC(targetSessionId);
                        return;
                    }
                }

                try {
                    dc.send(buf);
                } catch (err) {
                    console.error('DC send error (folder):', err);
                    this.activeTransfers.delete(transferId);
                    this.hideProgress(transferId);
                    this.closeRTC(targetSessionId);
                    return;
                }

                bytesSentTotal += (end - start);
                const transfer = this.activeTransfers.get(transferId);
                if (transfer) {
                    transfer.bytesSent = bytesSentTotal;
                    const pct = Math.round((bytesSentTotal / totalSize) * 100);
                    const elapsed = (Date.now() - startTime) / 1000;
                    const speed = elapsed > 0 ? bytesSentTotal / elapsed : 0;
                    const remaining = speed > 0 ? (totalSize - bytesSentTotal) / speed : 0;
                    this.showProgress(transferId, `Sending ${name} (${entry.relativePath})`, pct, speed, remaining, bytesSentTotal, totalSize);
                }
            }
        }

        try {
            dc.send(JSON.stringify({ kind: 'folder-done' }));
        } catch (_) { /* best-effort — the receiver also completes by byte count */ }

        this.activeTransfers.delete(transferId);
        this.hideProgress(transferId);
        this.toast(`Sent folder ${name}`, 'success');
        console.log(`Folder ${name} sent to ${targetSessionId.slice(-6)}`);
        setTimeout(() => this.closeRTC(targetSessionId), 2000);
    }

    // ── Receiver side (requester) ────────────────────────────────────────

    /** Server relays an SDP offer from a host that will send us a file. */
    async handleWebRTCOffer(msg) {
        const { from_session_id, sdp } = msg;
        let remoteDesc;
        try {
            remoteDesc = JSON.parse(sdp);
        } catch (err) {
            console.error('Invalid SDP offer:', err);
            return;
        }

        // Clean up any previous connection to this peer
        this.closeRTC(from_session_id);

        const pc = new RTCPeerConnection(this.getRTCConfig());
        this.rtcConnections.set(from_session_id, pc);

        // ICE timeout
        const iceTimer = setTimeout(() => {
            if (pc.connectionState !== 'connected') {
                console.warn('ICE timeout (receiver) — aborting');
                this.closeRTC(from_session_id);
            }
        }, this.ICE_TIMEOUT);

        pc.onicecandidate = (e) => {
            if (e.candidate) {
                this.sendWS({
                    type: 'ice_candidate',
                    session_id: this.sessionId,
                    target_session_id: from_session_id,
                    candidate: JSON.stringify(e.candidate),
                });
            }
        };

        pc.onconnectionstatechange = () => {
            if (pc.connectionState === 'connected') {
                clearTimeout(iceTimer);
            }
            if (pc.connectionState === 'failed' || pc.connectionState === 'disconnected') {
                clearTimeout(iceTimer);
                console.warn('RTC connection failed/disconnected (receiver)');
                this.closeRTC(from_session_id);
            }
        };

        // When the sender's DataChannel arrives — Phase 8: show consent dialog first
        pc.ondatachannel = (event) => {
            const dc = event.channel;
            dc.binaryType = 'arraybuffer';
            // Buffer the first message (the JSON header) to get file metadata,
            // then pause and show the consent dialog before resuming.
            this._handleIncomingDCWithConsent(dc, from_session_id);
        };

        try {
            await pc.setRemoteDescription(new RTCSessionDescription(remoteDesc));
            // Flush any ICE candidates that arrived before the remote description
            this.flushPendingCandidates(from_session_id, pc);

            const answer = await pc.createAnswer();
            await pc.setLocalDescription(answer);
        } catch (err) {
            clearTimeout(iceTimer);
            console.error('Failed to handle offer:', err);
            this.closeRTC(from_session_id);
            return;
        }

        this.sendWS({
            type: 'webrtc_answer',
            session_id: this.sessionId,
            target_session_id: from_session_id,
            sdp: JSON.stringify(pc.localDescription),
        });
    }

    /** Server relays an SDP answer back to us (the host / offer creator). */
    async handleWebRTCAnswer(msg) {
        const { from_session_id, sdp } = msg;
        const pc = this.rtcConnections.get(from_session_id);
        if (!pc) { console.warn('Answer for unknown peer', from_session_id); return; }
        try {
            await pc.setRemoteDescription(new RTCSessionDescription(JSON.parse(sdp)));
            this.flushPendingCandidates(from_session_id, pc);
        } catch (err) {
            console.error('Failed to set remote answer:', err);
            this.closeRTC(from_session_id);
        }
    }

    /** Server relays an ICE candidate. */
    async handleICECandidate(msg) {
        const { from_session_id, candidate } = msg;
        const pc = this.rtcConnections.get(from_session_id);
        let iceCandidate;
        try {
            iceCandidate = new RTCIceCandidate(JSON.parse(candidate));
        } catch (err) {
            console.warn('Invalid ICE candidate:', err);
            return;
        }

        if (pc && pc.remoteDescription) {
            try {
                await pc.addIceCandidate(iceCandidate);
            } catch (err) {
                console.warn('ICE add failed:', err);
            }
        } else {
            // Queue it — the remote description hasn't been set yet
            if (!this.pendingCandidates.has(from_session_id)) {
                this.pendingCandidates.set(from_session_id, []);
            }
            this.pendingCandidates.get(from_session_id).push(iceCandidate);
        }
    }

    async flushPendingCandidates(peerId, pc) {
        const queued = this.pendingCandidates.get(peerId);
        if (!queued || !pc.remoteDescription) return;
        for (const c of queued) {
            try { await pc.addIceCandidate(c); } catch (e) { console.warn('ICE add failed:', e); }
        }
        this.pendingCandidates.delete(peerId);
    }

    /**
     * Phase 8 §8.5: Intercept the first DC message (metadata header),
     * then pause and show a consent dialog before starting the receive.
     * The DataChannel is effectively paused until the user accepts or declines.
     *
     * BUG-02 fix: the sender starts streaming binary chunks immediately
     * after the header (see streamFileOverDC) — it doesn't wait for any
     * acknowledgement. RTCDataChannel does not queue 'message' events for a
     * listener that gets attached later, so every chunk that arrived while
     * the consent dialog (and any async showSaveFilePicker() prompt) was
     * still waiting on the user used to fire into the void and be lost —
     * silently corrupting or hanging every transfer. We now buffer anything
     * that isn't the header here; _installChunkReceiver() replays the
     * buffer, in order, once the real receiver handler is ready.
     */
    _handleIncomingDCWithConsent(dc, fromPeerId) {
        dc._pendingChunks = [];
        let headerHandled = false;

        dc.onmessage = (event) => {
            if (!headerHandled) {
                headerHandled = true;
                let meta;
                try {
                    meta = JSON.parse(event.data);
                } catch {
                    console.error('Invalid file header in consent flow');
                    this.closeRTC(fromPeerId);
                    return;
                }
                // F6: a folder manifest looks nothing like a single-file
                // header — dispatch to its own consent dialog. Folders
                // aren't offer-able via F3 yet, so no pre-accept bypass here.
                if (meta.kind === 'folder-manifest') {
                    this._showFolderConsentDialog(meta, fromPeerId, dc);
                    return;
                }
                // F3: the user already consented at the file-offer stage —
                // don't ask again now that the transfer is actually starting.
                if (this._preAcceptedTransfers.delete(fromPeerId)) {
                    this._beginReceive(dc, meta, fromPeerId);
                    return;
                }
                this._showTransferConsentDialog(meta, fromPeerId, dc);
                return;
            }
            // A binary chunk arriving before the user has responded (or
            // while showSaveFilePicker() is awaiting them) — buffer it.
            dc._pendingChunks.push(event.data);
        };
    }

    /** FSAA when available, Blob-accumulator fallback otherwise. Shared by
     *  the consent dialog's Accept button and the F3 pre-accept bypass. */
    async _beginReceive(dc, meta, fromPeerId) {
        if (window.showSaveFilePicker) {
            await this._setupFSAAReceive(dc, meta, fromPeerId);
        } else {
            this.setupReceiverDC(dc, fromPeerId, meta);
        }
    }

    /**
     * BUG-02 fix: install the real per-chunk handler on a DataChannel that
     * was buffering chunks during the consent dialog. Replays anything that
     * arrived early, in order, through the exact same path as live chunks,
     * then hands off future messages to it too.
     */
    _installChunkReceiver(dc, onChunk) {
        const queued = dc._pendingChunks || [];
        dc._pendingChunks = null;
        dc.onmessage = (event) => {
            if (typeof event.data === 'string') return; // stray text frame
            onChunk(event.data);
        };
        for (const chunk of queued) onChunk(chunk);
    }

    /**
     * F3: someone offered to send us a file directly (no catalog browsing
     * involved). This is a separate, earlier-stage consent step from
     * _showTransferConsentDialog below — that one fires once a DataChannel
     * has actually opened; this one fires before any WebRTC connection
     * exists at all. Accepting marks the sender pre-approved so the later
     * DataChannel-level dialog doesn't ask a second time for the same
     * transfer (see _handleIncomingDCWithConsent).
     */
    _showFileOfferDialog(msg) {
        const file = this.serverFiles.find(f => f.id === msg.file_id);
        const senderName = this.escapeHtml(this._deviceDisplayName(msg.from_session_id, this.peers.get(msg.from_session_id)));
        const fileName = file ? this.escapeHtml(file.name) : 'a file';
        const sizeStr = file ? this.formatFileSize(file.size) : '';

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
            const localFile = this.getFile(msg.file_id);
            if (localFile) {
                this.saveFileToDisk(localFile, localFile.name);
                this.toast(`Saved ${localFile.name}`, 'success');
                return;
            }
            if (this.pendingDownloads.has(msg.file_id)) {
                this.toast('Already downloading that file', 'warning');
                return;
            }
            this.pendingDownloads.add(msg.file_id);
            this._preAcceptedTransfers.add(msg.from_session_id);
            this.sendWS({
                type: 'request_download_from',
                session_id: this.sessionId,
                file_id: msg.file_id,
                host_peer_id: msg.from_session_id,
            });
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
    //  F6: FOLDER RECEIVE
    //  A folder manifest arrives on the same kind of DataChannel a single
    //  file would, just with its own kind:'folder-manifest' header (see
    //  streamFolderOverDC) — _handleIncomingDCWithConsent dispatches here
    //  instead of the single-file consent dialog when it sees one.
    // =====================================================================

    /**
     * Non-blocking consent dialog for an incoming folder.
     * Accept → showDirectoryPicker() (falls back to assembling a zip for
     * browsers without it — same FSAA-primary/fallback-secondary shape as
     * every other receive path here).
     * Decline → sends transfer_declined to sender.
     */
    _showFolderConsentDialog(meta, fromPeerId, dc) {
        const senderName = this.escapeHtml(this._deviceDisplayName(fromPeerId, this.peers.get(fromPeerId)));
        const folderName = this.escapeHtml(meta.folderName);
        const sizeStr = this.formatFileSize(meta.totalBytes);

        document.getElementById('incoming-file-dialog')?.remove();
        const dialog = document.createElement('div');
        dialog.id = 'incoming-file-dialog';
        dialog.className = 'incoming-file-dialog';
        dialog.innerHTML = `
            <div class="ifd-inner">
                <div class="ifd-icon">📁</div>
                <div class="ifd-title">Incoming folder from <strong>${senderName}</strong></div>
                <div class="ifd-name">&ldquo;${folderName}&rdquo;</div>
                <div class="ifd-size">${meta.totalFiles} file${meta.totalFiles > 1 ? 's' : ''} · ${sizeStr}</div>
                <div class="ifd-actions">
                    <button id="ifd-accept" class="btn btn-primary">Accept &amp; choose folder</button>
                    <button id="ifd-decline" class="btn btn-danger">Decline</button>
                </div>
            </div>
        `;
        document.body.appendChild(dialog);

        document.getElementById('ifd-accept').addEventListener('click', async () => {
            dialog.remove();
            if (window.showDirectoryPicker) {
                await this._receiveFolderFSAA(dc, meta, fromPeerId);
            } else {
                await this._receiveFolderToZip(dc, meta, fromPeerId);
            }
        });

        document.getElementById('ifd-decline').addEventListener('click', () => {
            dialog.remove();
            this.sendWS({
                type: 'transfer_declined',
                session_id: this.sessionId,
                file_id: meta.folderId,
            });
            this.closeRTC(fromPeerId);
        });
    }

    /**
     * Resolves (creating as needed) the FileSystemFileHandle for a
     * relativePath inside a picked directory, walking/creating each
     * intermediate directory level — FSAA has no "create nested path"
     * shortcut. Drops empty/"."/".." segments defensively; they're not an
     * escape risk (a FileSystemDirectoryHandle can't resolve outside its
     * own subtree regardless — getDirectoryHandle('..') just looks up a
     * literally-named '..' entry), just not anything a sender should
     * legitimately send.
     */
    async _resolveFolderFileHandle(dirHandle, relativePath) {
        const parts = relativePath.split('/').filter(p => p && p !== '.' && p !== '..');
        let dir = dirHandle;
        for (let i = 0; i < parts.length - 1; i++) {
            dir = await dir.getDirectoryHandle(parts[i], { create: true });
        }
        const leaf = parts[parts.length - 1] || 'unnamed';
        return dir.getFileHandle(leaf, { create: true });
    }

    /** Streams each file in the folder straight to disk via FSAA — bounded
     *  memory regardless of folder size, real directory structure. */
    async _receiveFolderFSAA(dc, meta, fromPeerId) {
        let dirHandle;
        try {
            dirHandle = await window.showDirectoryPicker({ mode: 'readwrite' });
        } catch (err) {
            if (err.name === 'AbortError') {
                this.sendWS({ type: 'transfer_declined', session_id: this.sessionId, file_id: meta.folderId });
                this.closeRTC(fromPeerId);
                return;
            }
            console.warn('showDirectoryPicker failed, falling back to zip:', err);
            await this._receiveFolderToZip(dc, meta, fromPeerId);
            return;
        }

        const transferId = `dl:${meta.folderId}`;
        this._transferPeers.set(transferId, fromPeerId);
        this.showProgress(transferId, `Downloading ${meta.folderName}`, 0, null, null, 0, meta.totalBytes);

        let totalReceived = 0;
        let filesCompleted = 0;
        let transferComplete = false;
        let currentFile = null; // { relativePath, size, received, writable }
        const startTime = Date.now();
        let chain = Promise.resolve();

        const finishCurrentFile = async () => {
            if (currentFile?.writable) {
                try { await currentFile.writable.close(); } catch (_) { /* best-effort */ }
            }
            currentFile = null;
        };

        const onMessage = (event) => {
            chain = chain.then(async () => {
                if (transferComplete) return;

                if (typeof event.data === 'string') {
                    let msg;
                    try { msg = JSON.parse(event.data); } catch { return; }

                    if (msg.kind === 'file-header') {
                        await finishCurrentFile();
                        try {
                            const fileHandle = await this._resolveFolderFileHandle(dirHandle, msg.relativePath);
                            const writable = await fileHandle.createWritable();
                            currentFile = { relativePath: msg.relativePath, size: msg.size, received: 0, writable };
                        } catch (err) {
                            console.error(`Could not open ${msg.relativePath} for writing:`, err);
                            currentFile = { relativePath: msg.relativePath, size: msg.size, received: 0, writable: null };
                        }
                    } else if (msg.kind === 'folder-done') {
                        await finishCurrentFile();
                        transferComplete = true;
                        this.hideProgress(transferId);
                        this.pendingDownloads.delete(meta.folderId);
                        this.toast(`Downloaded folder ${meta.folderName} (${filesCompleted} file${filesCompleted === 1 ? '' : 's'})`, 'success', 5000);
                        this.sendWS({ type: 'file_downloaded', session_id: this.sessionId, file_id: meta.folderId });
                        setTimeout(() => this.closeRTC(fromPeerId), 2000);
                    }
                    return;
                }

                // Binary chunk for whichever file is currently open
                if (!currentFile) return; // out-of-protocol — ignore defensively
                if (currentFile.writable) {
                    try {
                        await currentFile.writable.write(event.data);
                    } catch (err) {
                        console.error(`Write error for ${currentFile.relativePath}:`, err);
                    }
                }
                currentFile.received += event.data.byteLength;
                totalReceived += event.data.byteLength;
                if (currentFile.received >= currentFile.size) filesCompleted++;

                const pct = Math.round((totalReceived / meta.totalBytes) * 100);
                const elapsed = (Date.now() - startTime) / 1000;
                const speed = elapsed > 0 ? totalReceived / elapsed : 0;
                const remaining = speed > 0 ? (meta.totalBytes - totalReceived) / speed : 0;
                this.showProgress(transferId, `Downloading ${meta.folderName} (${currentFile.relativePath})`, pct, speed, remaining, totalReceived, meta.totalBytes);
            });
        };

        dc.onmessage = onMessage;
        // Replay whatever arrived during the consent dialog / directory
        // picker prompt — see _handleIncomingDCWithConsent's buffering.
        const queued = dc._pendingChunks || [];
        dc._pendingChunks = null;
        for (const item of queued) onMessage({ data: item });

        dc.onclose = () => {
            if (!transferComplete) {
                this.hideProgress(transferId);
                this.pendingDownloads.delete(meta.folderId);
                const wasCancelled = this.cancelledTransfers.has(`cancelled:${meta.folderId}`);
                if (wasCancelled) {
                    this.cancelledTransfers.delete(`cancelled:${meta.folderId}`);
                } else {
                    // F6 v1: folder transfers aren't resumable yet — unlike
                    // single files (F8), so this is a fresh restart, not a
                    // continuation. A worthwhile follow-up, not in scope here.
                    this.toast(`Folder transfer interrupted: ${meta.folderName}`, 'warning');
                }
            }
        };

        dc.onerror = (err) => {
            console.error('DC error (folder receive):', err);
            if (!transferComplete) {
                this.hideProgress(transferId);
                this.pendingDownloads.delete(meta.folderId);
                this.toast(`Folder transfer error: ${meta.folderName}`, 'error');
            }
        };
    }

    /** Fallback for browsers without showDirectoryPicker: assembles the
     *  incoming files into a zip (one at a time — peak memory is bounded by
     *  the largest single file, not the whole folder) and saves that,
     *  reusing the same _zipToDisk/_zipToMemory helpers folder *uploads*
     *  used before F6 moved uploads to real streaming. */
    async _receiveFolderToZip(dc, meta, fromPeerId) {
        if (typeof JSZip === 'undefined') {
            this.toast('JSZip library not loaded — cannot receive this folder.', 'error');
            this.sendWS({ type: 'transfer_declined', session_id: this.sessionId, file_id: meta.folderId });
            this.closeRTC(fromPeerId);
            return;
        }

        const zip = new JSZip();
        const transferId = `dl:${meta.folderId}`;
        this._transferPeers.set(transferId, fromPeerId);
        this.showProgress(transferId, `Downloading ${meta.folderName}`, 0, null, null, 0, meta.totalBytes);

        let totalReceived = 0;
        let filesCompleted = 0;
        let transferComplete = false;
        let currentFile = null; // { relativePath, size, received, chunks }
        const startTime = Date.now();

        const finishCurrentFile = () => {
            if (currentFile) {
                zip.file(currentFile.relativePath, new Blob(currentFile.chunks));
            }
            currentFile = null;
        };

        const onMessage = (event) => {
            if (transferComplete) return;

            if (typeof event.data === 'string') {
                let msg;
                try { msg = JSON.parse(event.data); } catch { return; }

                if (msg.kind === 'file-header') {
                    finishCurrentFile();
                    currentFile = { relativePath: msg.relativePath, size: msg.size, received: 0, chunks: [] };
                } else if (msg.kind === 'folder-done') {
                    finishCurrentFile();
                    transferComplete = true;
                    this._finalizeFolderZip(zip, meta, fromPeerId, filesCompleted);
                }
                return;
            }

            if (!currentFile) return;
            currentFile.chunks.push(event.data);
            currentFile.received += event.data.byteLength;
            totalReceived += event.data.byteLength;
            if (currentFile.received >= currentFile.size) filesCompleted++;

            const pct = Math.round((totalReceived / meta.totalBytes) * 100);
            const elapsed = (Date.now() - startTime) / 1000;
            const speed = elapsed > 0 ? totalReceived / elapsed : 0;
            const remaining = speed > 0 ? (meta.totalBytes - totalReceived) / speed : 0;
            this.showProgress(transferId, `Downloading ${meta.folderName} (${currentFile.relativePath})`, pct, speed, remaining, totalReceived, meta.totalBytes);
        };

        dc.onmessage = onMessage;
        const queued = dc._pendingChunks || [];
        dc._pendingChunks = null;
        for (const item of queued) onMessage({ data: item });

        dc.onclose = () => {
            if (!transferComplete) {
                this.hideProgress(transferId);
                this.pendingDownloads.delete(meta.folderId);
                const wasCancelled = this.cancelledTransfers.has(`cancelled:${meta.folderId}`);
                if (wasCancelled) {
                    this.cancelledTransfers.delete(`cancelled:${meta.folderId}`);
                } else {
                    this.toast(`Folder transfer interrupted: ${meta.folderName}`, 'warning');
                }
            }
        };

        dc.onerror = (err) => {
            console.error('DC error (folder receive, zip fallback):', err);
            if (!transferComplete) {
                this.hideProgress(transferId);
                this.pendingDownloads.delete(meta.folderId);
                this.toast(`Folder transfer error: ${meta.folderName}`, 'error');
            }
        };
    }

    async _finalizeFolderZip(zip, meta, fromPeerId, filesCompleted) {
        const transferId = `dl:${meta.folderId}`;
        const zipName = `${meta.folderName}.zip`;
        try {
            const file = window.showSaveFilePicker
                ? await this._zipToDisk(zip, zipName)
                : await this._zipToMemory(zip, zipName);
            this.hideProgress(transferId);
            this.pendingDownloads.delete(meta.folderId);
            if (file) {
                // _zipToDisk already saved it via the picker; _zipToMemory
                // just builds the File object in memory, so trigger the
                // actual download ourselves in that case.
                if (!window.showSaveFilePicker) this.saveFileToDisk(file, zipName);
                this.toast(`Downloaded folder ${meta.folderName} as ${zipName} (${filesCompleted} file${filesCompleted === 1 ? '' : 's'})`, 'success', 5000);
            }
        } catch (err) {
            console.error('Failed to finalize folder zip:', err);
            this.hideProgress(transferId);
            this.pendingDownloads.delete(meta.folderId);
            this.toast(`Could not save folder ${meta.folderName}: ${err.message}`, 'error');
        }
        this.sendWS({ type: 'file_downloaded', session_id: this.sessionId, file_id: meta.folderId });
        setTimeout(() => this.closeRTC(fromPeerId), 2000);
    }

    /**
     * Phase 8 §8.5: Non-blocking consent dialog.
     * Accept → opens showSaveFilePicker (or falls back to Blob path).
     * Decline → sends transfer_declined to sender.
     */
    _showTransferConsentDialog(meta, fromPeerId, dc) {
        // BUG-05 fix: fromPeerId is a peer-chosen session_id, not trustworthy.
        const senderShort = this.escapeHtml(fromPeerId.slice(-6));
        const sizeStr = this.formatFileSize(meta.fileSize);

        // Remove any existing dialog
        document.getElementById('incoming-file-dialog')?.remove();

        const dialog = document.createElement('div');
        dialog.id = 'incoming-file-dialog';
        dialog.className = 'incoming-file-dialog';
        dialog.innerHTML = `
            <div class="ifd-inner">
                <div class="ifd-icon">📥</div>
                <div class="ifd-title">Incoming file from <strong>${senderShort}</strong></div>
                <div class="ifd-name">&ldquo;${this.escapeHtml(meta.fileName)}&rdquo;</div>
                <div class="ifd-size">${sizeStr}</div>
                <div class="ifd-actions">
                    <button id="ifd-accept" class="btn btn-primary">Accept &amp; choose save location</button>
                    <button id="ifd-decline" class="btn btn-danger">Decline</button>
                </div>
            </div>
        `;
        document.body.appendChild(dialog);

        document.getElementById('ifd-accept').addEventListener('click', async () => {
            dialog.remove();
            await this._beginReceive(dc, meta, fromPeerId);
        });

        document.getElementById('ifd-decline').addEventListener('click', () => {
            dialog.remove();
            // Notify sender of decline
            this.sendWS({
                type: 'transfer_declined',
                session_id: this.sessionId,
                file_id: meta.fileId,
            });
            this.closeRTC(fromPeerId);
        });
    }

    /**
     * F7: read back the part of a file we already wrote (on a resumed
     * download) and fold it into a hasher, so the running hash stays
     * correct across a resume without ever holding the whole prefix in
     * memory at once.
     */
    async _hashExistingPrefix(hasher, fileHandle, bytesToHash) {
        const existing = await fileHandle.getFile();
        for (let offset = 0; offset < bytesToHash; offset += this.RTC_CHUNK_SIZE) {
            const end = Math.min(offset + this.RTC_CHUNK_SIZE, bytesToHash);
            const buf = await existing.slice(offset, end).arrayBuffer();
            hasher.update(new Uint8Array(buf));
        }
    }

    /** F7: a failed integrity check gets a toast with a one-click retry. */
    _offerRedownload(fileId, fileName) {
        this.toast(`Integrity check failed for "${fileName}" — it may be corrupted.`, 'error', 10000, {
            label: 'Re-download',
            onClick: () => {
                this._resumeState.delete(fileId); // clean restart, not a resume
                this.pendingDownloads.delete(fileId);
                this.downloadFile(fileId);
            },
        });
    }

    /**
     * Phase 8 §8.2: FSAA streaming receive.
     * Chunks are written sequentially to a FileSystemWritableFileStream.
     * O(1) memory: only one chunk is live at any time.
     * The sequential `await writable.write()` provides natural receive-side backpressure.
     *
     * F8: resumes from meta.resumeFromBytes when we have a cached
     * fileHandle from an earlier, interrupted attempt at this same fileId
     * (see _resumeState) — reopens the SAME file with keepExistingData
     * instead of prompting showSaveFilePicker() again, seeks past what's
     * already on disk, and continues from there.
     *
     * F7: verifies the sender's sha256 (if it sent one) by hashing
     * incrementally as chunks arrive — never re-reads the whole file to do
     * it, so this scales to files far larger than available RAM. On a
     * resume, the already-written prefix is read back once (bounded
     * memory, see _hashExistingPrefix) to catch the hasher up first.
     */
    async _setupFSAAReceive(dc, meta, fromPeerId) {
        const cached = this._resumeState.get(meta.fileId);
        const isResume = !!(cached?.fileHandle && meta.resumeFromBytes > 0);

        let fileHandle;
        if (isResume) {
            fileHandle = cached.fileHandle;
        } else {
            try {
                fileHandle = await window.showSaveFilePicker({
                    suggestedName: meta.fileName,
                    types: [{ description: 'File', accept: { [meta.mimeType || 'application/octet-stream']: [] } }],
                });
            } catch (err) {
                if (err.name === 'AbortError') {
                    // User dismissed the picker
                    this.sendWS({ type: 'transfer_declined', session_id: this.sessionId, file_id: meta.fileId });
                    this.closeRTC(fromPeerId);
                    return;
                }
                // Unexpected error — fall back to Blob path
                console.warn('showSaveFilePicker failed, falling back:', err);
                this.setupReceiverDC(dc, fromPeerId, meta);
                return;
            }
        }

        let writable;
        try {
            writable = await fileHandle.createWritable({ keepExistingData: isResume });
            if (isResume) {
                await writable.write({ type: 'seek', position: meta.resumeFromBytes });
            }
        } catch (err) {
            console.error('createWritable failed:', err);
            this.toast('Could not open file for writing.', 'error');
            this._resumeState.delete(meta.fileId);
            this.closeRTC(fromPeerId);
            return;
        }

        let hasher = null;
        if (meta.sha256 && typeof hashwasm !== 'undefined') {
            try {
                hasher = await hashwasm.createSHA256();
                if (isResume) {
                    await this._hashExistingPrefix(hasher, fileHandle, meta.resumeFromBytes);
                }
            } catch (err) {
                console.warn('Could not start integrity hash:', err);
                hasher = null;
            }
        }

        let receivedBytes = isResume ? meta.resumeFromBytes : 0;
        const startTime = Date.now();
        let transferComplete = false;
        // BUG-12 fix: keyed by fileId (not fileId+peer) — a retry can pick
        // a different host peer for the same logical download, and the
        // user's progress card should follow the download, not the peer.
        const transferId = `dl:${meta.fileId}`;
        this._transferPeers.set(transferId, fromPeerId);
        this._resumeState.set(meta.fileId, { fileHandle, bytesReceived: receivedBytes });
        this.showProgress(transferId, `Downloading ${meta.fileName}`, Math.round((receivedBytes / meta.fileSize) * 100), null, null, receivedBytes, meta.fileSize);
        console.log(`[FSAA] Receiving ${meta.fileName} (${this.formatFileSize(meta.fileSize)})${isResume ? ` — resuming from ${this.formatFileSize(receivedBytes)}` : ''}`);

        // Use an async queue so we never process two chunks concurrently
        // and the DC’s JS buffer can’t grow unboundedly.
        let writeChain = Promise.resolve();

        const onChunk = (chunk) => {
            // Chain writes — each write waits for the previous one
            writeChain = writeChain.then(async () => {
                if (transferComplete) return;
                try {
                    await writable.write(chunk);  // sequential, backpressure-safe
                } catch (err) {
                    console.error('Write error:', err);
                    transferComplete = true;
                    try { await writable.close(); } catch (_) {}
                    this.hideProgress(transferId);
                    this.toast(`Write error while receiving ${meta.fileName}`, 'error');
                    this.closeRTC(fromPeerId);
                    return;
                }
                if (hasher) hasher.update(new Uint8Array(chunk));
                receivedBytes += chunk.byteLength;
                const resumeEntry = this._resumeState.get(meta.fileId);
                if (resumeEntry) resumeEntry.bytesReceived = receivedBytes;
                const pct = Math.round((receivedBytes / meta.fileSize) * 100);
                const elapsed = (Date.now() - startTime) / 1000;
                const speed = elapsed > 0 ? (receivedBytes - (isResume ? meta.resumeFromBytes : 0)) / elapsed : 0;
                const remaining = speed > 0 ? (meta.fileSize - receivedBytes) / speed : 0;
                this.showProgress(transferId, `Downloading ${meta.fileName}`, pct, speed, remaining, receivedBytes, meta.fileSize);

                if (receivedBytes >= meta.fileSize) {
                    transferComplete = true;
                    try {
                        await writable.close();
                    } catch (err) {
                        console.warn('writable.close() error:', err);
                    }
                    this.hideProgress(transferId);
                    this.pendingDownloads.delete(meta.fileId);
                    this.activeTransfers.delete(`retry:${meta.fileId}`);
                    this._resumeState.delete(meta.fileId);
                    console.log(`[FSAA] Download complete: ${meta.fileName}`);

                    if (hasher) {
                        const computed = hasher.digest('hex');
                        if (computed === meta.sha256) {
                            this._integrityStatus.set(meta.fileId, 'verified');
                            this.toast(`Downloaded ${meta.fileName} (${this.formatFileSize(meta.fileSize)}) — ✓ verified`, 'success', 5000);
                        } else {
                            console.error(`Integrity check FAILED for ${meta.fileName}: expected ${meta.sha256}, got ${computed}`);
                            this._integrityStatus.set(meta.fileId, 'failed');
                            this._offerRedownload(meta.fileId, meta.fileName);
                        }
                        this.updateFileList(this.serverFiles);
                    } else {
                        this.toast(`Downloaded ${meta.fileName} (${this.formatFileSize(meta.fileSize)})`, 'success', 5000);
                    }
                    // Notify server we now also host this file
                    this.sendWS({ type: 'file_downloaded', session_id: this.sessionId, file_id: meta.fileId });
                    setTimeout(() => this.closeRTC(fromPeerId), 2000);
                }
            });
        };

        // BUG-02 fix: replay any chunks buffered during the consent dialog /
        // showSaveFilePicker() prompt, then hand off future ones the same way.
        this._installChunkReceiver(dc, onChunk);

        dc.onclose = () => {
            if (!transferComplete && meta && receivedBytes < meta.fileSize) {
                writeChain.then(async () => {
                    try { await writable.close(); } catch (_) {}
                });
                this.hideProgress(transferId);
                const wasCancelled = this.cancelledTransfers.has(`cancelled:${meta.fileId}`);
                if (!wasCancelled) {
                    this.toast(`Transfer interrupted: ${meta.fileName} — resuming…`, 'warning');
                    this._retryDownload(meta.fileId);
                } else {
                    this.cancelledTransfers.delete(`cancelled:${meta.fileId}`);
                    this._resumeState.delete(meta.fileId);
                }
            }
        };

        dc.onerror = (err) => {
            console.error('DC error (FSAA receive):', err);
            if (!transferComplete) {
                writeChain.then(async () => {
                    try { await writable.close(); } catch (_) {}
                });
                this.hideProgress(transferId);
                if (meta && !this.cancelledTransfers.has(`cancelled:${meta.fileId}`)) {
                    this.toast(`Transfer error: ${meta.fileName} — resuming…`, 'warning');
                    this._retryDownload(meta.fileId);
                }
            }
        };
    }

    /**
     * Set up a DataChannel on the receiver side (legacy Blob path).
     * Used when showSaveFilePicker is not available (Firefox/Safari).
     *
     * BUG-02 fix: `meta` is now passed in from the consent dialog instead
     * of being re-parsed from "the first message" — that header was already
     * consumed by _handleIncomingDCWithConsent before this function is ever
     * called, so waiting for it again here meant this fallback path was
     * broken independently of the chunk-buffering bug (it would try to
     * JSON.parse the first binary chunk and fail).
     */
    /**
     * F8: resumes in-memory from a previous attempt's buffered chunks (see
     * _resumeState) instead of starting over — this path has no on-disk
     * handle to reopen the way FSAA does, so "resuming" here just means not
     * throwing away what's already been buffered in RAM.
     *
     * F7: verifies the sender's sha256 the same way as _setupFSAAReceive,
     * hashing each chunk as it arrives (plus any already-buffered ones from
     * a resume) rather than re-hashing the whole Blob at the end.
     */
    async setupReceiverDC(dc, fromPeerId, meta) {
        const cached = this._resumeState.get(meta.fileId);
        const isResume = !!(cached?.chunks && meta.resumeFromBytes > 0);

        let chunks = isResume ? cached.chunks : [];
        let receivedBytes = isResume ? cached.bytesReceived : 0;
        let startTime = Date.now();
        let transferComplete = false;  // Flag to prevent onclose from firing after success
        // BUG-12 fix: see the matching comment in _setupFSAAReceive.
        const transferId = `dl:${meta.fileId}`;
        this._transferPeers.set(transferId, fromPeerId);
        this._resumeState.set(meta.fileId, { chunks, bytesReceived: receivedBytes });

        let hasher = null;
        if (meta.sha256 && typeof hashwasm !== 'undefined') {
            try {
                hasher = await hashwasm.createSHA256();
                for (const c of chunks) hasher.update(new Uint8Array(c)); // catch up on a resume
            } catch (err) {
                console.warn('Could not start integrity hash:', err);
                hasher = null;
            }
        }

        console.log(`Receiving ${meta.fileName} (${this.formatFileSize(meta.fileSize)}) from ${fromPeerId.slice(-6)}${isResume ? ` — resuming from ${this.formatFileSize(receivedBytes)}` : ''}`);
        this.showProgress(transferId, `Downloading ${meta.fileName}`, Math.round((receivedBytes / meta.fileSize) * 100), null, null, receivedBytes, meta.fileSize);

        const onChunk = (chunk) => {
            chunks.push(chunk);
            if (hasher) hasher.update(new Uint8Array(chunk));
            receivedBytes += chunk.byteLength;
            const resumeEntry = this._resumeState.get(meta.fileId);
            if (resumeEntry) resumeEntry.bytesReceived = receivedBytes;

            const pct = Math.round((receivedBytes / meta.fileSize) * 100);
            const elapsed = (Date.now() - startTime) / 1000;
            const speed = elapsed > 0 ? (receivedBytes - (isResume ? meta.resumeFromBytes : 0)) / elapsed : 0;
            const remaining = speed > 0 ? (meta.fileSize - receivedBytes) / speed : 0;
            this.showProgress(transferId, `Downloading ${meta.fileName}`, pct, speed, remaining, receivedBytes, meta.fileSize);

            // All chunks received?
            if (receivedBytes >= meta.fileSize) {
                transferComplete = true;  // Mark as complete before finalize
                this._resumeState.delete(meta.fileId);

                if (hasher) {
                    const computed = hasher.digest('hex');
                    if (computed === meta.sha256) {
                        this._integrityStatus.set(meta.fileId, 'verified');
                    } else {
                        console.error(`Integrity check FAILED for ${meta.fileName}: expected ${meta.sha256}, got ${computed}`);
                        this._integrityStatus.set(meta.fileId, 'failed');
                        this.hideProgress(transferId);
                        this.pendingDownloads.delete(meta.fileId);
                        this._offerRedownload(meta.fileId, meta.fileName);
                        this.updateFileList(this.serverFiles);
                        // Still finalize — a corrupted copy the user can see
                        // and retry beats silently discarding their transfer.
                        this.finalizeReceivedFile(meta, chunks, fromPeerId, /* verified */ false);
                        return;
                    }
                }
                this.finalizeReceivedFile(meta, chunks, fromPeerId, hasher ? true : null);
            }
        };

        // BUG-02 fix: replay any chunks buffered during the consent dialog,
        // then hand off future ones the same way.
        this._installChunkReceiver(dc, onChunk);

        dc.onclose = () => {
            console.log('DataChannel closed (receiver side)');
            // If we received all data before close, finalizeReceivedFile
            // already handled it.  If not, the transfer was interrupted.
            // BUT: don't retry if user manually cancelled
            const wasCancelled = this.cancelledTransfers.has(`cancelled:${meta.fileId}`);
            if (wasCancelled) {
                this.cancelledTransfers.delete(`cancelled:${meta.fileId}`);
                this._resumeState.delete(meta.fileId);
                return; // User cancelled, don't retry
            }
            if (!transferComplete && receivedBytes < meta.fileSize) {
                this.hideProgress(transferId);
                this.toast(`Transfer interrupted: ${meta.fileName} — resuming…`, 'warning');
                this._retryDownload(meta.fileId);
            }
        };

        dc.onerror = (err) => {
            console.error('DataChannel error (receiver):', err);
            const wasCancelled = this.cancelledTransfers.has(`cancelled:${meta.fileId}`);
            if (wasCancelled) {
                this.cancelledTransfers.delete(`cancelled:${meta.fileId}`);
                this._resumeState.delete(meta.fileId);
                return; // User cancelled, don't retry
            }
            if (!transferComplete) {
                this.hideProgress(transferId);
                this.toast(`Transfer error: ${meta.fileName} — resuming…`, 'warning');
                this._retryDownload(meta.fileId);
            }
        };
    }

    /** Assemble received chunks into a File, trigger browser download, become a host. */
    finalizeReceivedFile(meta, chunks, fromPeerId, verified = null) {
        const blob = new Blob(chunks, { type: meta.mimeType });
        const file = new File([blob], meta.fileName, { type: meta.mimeType });

        // Store locally so WE become a host too
        this.storeFile(meta.fileId, file);

        // Trigger browser "Save As"
        this.saveFileToDisk(file, meta.fileName);

        this.hideProgress(`dl:${meta.fileId}`);
        this.pendingDownloads.delete(meta.fileId);
        this.activeTransfers.delete(`retry:${meta.fileId}`);
        const sizeStr = this.formatFileSize(meta.fileSize);
        if (verified === false) {
            this.toast(`Downloaded ${meta.fileName} (${sizeStr}) — ⚠ integrity check failed`, 'warning', 6000);
        } else {
            const suffix = verified === true ? ' — ✓ verified' : '';
            this.toast(`Downloaded ${meta.fileName} (${sizeStr})${suffix}`, 'success', 5000);
        }
        console.log(`Download complete: ${meta.fileName}`);

        // Notify server we now also host this file
        this.sendWS({ type: 'file_downloaded', session_id: this.sessionId, file_id: meta.fileId });

        // Clean up RTC connection
        setTimeout(() => this.closeRTC(fromPeerId), 2000);
    }

    saveFileToDisk(fileOrBlob, fileName) {
        const url = URL.createObjectURL(fileOrBlob);
        const a = document.createElement('a');
        a.href = url;
        a.download = fileName;
        a.style.display = 'none';
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
        setTimeout(() => URL.revokeObjectURL(url), 10000);
    }

    // ── RTC cleanup ─────────────────────────────────────────────────────

    closeRTC(peerId) {
        const pc = this.rtcConnections.get(peerId);
        if (pc) {
            try { pc.close(); } catch (_) { /* already closed */ }
            this.rtcConnections.delete(peerId);
        }
        this.pendingCandidates.delete(peerId);
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

        if (files && files.length > 0) {
            files.forEach(f => allItems.push({ type: 'file', data: f, timestamp: new Date(f.uploaded_at) }));
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
                const hosts = Array.isArray(f.hosts) ? f.hosts : Array.from(f.hosts || []);
                const isDownloading = this.pendingDownloads.has(f.id);
                const isMine = f.uploader_id === this.sessionId;
                // F7: ✓/✗ badge for files this tab downloaded and checked
                // against the sender's sha256 (only set for our own downloads;
                // F6 folders aren't hashed as a whole — see streamFolderOverDC)
                const integrity = this._integrityStatus.get(f.id);
                const integrityBadge = integrity === 'verified'
                    ? '<span class="integrity-badge integrity-ok" title="Integrity verified">✓</span>'
                    : integrity === 'failed'
                        ? '<span class="integrity-badge integrity-bad" title="Integrity check failed">✗</span>'
                        : '';
                const icon = f.is_folder ? '📁' : '📄';
                const typeLabel = f.is_folder ? 'Folder' : f.mime_type;
                return `
                    <tr class="file-row" draggable="true" data-file-id="${this.escapeHtml(f.id)}">
                        <td class="file-name">${icon} ${this.escapeHtml(f.name)}${integrityBadge}</td>
                        <td class="file-type">${this.escapeHtml(typeLabel)}</td>
                        <td class="file-size">${this.formatSize(f.size)}</td>
                        <td>
                            <div class="file-hosts">
                                ${hosts.map(h => h === this.sessionId
                                    ? '<span class="host-badge host-self">You</span>'
                                    : `<span class="host-badge">${this.escapeHtml(this._deviceDisplayName(h, this.peers.get(h)))}</span>`
                                ).join('')}
                            </div>
                        </td>
                        <td class="file-actions">
                            ${hosts.length > 0
                                ? `<button class="btn download${isDownloading ? ' downloading' : ''}" data-action="download-file" data-file-id="${this.escapeHtml(f.id)}" ${isDownloading ? 'disabled' : ''}>${isDownloading ? '⏳ Downloading…' : '⬇️ Download'}</button>`
                                : '<span style="color:#a0aec0;">No hosts</span>'}
                            ${isMine ? `<button class="btn secondary delete-file-btn" data-action="delete-file" data-file-id="${this.escapeHtml(f.id)}" title="Unshare">🗑️</button>` : ''}
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
        this._transferPeers.delete(transferId);
        const panel = document.getElementById('progress-panel');
        if (panel && !this._progressCards.size) panel.classList.remove('visible');
    }

    /** Cancel one transfer by id — used by each card's own Cancel button. */
    cancelTransfer(transferId) {
        this.cancelledTransfers.add(transferId);
        if (transferId.startsWith('dl:')) {
            const fileId = transferId.slice('dl:'.length);
            this.cancelledTransfers.add(`cancelled:${fileId}`);
            this.pendingDownloads.delete(fileId);
        }
        this.activeTransfers.delete(transferId);
        const peerId = this._transferPeers.get(transferId);
        if (peerId) this.closeRTC(peerId);
        this.hideProgress(transferId);
        this.toast('Transfer cancelled', 'info');
    }

    /** Cancel every in-flight transfer — bound to the panel's "Cancel all" button. */
    cancelActiveTransfer() {
        const ids = new Set([
            ...this.activeTransfers.keys(),
            ...Array.from(this.pendingDownloads, fileId => `dl:${fileId}`),
        ]);
        for (const id of ids) this.cancelTransfer(id);
        if (ids.size === 0) this.toast('No transfers in progress', 'info');
    }

    /**
     * Phase 8 §8.3: Show a persistent warning banner for browsers lacking FSAA.
     * Keeps the legacy Blob path working but lets users know about the limitation.
     */
    _showFsaaBanner() {
        if (document.getElementById('fsaa-warning-banner')) return;
        const banner = document.createElement('div');
        banner.id = 'fsaa-warning-banner';
        banner.className = 'system-banner system-banner-warning';
        banner.innerHTML = `
            <span>⚠️ Your browser does not support streaming downloads.
            Files larger than ~2 GB may cause this tab to crash.
            Use Chrome or Edge for large-file transfers.</span>
            <button class="banner-dismiss" onclick="this.parentElement.remove()">✕</button>
        `;
        document.body.prepend(banner);
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

window.addEventListener('beforeunload', () => {
    if (window.app) {
        // Close all RTC connections
        for (const [pid] of window.app.rtcConnections) window.app.closeRTC(pid);
        if (window.app.ws) window.app.ws.close();
    }
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