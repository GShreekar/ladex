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

        // ── WebRTC state ────────────────────────────────────────────────
        // Active RTCPeerConnections keyed by remote sessionId.
        this.rtcConnections = new Map();
        // ICE candidates that arrived before the remote description was set.
        this.pendingCandidates = new Map();
        // Track ongoing sends/receives for progress UI.
        this.activeTransfers = new Map();

        this.RTC_CHUNK_SIZE = 64 * 1024; // 64 KB per DataChannel message

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
        };

        this.ws.onmessage = (e) => this.handleServerMessage(JSON.parse(e.data));

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

            // ── file catalog ────────────────────────────────────────────
            case 'file_list_update':
                this.serverFiles = msg.files || [];
                this.updateFileList(this.serverFiles);
                break;

            // ── download orchestration ──────────────────────────────────
            // Server tells US (a host) to send a file to a requester.
            // We are the host → create an RTC offer and stream the file.
            case 'download_request':
                this.handleDownloadRequest(msg);
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
                this.showError(msg.message);
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
        } catch (err) {
            this.showError(`Upload failed: ${err.message}`);
        }
    }

    async handleFolderUpload(files) {
        if (typeof JSZip === 'undefined') {
            this.showError('JSZip library not loaded.');
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
    //  DOWNLOAD REQUEST
    // =====================================================================

    /** User clicks "Download" — tell the server we want this file. */
    downloadFile(fileId) {
        // Check: if WE already have the file locally, just save it.
        const localFile = this.getFile(fileId);
        if (localFile) {
            this.saveFileToDisk(localFile, localFile.name);
            return;
        }
        console.log(`Requesting download for ${fileId}`);
        this.sendWS({ type: 'request_download', session_id: this.sessionId, file_id: fileId });
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
        // On a LAN, STUN is technically unnecessary, but it helps with
        // host-candidate gathering on some platforms.
        return { iceServers: [{ urls: 'stun:stun.l.google.com:19302' }] };
    }

    // ── Sender side (host) ──────────────────────────────────────────────

    async initiateWebRTCSend(targetSessionId, fileId, file) {
        const pc = new RTCPeerConnection(this.getRTCConfig());
        this.rtcConnections.set(targetSessionId, pc);

        // Create a DataChannel labelled with the fileId
        const dc = pc.createDataChannel(`file:${fileId}`, {
            ordered: true,
        });
        dc.binaryType = 'arraybuffer';

        dc.onopen = () => {
            console.log(`DataChannel open → streaming ${file.name}`);
            this.streamFileOverDC(dc, fileId, file, targetSessionId);
        };

        dc.onclose = () => {
            console.log('DataChannel closed (sender side)');
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
            if (pc.connectionState === 'failed' || pc.connectionState === 'disconnected') {
                console.warn('RTC connection failed/disconnected (sender)');
                this.closeRTC(targetSessionId);
            }
        };

        // Create offer
        const offer = await pc.createOffer();
        await pc.setLocalDescription(offer);

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

        // 1. Send metadata header as a text message
        dc.send(JSON.stringify({
            fileId,
            fileName: file.name,
            fileSize: file.size,
            mimeType: file.type || 'application/octet-stream',
            totalChunks,
        }));

        const transferId = `send:${fileId}:${targetSessionId}`;
        this.activeTransfers.set(transferId, { startTime: Date.now(), bytesSent: 0, totalBytes: file.size });
        this.showProgress(`Sending ${file.name}`, 0);

        // 2. Stream binary chunks using file.slice() (disk → DataChannel)
        for (let i = 0; i < totalChunks; i++) {
            const start = i * this.RTC_CHUNK_SIZE;
            const end   = Math.min(start + this.RTC_CHUNK_SIZE, file.size);
            const blob  = file.slice(start, end);
            const buf   = await blob.arrayBuffer();

            // Back-pressure: wait if the DC buffer is getting full
            while (dc.bufferedAmount > 4 * 1024 * 1024) {
                await new Promise(r => setTimeout(r, 20));
            }

            dc.send(buf);

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
        console.log(`File ${file.name} sent to ${targetSessionId.slice(-6)}`);

        // Keep the connection open briefly so the last chunk flushes, then close
        setTimeout(() => this.closeRTC(targetSessionId), 2000);
    }

    // ── Receiver side (requester) ────────────────────────────────────────

    /** Server relays an SDP offer from a host that will send us a file. */
    async handleWebRTCOffer(msg) {
        const { from_session_id, sdp } = msg;
        const remoteDesc = JSON.parse(sdp);

        const pc = new RTCPeerConnection(this.getRTCConfig());
        this.rtcConnections.set(from_session_id, pc);

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
            if (pc.connectionState === 'failed' || pc.connectionState === 'disconnected') {
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

        await pc.setRemoteDescription(new RTCSessionDescription(remoteDesc));

        // Flush any ICE candidates that arrived before the remote description
        this.flushPendingCandidates(from_session_id, pc);

        const answer = await pc.createAnswer();
        await pc.setLocalDescription(answer);

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
        await pc.setRemoteDescription(new RTCSessionDescription(JSON.parse(sdp)));
        this.flushPendingCandidates(from_session_id, pc);
    }

    /** Server relays an ICE candidate. */
    async handleICECandidate(msg) {
        const { from_session_id, candidate } = msg;
        const pc = this.rtcConnections.get(from_session_id);
        const iceCandidate = new RTCIceCandidate(JSON.parse(candidate));

        if (pc && pc.remoteDescription) {
            await pc.addIceCandidate(iceCandidate);
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

        dc.onmessage = (event) => {
            // First message is the JSON header (string)
            if (!meta) {
                meta = JSON.parse(event.data);
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
                this.finalizeReceivedFile(meta, chunks, fromPeerId);
            }
        };

        dc.onclose = () => {
            console.log('DataChannel closed (receiver side)');
            // If we received all data before close, finalizeReceivedFile
            // already handled it.  If not, the transfer was interrupted.
            if (meta && receivedBytes < meta.fileSize) {
                this.hideProgress();
                this.showError(`Transfer interrupted: ${meta.fileName}`);
            }
        };

        dc.onerror = (err) => {
            console.error('DataChannel error (receiver):', err);
            this.hideProgress();
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
            pc.close();
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
                                ? `<button class="btn download" onclick="app.downloadFile('${f.id}')">⬇️ Download</button>`
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
                etaEl.textContent = s < 60 ? `${s}s left` : `${Math.floor(s/60)}m ${s%60}s left`;
            } else {
                etaEl.textContent = 'Calculating…';
            }
        }

        modal.style.display = 'block';
    }

    hideProgress() {
        const modal = document.getElementById('progress-modal');
        if (modal) modal.style.display = 'none';
    }

    cancelActiveTransfer() {
        // Close all active RTC connections to abort transfers
        for (const [peerId] of this.rtcConnections) {
            this.closeRTC(peerId);
        }
        this.activeTransfers.clear();
        this.hideProgress();
    }

    showError(message) {
        console.error(message);
        alert(`Error: ${message}`);
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