// Uploading, downloading and the progress panel.

(function () {
    'use strict';


    LADEXApp.prototype.handleFileUpload = async function(files, isFolder) {
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
    };

    /** Runs `work` over `items`, at most `limit` at a time. */
    LADEXApp.prototype._runLimited = async function(items, limit, work) {
        let next = 0;
        const worker = async () => {
            while (next < items.length) {
                const index = next++;
                await work(items[index], index);
            }
        };
        await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
    };

    LADEXApp.prototype._sleep = function(ms) {
        return new Promise((resolve) => setTimeout(resolve, ms));
    };

    /** How much of this upload the node already has (0 if none). */
    LADEXApp.prototype._uploadStatus = async function(fileId, size) {
        const response = await fetch(`/api/files/${fileId}/upload?size=${size}`, { cache: 'no-store' });
        if (response.status === 401) { window.location.href = '/login'; throw new Error('signed out'); }
        return response.json();
    };

    /** One attempt at sending the file from `offset`; resolves, never rejects. */
    LADEXApp.prototype._putFile = function(group, fileId, file, offset, parent, onProgress) {
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
    };

    /** Uploads one file, resuming after interruptions; resolves with the node's catalog entry. */
    LADEXApp.prototype.uploadFile = async function(file, { parent = null, group = null, report = null } = {}) {
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
    };

    /** Uploads a folder file by file, then publishes it as one entry listing them. */
    LADEXApp.prototype.handleFolderUpload = async function(files) {
        const folderId = this.generateFileId();
        const folderName = files[0].webkitRelativePath?.split('/')[0] || 'folder';
        if (files.length > LadexPolicy.MAX_FOLDER_FILES) {
            throw new Error(`a folder can hold at most ${LadexPolicy.MAX_FOLDER_FILES} files`);
        }
        const entries = files.map((file) => ({ path: file.webkitRelativePath || file.name, file }));
        const totalBytes = entries.reduce((sum, e) => sum + e.file.size, 0);
        const group = `up:${folderId}`;
        const started = Date.now();

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
    };

    LADEXApp.prototype.setupDragAndDrop = function() {
        const body = document.body;
        let dragDepth = 0;
        // File rows dragged onto device chips bubble up here too; only react to drags carrying OS files.
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
    };

    LADEXApp.prototype.downloadFile = async function(fileId) {
        const file = this.serverFiles.find((f) => f.id === fileId);
        if (!file) return;
        if (file.is_folder) return this.downloadFolder(file);
        if (this.pendingDownloads.has(fileId)) return;

        this.pendingDownloads.add(fileId);
        this.updateFileList(this.serverFiles);
        try {
            // A HEAD first tells us right away if the file can't be had, which a download link can't.
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
    };

    LADEXApp.prototype._downloadProblem = function(status) {
        if (status === 404) return 'That file has been removed.';
        if (status === 503) return 'No device that has this file is online right now.';
        if (status === 507) return 'This node does not have room to fetch that file.';
        return `The node could not provide that file (error ${status}).`;
    };

    /** Hands a URL to the browser's download manager. */
    LADEXApp.prototype._saveViaBrowser = function(url, name) {
        const a = document.createElement('a');
        a.href = url;
        a.download = name;
        a.style.display = 'none';
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
    };

    /** Downloads a folder into a picked directory where supported, otherwise as one zip from the node. */
    LADEXApp.prototype.downloadFolder = async function(folder) {
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
    };

    /** The file handle for a path inside a picked directory; a name clash becomes "name (1).ext", never a replacement. */
    LADEXApp.prototype._resolveFolderFileHandle = async function(dirHandle, relativePath) {
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
    };

    LADEXApp.prototype.showProgress = function(transferId, filename, percentage, speedBytesPerSec, etaSeconds, transferredBytes, totalBytes) {
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
            ? `${LadexUtil.formatFileSize(speedBytesPerSec)}/s`
            : '';

        const etaEl = card.querySelector('.progress-eta');
        if (etaSeconds != null && isFinite(etaSeconds)) {
            etaEl.textContent = LadexUtil.formatTimeLeft(etaSeconds);
        } else {
            etaEl.textContent = 'Calculating…';
        }

        card.querySelector('.progress-bytes').textContent = totalBytes > 0
            ? `${LadexUtil.formatFileSize(transferredBytes || 0)} / ${LadexUtil.formatFileSize(totalBytes)}`
            : '';

        panel.classList.add('visible');
    };

    LADEXApp.prototype.hideProgress = function(transferId) {
        const card = this._progressCards.get(transferId);
        if (card) {
            card.remove();
            this._progressCards.delete(transferId);
        }
        const panel = document.getElementById('progress-panel');
        if (panel && !this._progressCards.size) panel.classList.remove('visible');
    };

    /** Cancels one transfer by id. */
    LADEXApp.prototype.cancelTransfer = function(transferId) {
        this.cancelledTransfers.add(transferId);
        for (const xhr of this._uploads.get(transferId) ?? []) xhr.abort();
        this._aborters.get(transferId)?.abort();
        this.hideProgress(transferId);
        this.toast('Transfer cancelled', 'info');
    };

    /** Cancels every in-flight transfer. */
    LADEXApp.prototype.cancelActiveTransfer = function() {
        const ids = Array.from(this._progressCards.keys());
        for (const id of ids) this.cancelTransfer(id);
        if (ids.length === 0) this.toast('No transfers in progress', 'info');
    };
})();
