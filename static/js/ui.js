// Everything drawn on the page: device list, file list, toasts, dialogs and modals.

(function () {
    'use strict';


    /** Nickname, else the User-Agent name, else a short session id. */
    LADEXApp.prototype._deviceDisplayName = function(sessionId, peer) {
        const isSelf = sessionId === this.sessionId;
        const nickname = isSelf ? this.nickname : peer?.nickname;
        if (nickname) return nickname;
        const friendly = LadexUtil.friendlyDeviceName(peer?.user_agent);
        if (friendly) return friendly;
        return `Peer ${sessionId.slice(-6)}`;
    };

    LADEXApp.prototype.updateDevicesList = function() {
        const list = document.getElementById('devices-list');
        const countEl = document.getElementById('devices-count');
        if (!list) return;

        const others = Array.from(this.peers.values()).filter(p => p.session_id !== this.sessionId && !p.left);
        const self = { session_id: this.sessionId, nickname: this.nickname };

        const chip = (peer, isSelf) => {
            const name = LadexUtil.escapeHtml(this._deviceDisplayName(peer.session_id, peer));
            const host = peer.hosting_node_name ? LadexUtil.escapeHtml(peer.hosting_node_name) : '';
            const rtt = (!isSelf && peer.node_rtt_ms != null) ? `${peer.node_rtt_ms}ms` : '';
            const sub = [host, rtt].filter(Boolean).join(' · ');
            const youBadge = isSelf ? ' <span class="device-chip-you">(you)</span>' : '';
            return `
                <div class="device-chip${isSelf ? ' device-chip-self' : ''}" data-session-id="${LadexUtil.escapeHtml(peer.session_id)}" title="${isSelf ? 'Click to set a nickname' : 'Click to send a file, or drag one here'}">
                    <div class="device-chip-name">${name}${youBadge}</div>
                    ${sub ? `<div class="device-chip-host">${sub}</div>` : ''}
                </div>`;
        };

        list.innerHTML = chip(self, true) + others.map(p => chip(p, false)).join('');
        if (countEl) countEl.textContent = `(${1 + others.length})`;
    };

    /** Turns the "you" chip's name into an inline input; prompt() would block the event loop mid-transfer. */
    LADEXApp.prototype._editNicknameInline = function(chipEl) {
        const nameEl = chipEl.querySelector('.device-chip-name');
        if (!nameEl || chipEl.querySelector('.device-chip-input')) return;

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
    };

    LADEXApp.prototype._ensureToastContainer = function() {
        if (this._toastContainer) return;
        this._toastContainer = document.createElement('div');
        this._toastContainer.id = 'toast-container';
        document.body.appendChild(this._toastContainer);
    };

    /** Shows a toast; `action` adds a button such as Retry. */
    LADEXApp.prototype.toast = function(message, type = 'info', durationMs = 4000, action = null) {
        this._ensureToastContainer();
        const el = document.createElement('div');
        el.className = `toast toast-${type}`;
        const icons = { info: 'ℹ️', success: '✅', error: '❌', warning: '⚠️' };
        el.innerHTML = `<span class="toast-icon">${icons[type] || ''}</span><span class="toast-msg">${LadexUtil.escapeHtml(message)}</span>`;
        if (action) {
            const btn = document.createElement('button');
            btn.className = 'toast-action';
            btn.textContent = action.label;
            btn.addEventListener('click', () => { action.onClick(); dismiss(); });
            el.appendChild(btn);
        }
        this._toastContainer.appendChild(el);
        requestAnimationFrame(() => el.classList.add('toast-visible'));
        const dismiss = () => {
            el.classList.remove('toast-visible');
            el.classList.add('toast-exit');
            el.addEventListener('transitionend', () => el.remove());
            setTimeout(() => el.remove(), 500);
        };
        setTimeout(dismiss, durationMs);
    };

    /** Unshares a file; the server enforces who may, this only hides the button from others. */
    LADEXApp.prototype.deleteFile = function(fileId) {
        const f = this.serverFiles.find(x => x.id === fileId);
        this._pendingDeletes.set(fileId, f ? f.name : 'File');
        this.sendWS({ type: 'delete_file', session_id: this.sessionId, file_id: fileId });
    };

    LADEXApp.prototype.offerFileToPeer = function(fileId, targetSessionId) {
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
    };

    LADEXApp.prototype._showSendFilePicker = function(anchorEl, targetSessionId) {
        document.querySelector('.send-file-popover')?.remove();
        const options = this.serverFiles
            .filter((f) => f.uploader_id === this.sessionId && !f.parent)
            .map((f) => ({ id: f.id, name: f.name, icon: f.is_folder ? '📁' : '📄' }));
        if (options.length === 0) {
            this.toast('You have nothing to send — upload something first', 'info');
            return;
        }

        const targetName = LadexUtil.escapeHtml(this._deviceDisplayName(targetSessionId, this.peers.get(targetSessionId)));
        const popover = document.createElement('div');
        popover.className = 'send-file-popover';
        popover.innerHTML = `
            <div class="send-file-popover-title">Send to ${targetName}</div>
            ${options.map(o => `
                <button class="send-file-option" data-file-id="${LadexUtil.escapeHtml(o.id)}">${o.icon} ${LadexUtil.escapeHtml(o.name)}</button>
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

        // Deferred so this same click doesn't close it.
        setTimeout(() => {
            const closeHandler = (e) => {
                if (!popover.contains(e.target)) {
                    popover.remove();
                    document.removeEventListener('click', closeHandler);
                }
            };
            document.addEventListener('click', closeHandler);
        }, 0);
    };

    /** Someone pointed us at a file; accepting just downloads it. */
    LADEXApp.prototype._showFileOfferDialog = function(msg) {
        const file = this.serverFiles.find((f) => f.id === msg.file_id);
        const senderName = LadexUtil.escapeHtml(this._deviceDisplayName(msg.from_session_id, this.peers.get(msg.from_session_id)));
        const fileName = file ? LadexUtil.escapeHtml(file.name) : 'a file';
        const sizeStr = file ? LadexUtil.formatFileSize(file.is_folder ? file.folder_bytes : file.size) : '';

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
    };

    LADEXApp.prototype.updateFileList = function(files) {
        const tbody = document.getElementById('files-list');
        const allItems = [];

        if (files && files.length > 0) {
            files.filter(f => !f.parent).forEach(f => allItems.push({ type: 'file', data: f, timestamp: new Date(f.uploaded_at) }));
        }
        this.messages.forEach(m => allItems.push({ type: 'message', data: m, timestamp: new Date(m.timestamp) }));
        allItems.sort((a, b) => b.timestamp - a.timestamp);

        if (allItems.length === 0) {
            tbody.innerHTML = '<tr class="no-files"><td colspan="5">No files or messages shared yet!</td></tr>';
            return;
        }

        // Everything below comes from other devices: escape all of it, and use data-action attributes instead of inline handlers.
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
                    <tr class="file-row" draggable="true" data-file-id="${LadexUtil.escapeHtml(f.id)}">
                        <td class="file-name">${icon} ${LadexUtil.escapeHtml(f.name)}</td>
                        <td class="file-type">${LadexUtil.escapeHtml(typeLabel)}</td>
                        <td class="file-size">${LadexUtil.formatFileSize(size)}</td>
                        <td>
                            <div class="file-hosts">
                                ${holders.map(name => `<span class="host-badge">${LadexUtil.escapeHtml(name)}</span>`).join('')}
                            </div>
                        </td>
                        <td class="file-actions">
                            ${holders.length > 0
                                ? `<button class="btn download${isDownloading ? ' downloading' : ''}" data-action="download-file" data-file-id="${LadexUtil.escapeHtml(f.id)}" ${isDownloading ? 'disabled' : ''}>${isDownloading ? '⏳ Starting…' : '⬇️ Download'}</button>`
                                : '<span style="color:#a0aec0;" title="Every device that has this file is offline">Unavailable</span>'}
                            ${canDelete ? `<button class="btn secondary delete-file-btn" data-action="delete-file" data-file-id="${LadexUtil.escapeHtml(f.id)}" title="Unshare">🗑️</button>` : ''}
                        </td>
                    </tr>`;
            } else {
                const m = item.data;
                const who = m.sender_id === this.sessionId ? 'You' : LadexUtil.escapeHtml(this._deviceDisplayName(m.sender_id, this.peers.get(m.sender_id)));
                const preview = m.content.length > 50 ? m.content.substring(0, 50) + '…' : m.content;
                return `
                    <tr class="message-row">
                        <td class="file-name">💬 ${LadexUtil.escapeHtml(preview)}</td>
                        <td class="file-type">Text Message</td>
                        <td class="file-size">${m.content.length} chars</td>
                        <td><span class="host-badge">${who}</span></td>
                        <td class="file-actions">
                            <button class="btn download" data-action="view-message" data-message-id="${LadexUtil.escapeHtml(m.id)}">View</button>
                        </td>
                    </tr>`;
            }
        }).join('');
    };

    /** Shows a sticky banner when no other nodes are found, likely AP isolation. */
    LADEXApp.prototype._showApIsolationBanner = function(message) {
        if (document.getElementById('ap-isolation-banner')) return;
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
    };

    LADEXApp.prototype.showError = function(message) {
        console.error(message);
        this.toast(message, 'error', 6000);
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

    /** Shows a QR code so a phone can join by scanning instead of typing the URL. */
    LADEXApp.prototype.showAddDeviceModal = function() {
        const url = location.href;
        document.getElementById('qr-code-container').innerHTML = this._renderQrSvg(url);
        document.getElementById('add-device-url').textContent = url;
        document.getElementById('add-device-modal').style.display = 'block';
    };

    LADEXApp.prototype.hideAddDeviceModal = function() {
        document.getElementById('add-device-modal').style.display = 'none';
    };

    /** Renders a QR code as inline SVG using the vendored qrcode.js. */
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
})();
