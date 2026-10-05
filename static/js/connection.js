// Connection to the node: the WebSocket, messages from the server, chat and node/device naming.

(function () {
    'use strict';

    /** Which node this is, and whether we are on the machine running it. */
    LADEXApp.prototype._loadNodeInfo = async function() {
        try {
            const status = await (await fetch('/auth-status', { cache: 'no-store' })).json();
            this.nodeId = status.node_id || null;
            this.isHost = !!status.is_host;
            this.updateFileList(this.serverFiles);
        } catch (_) { /* the page works without it; only the unshare button depends on it */ }
    };

    LADEXApp.prototype.connectWebSocket = function() {
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
    };

    /** Reconnects, unless this device was signed out: then the node would refuse, so go to the login page. */
    LADEXApp.prototype._reconnectOrSignIn = async function() {
        try {
            const status = await (await fetch('/auth-status', { cache: 'no-cache' })).json();
            if (status.auth_required && !status.authenticated) {
                window.location.href = '/login';
                return;
            }
        } catch (_) { /* node unreachable — fall through and retry */ }
        this.connectWebSocket();
    };

    LADEXApp.prototype.sendWS = function(msg) {
        if (this.ws && this.ws.readyState === WebSocket.OPEN) {
            this.ws.send(JSON.stringify(msg));
        }
    };

    LADEXApp.prototype.handleServerMessage = function(msg) {
        switch (msg.type) {
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
            case 'peer_sync':
                if (msg.peers) {
                    for (const peer of msg.peers) {
                        if (peer.hosting_node_id == null) {
                            this.peers.delete(peer.session_id);
                        } else {
                            this._rememberNode(peer);
                            const existing = this.peers.get(peer.session_id) || {};
                            this.peers.set(peer.session_id, { ...existing, ...peer });
                        }
                    }
                }
                this.updateDevicesList();
                this.updateFileList(this.serverFiles);
                break;

            case 'file_list_update':
                this.serverFiles = msg.files || [];
                this.updateFileList(this.serverFiles);
                break;

            // deleteFile() records the name first, so this confirms with it instead of double-toasting.
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

            case 'incoming_file_offer':
                this._showFileOfferDialog(msg);
                break;
            case 'file_offer_declined': {
                const name = this._deviceDisplayName(msg.from_session_id, this.peers.get(msg.from_session_id));
                this.toast(`${name} declined your file offer`, 'warning');
                break;
            }

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
    };

    LADEXApp.prototype._rememberNode = function(peer) {
        if (peer && peer.hosting_node_id && peer.hosting_node_name) {
            this.nodeNames.set(peer.hosting_node_id, peer.hosting_node_name);
        }
    };

    LADEXApp.prototype._nodeName = function(nodeId) {
        return this.nodeNames.get(nodeId) || `node ${String(nodeId).slice(-6)}`;
    };

    /** Names of the nodes that currently have the whole file. */
    LADEXApp.prototype._holderNames = function(file) {
        return Object.entries(file.holders || {})
            .filter(([, holder]) => holder && holder.present)
            .map(([nodeId]) => this._nodeName(nodeId));
    };

    LADEXApp.prototype.updateConnectionStatus = function(connected) {
        const el = document.getElementById('connection-status');
        if (!el) return;
        el.textContent = connected ? 'Connected' : 'Disconnected';
        el.className   = connected ? 'status-connected' : 'status-disconnected';
    };

    LADEXApp.prototype.updatePeerStatus = function(count) {
        const ps = document.getElementById('peer-status');
        const pn = document.getElementById('peer-number');
        if (ps) ps.textContent = `Connected peers: ${count}`;
        if (pn) pn.textContent = `Peer: ${this.getShortPeerId()}`;
    };

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
})();
