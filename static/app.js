// ============================================================================
// LADEX — Local Area Data Exchange
// Pure P2P file transfer over WebRTC DataChannels.
// The server is only a signaling relay + file catalog.  Zero bytes of file
// data ever touch the server.
// ============================================================================

class LADEXApp {
    constructor() {
        this.ws = null;
        this.sessionId = this.generateSessionId();

        // File *references* (File objects) stored by fileId.
        // A File object is a handle to the on-disk blob — costs ~0 RAM
        // regardless of file size.  We only read slices on demand.
        this.files = new Map();

        this.peers = new Map();
        this.messages = [];
        this.serverFiles = [];
        // Phase 6: RTT-aware peer map (same data as this.peers but refreshed via peer_sync)
        // node_rtt_ms is stored on each PeerInfo when received from the server.

        // ── WebRTC state ────────────────────────────────────────────────
        // Active RTCPeerConnections keyed by remote sessionId.
        this.rtcConnections = new Map();
        // ICE candidates that arrived before the remote description was set.
        this.pendingCandidates = new Map();
        // Track ongoing sends/receives for progress UI.
        this.activeTransfers = new Map();

        this.RTC_CHUNK_SIZE = 64 * 1024; // 64 KB per DataChannel message

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
    toast(message, type = 'info', durationMs = 4000) {
        this._ensureToastContainer();
        const el = document.createElement('div');
        el.className = `toast toast-${type}`;
        const icons = { info: 'ℹ️', success: '✅', error: '❌', warning: '⚠️' };
        el.innerHTML = `<span class="toast-icon">${icons[type] || ''}</span><span class="toast-msg">${this.escapeHtml(message)}</span>`;
        this._toastContainer.appendChild(el);
        // Trigger CSS enter animation
        requestAnimationFrame(() => el.classList.add('toast-visible'));
        setTimeout(() => {
            el.classList.remove('toast-visible');
            el.classList.add('toast-exit');
            el.addEventListener('transitionend', () => el.remove());
            // Fallback if transitionend doesn't fire
            setTimeout(() => el.remove(), 500);
        }, durationMs);
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
            this.sendWS({ type: 'join', session_id: this.sessionId, user_agent: navigator.userAgent });
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
                break;
            case 'peer_left':
                this.peers.delete(msg.session_id);
                this.updatePeerStatus(msg.total_peers);
                // Clean up any RTC connection to that peer
                this.closeRTC(msg.session_id);
                break;
            // Phase 6: incremental peer list update (RTT changes)
            case 'peer_sync':
                if (msg.peers) {
                    for (const peer of msg.peers) {
                        if (peer.hosting_node_id == null) {
                            // Departure tombstone
                            this.peers.delete(peer.session_id);
                        } else {
                            // Merge: update RTT without overwriting other fields
                            const existing = this.peers.get(peer.session_id) || {};
                            this.peers.set(peer.session_id, { ...existing, ...peer });
                        }
                    }
                }
                break;

            // ── file catalog ────────────────────────────────────────────
            case 'file_list_update':
                this.serverFiles = msg.files || [];
                this.updateFileList(this.serverFiles);
                break;

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
                this.hideProgress();
                break;

            // Phase 5: remote receiver declined the file
            case 'transfer_declined':
                this.toast(`Receiver declined the file transfer`, 'warning', 5000);
                this.hideProgress();
                break;

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
            }
            const count = isFolder ? 1 : files.length;
            this.toast(`${count} file${count > 1 ? 's' : ''} shared`, 'success');
        } catch (err) {
            this.toast(`Upload failed: ${err.message}`, 'error');
        }
    }

    async handleFolderUpload(files) {
        if (typeof JSZip === 'undefined') {
            this.toast('JSZip library not loaded.', 'error');
            return;
        }
        const zip = new JSZip();
        for (const f of files) zip.file(f.webkitRelativePath || f.name, f);
        const folderName = files[0].webkitRelativePath?.split('/')[0] || 'folder';
        const blob = await zip.generateAsync({ type: 'blob' });
        await this.uploadFile(new File([blob], `${folderName}.zip`, { type: 'application/zip' }));
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
    }

    // =====================================================================
    //  DRAG-AND-DROP
    // =====================================================================

    setupDragAndDrop() {
        const body = document.body;
        let dragDepth = 0;

        body.addEventListener('dragenter', (e) => {
            e.preventDefault();
            dragDepth++;
            body.classList.add('drag-over');
        });

        body.addEventListener('dragleave', (e) => {
            e.preventDefault();
            dragDepth--;
            if (dragDepth <= 0) {
                dragDepth = 0;
                body.classList.remove('drag-over');
            }
        });

        body.addEventListener('dragover', (e) => {
            e.preventDefault();
            e.dataTransfer.dropEffect = 'copy';
        });

        body.addEventListener('drop', (e) => {
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
            this.toast(`Download failed after ${this.MAX_RETRIES} attempts`, 'error', 6000);
            this.hideProgress();
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

        console.log(`Requesting ${fileId} from host ${chosenHost.slice(-6)} (attempt ${attempt + 1}, Phase 6 client selection)`);
        this.activeTransfers.set(`retry:${fileId}`, { attempt, fileId });
        this.sendWS({
            type: 'request_download_from',
            session_id: this.sessionId,
            file_id: fileId,
            host_peer_id: chosenHost,
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
     */
    _retryDownload(fileId) {
        const retryInfo = this.activeTransfers.get(`retry:${fileId}`);
        const attempt = retryInfo ? retryInfo.attempt + 1 : 1;
        this.activeTransfers.delete(`retry:${fileId}`);

        const delay = this.RETRY_BASE_DELAY * Math.pow(2, attempt - 1);
        setTimeout(() => this._requestDownloadWithRetry(fileId, attempt), delay);
    }

    /**
     * Server tells us: "you are a host — send file X to requester Y".
     * We (the host) initiate a WebRTC connection and stream the file.
     */
    handleDownloadRequest(msg) {
        const { file_id, requester_session_id } = msg;
        const file = this.getFile(file_id);
        if (!file) {
            console.error('Download request for file we don\'t have:', file_id);
            return;
        }
        console.log(`Initiating WebRTC transfer of ${file.name} → ${requester_session_id.slice(-6)}`);
        this.initiateWebRTCSend(requester_session_id, file_id, file);
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

    async initiateWebRTCSend(targetSessionId, fileId, file) {
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

        // Create a DataChannel labelled with the fileId
        const dc = pc.createDataChannel(`file:${fileId}`, {
            ordered: true,
        });
        dc.binaryType = 'arraybuffer';

        const transferId = `send:${fileId}:${targetSessionId}`;

        dc.onopen = () => {
            clearTimeout(iceTimer);
            console.log(`DataChannel open → streaming ${file.name}`);
            this.streamFileOverDC(dc, fileId, file, targetSessionId);
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

    /**
     * Stream a File over a DataChannel using file.slice() — never loads the
     * entire file into RAM.
     *
     * Wire protocol (per DataChannel message):
     *   Message 0:  UTF-8 JSON header
     *       { "fileId", "fileName", "fileSize", "mimeType", "totalChunks" }
     *   Messages 1..N:  raw ArrayBuffer chunks (binary, no base64)
     */
    async streamFileOverDC(dc, fileId, file, targetSessionId) {
        const totalChunks = Math.ceil(file.size / this.RTC_CHUNK_SIZE);
        const transferId = `send:${fileId}:${targetSessionId}`;

        // 1. Send metadata header as a text message
        try {
            dc.send(JSON.stringify({
                fileId,
                fileName: file.name,
                fileSize: file.size,
                mimeType: file.type || 'application/octet-stream',
                totalChunks,
            }));
        } catch (err) {
            console.error('Failed to send file header:', err);
            this.closeRTC(targetSessionId);
            return;
        }

        this.activeTransfers.set(transferId, { startTime: Date.now(), bytesSent: 0, totalBytes: file.size });
        this.showProgress(`Sending ${file.name}`, 0);

        // 2. Stream binary chunks using file.slice() (disk → DataChannel)
        for (let i = 0; i < totalChunks; i++) {
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
                this.hideProgress();
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
                    this.hideProgress();
                    this.closeRTC(targetSessionId);
                    return;
                }
            }

            try {
                dc.send(buf);
            } catch (err) {
                console.error('DC send error:', err);
                this.activeTransfers.delete(transferId);
                this.hideProgress();
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
                this.showProgress(`Sending ${file.name}`, pct, speed, remaining);
            }
        }

        this.activeTransfers.delete(transferId);
        this.hideProgress();
        this.toast(`Sent ${file.name}`, 'success');
        console.log(`File ${file.name} sent to ${targetSessionId.slice(-6)}`);

        // Keep the connection open briefly so the last chunk flushes, then close
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

        // When the sender's DataChannel arrives
        pc.ondatachannel = (event) => {
            const dc = event.channel;
            dc.binaryType = 'arraybuffer';
            this.setupReceiverDC(dc, from_session_id);
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
     * Set up a DataChannel on the receiver side.
     * First message = JSON header, subsequent messages = binary chunks.
     */
    setupReceiverDC(dc, fromPeerId) {
        let meta = null;       // filled on first message
        let chunks = [];
        let receivedBytes = 0;
        let startTime = Date.now();
        let transferComplete = false;  // Flag to prevent onclose from firing after success

        dc.onmessage = (event) => {
            // First message is the JSON header (string)
            if (!meta) {
                try {
                    meta = JSON.parse(event.data);
                } catch (err) {
                    console.error('Invalid file header:', err);
                    this.closeRTC(fromPeerId);
                    return;
                }
                console.log(`Receiving ${meta.fileName} (${this.formatFileSize(meta.fileSize)}) from ${fromPeerId.slice(-6)}`);
                this.showProgress(`Downloading ${meta.fileName}`, 0);
                return;
            }

            // Subsequent messages are binary ArrayBuffers
            const chunk = event.data;           // ArrayBuffer
            chunks.push(chunk);
            receivedBytes += chunk.byteLength;

            const pct = Math.round((receivedBytes / meta.fileSize) * 100);
            const elapsed = (Date.now() - startTime) / 1000;
            const speed = elapsed > 0 ? receivedBytes / elapsed : 0;
            const remaining = speed > 0 ? (meta.fileSize - receivedBytes) / speed : 0;
            this.showProgress(`Downloading ${meta.fileName}`, pct, speed, remaining);

            // All chunks received?
            if (receivedBytes >= meta.fileSize) {
                transferComplete = true;  // Mark as complete before finalize
                this.finalizeReceivedFile(meta, chunks, fromPeerId);
            }
        };

        dc.onclose = () => {
            console.log('DataChannel closed (receiver side)');
            // If we received all data before close, finalizeReceivedFile
            // already handled it.  If not, the transfer was interrupted.
            // BUT: don't retry if user manually cancelled
            const wasCancelled = this.cancelledTransfers.has(`cancelled:${meta?.fileId}`);
            if (wasCancelled) {
                this.cancelledTransfers.delete(`cancelled:${meta.fileId}`);
                return; // User cancelled, don't retry
            }
            if (!transferComplete && meta && receivedBytes < meta.fileSize) {
                this.hideProgress();
                this.toast(`Transfer interrupted: ${meta.fileName} — retrying…`, 'warning');
                this._retryDownload(meta.fileId);
            }
        };

        dc.onerror = (err) => {
            console.error('DataChannel error (receiver):', err);
            const wasCancelled = this.cancelledTransfers.has(`cancelled:${meta?.fileId}`);
            if (wasCancelled) {
                this.cancelledTransfers.delete(`cancelled:${meta.fileId}`);
                return; // User cancelled, don't retry
            }
            if (!transferComplete) {
                this.hideProgress();
                if (meta) {
                    this.toast(`Transfer error: ${meta.fileName} — retrying…`, 'warning');
                    this._retryDownload(meta.fileId);
                }
            }
        };
    }

    /** Assemble received chunks into a File, trigger browser download, become a host. */
    finalizeReceivedFile(meta, chunks, fromPeerId) {
        const blob = new Blob(chunks, { type: meta.mimeType });
        const file = new File([blob], meta.fileName, { type: meta.mimeType });

        // Store locally so WE become a host too
        this.storeFile(meta.fileId, file);

        // Trigger browser "Save As"
        this.saveFileToDisk(file, meta.fileName);

        this.hideProgress();
        this.pendingDownloads.delete(meta.fileId);
        this.activeTransfers.delete(`retry:${meta.fileId}`);
        this.toast(`Downloaded ${meta.fileName} (${this.formatFileSize(meta.fileSize)})`, 'success', 5000);
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
        document.getElementById('cancel-transfer').addEventListener('click', () => {
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

        tbody.innerHTML = allItems.map(item => {
            if (item.type === 'file') {
                const f = item.data;
                const hosts = Array.isArray(f.hosts) ? f.hosts : Array.from(f.hosts || []);
                const isDownloading = this.pendingDownloads.has(f.id);
                return `
                    <tr class="file-row">
                        <td class="file-name">📄 ${this.escapeHtml(f.name)}</td>
                        <td class="file-type">${this.escapeHtml(f.mime_type)}</td>
                        <td class="file-size">${this.formatSize(f.size)}</td>
                        <td>
                            <div class="file-hosts">
                                ${hosts.map(h => h === this.sessionId
                                    ? '<span class="host-badge host-self">You</span>'
                                    : `<span class="host-badge">${h.slice(-6)}</span>`
                                ).join('')}
                            </div>
                        </td>
                        <td class="file-actions">
                            ${hosts.length > 0
                                ? `<button class="btn download${isDownloading ? ' downloading' : ''}" onclick="app.downloadFile('${f.id}')" ${isDownloading ? 'disabled' : ''}>${isDownloading ? '⏳ Downloading…' : '⬇️ Download'}</button>`
                                : '<span style="color:#a0aec0;">No hosts</span>'}
                        </td>
                    </tr>`;
            } else {
                const m = item.data;
                const who = m.sender_id === this.sessionId ? 'You' : `User ${m.sender_id.slice(-6)}`;
                const preview = m.content.length > 50 ? m.content.substring(0, 50) + '…' : m.content;
                return `
                    <tr class="message-row">
                        <td class="file-name">💬 ${this.escapeHtml(preview)}</td>
                        <td class="file-type">Text Message</td>
                        <td class="file-size">${m.content.length} chars</td>
                        <td><span class="host-badge">${who}</span></td>
                        <td class="file-actions">
                            <button class="btn download" onclick="app.viewMessage('${m.id}')">View</button>
                        </td>
                    </tr>`;
            }
        }).join('');
    }

    // ── Progress modal ──────────────────────────────────────────────────

    showProgress(filename, percentage, speedBytesPerSec, etaSeconds) {
        const modal      = document.getElementById('progress-modal');
        const filenameEl = document.getElementById('progress-filename');
        const pctEl      = document.getElementById('progress-percentage');
        const fillEl     = document.getElementById('progress-fill');
        const speedEl    = document.getElementById('progress-speed');
        const etaEl      = document.getElementById('progress-eta');
        const bytesEl    = document.getElementById('progress-bytes');
        if (!modal) return;

        if (filenameEl) filenameEl.textContent = filename;
        if (pctEl)      pctEl.textContent = `${percentage}%`;
        if (fillEl)     fillEl.style.width = `${percentage}%`;

        if (speedEl) {
            speedEl.textContent = speedBytesPerSec != null
                ? `${this.formatFileSize(speedBytesPerSec)}/s`
                : '';
        }
        if (etaEl) {
            if (etaSeconds != null && isFinite(etaSeconds)) {
                const s = Math.round(etaSeconds);
                if (s < 60) etaEl.textContent = `${s}s left`;
                else if (s < 3600) etaEl.textContent = `${Math.floor(s/60)}m ${s%60}s left`;
                else etaEl.textContent = `${Math.floor(s/3600)}h ${Math.floor((s%3600)/60)}m left`;
            } else {
                etaEl.textContent = 'Calculating…';
            }
        }

        // Show transferred / total bytes
        if (bytesEl) {
            // Determine total from an active transfer
            let transferred = 0, total = 0;
            for (const [, t] of this.activeTransfers) {
                if (t.totalBytes) {
                    transferred = t.bytesSent || 0;
                    total = t.totalBytes;
                    break;
                }
            }
            bytesEl.textContent = total > 0
                ? `${this.formatFileSize(transferred)} / ${this.formatFileSize(total)}`
                : '';
        }

        modal.style.display = 'block';
    }

    hideProgress() {
        const modal = document.getElementById('progress-modal');
        if (modal) modal.style.display = 'none';
    }

    cancelActiveTransfer() {
        // Mark all current transfers as cancelled so async loops break
        for (const [id] of this.activeTransfers) {
            this.cancelledTransfers.add(id);
        }
        // Mark all pending downloads as cancelled to prevent retry
        for (const fileId of this.pendingDownloads) {
            this.cancelledTransfers.add(`cancelled:${fileId}`);
        }
        // Close all active RTC connections to abort transfers
        for (const [peerId] of this.rtcConnections) {
            this.closeRTC(peerId);
        }
        this.activeTransfers.clear();
        this.pendingDownloads.clear();
        this.hideProgress();
        this.toast('Transfer cancelled', 'info');
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
    window.app = new LADEXApp();
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

LADEXApp.prototype.escapeHtml = function(text) {
    const d = document.createElement('div');
    d.textContent = text;
    return d.innerHTML;
};