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

    // A tab keeps its id across a reload (so it still owns what it shared),
    // but two tabs never share one.
    generateSessionId() {
        try {
            const saved = sessionStorage.getItem('ladex_session');
            if (saved) return saved;
        } catch (_) { /* storage blocked */ }
        const id = LadexUtil.randomId('peer_', 10);
        try { sessionStorage.setItem('ladex_session', id); } catch (_) { /* storage blocked */ }
        return id;
    }

    generateFileId() {
        return LadexUtil.randomId('file_', 12);
    }

    getShortPeerId() {
        return this.sessionId.slice(-6);
    }

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

    initializePeerDisplay() {
        const update = () => {
            const el = document.getElementById('peer-number');
            if (el) { el.textContent = `Peer: ${this.getShortPeerId()}`; return true; }
            return false;
        };
        if (!update()) setTimeout(() => { if (!update()) setTimeout(update, 1000); }, 100);
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
