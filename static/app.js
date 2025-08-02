class LADEXApp {
    constructor() {
        this.ws = null;
        this.sessionId = this.generateSessionId();
        this.files = new Map();
        this.peers = new Map();
        this.activeTransfers = new Map();
        this.activeDownloads = new Map();
        this.pendingDownloads = new Set();
        this.rtcConnections = new Map();
        
        this.init();
    }

    generateSessionId() {
        return 'peer_' + Math.random().toString(36).substr(2, 9) + '_' + Date.now();
    }

    init() {
        this.connectWebSocket();
        this.setupEventListeners();
        this.setupRTC();
    }

    connectWebSocket() {
        const wsProtocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        const wsUrl = `${wsProtocol}//${window.location.host}/ws`;
        
        this.ws = new WebSocket(wsUrl);
        
        this.ws.onopen = () => {
            console.log('Connected to LADEX server');
            this.updateConnectionStatus(true);
            this.joinSession();
        };
        
        this.ws.onmessage = (event) => {
            const message = JSON.parse(event.data);
            this.handleServerMessage(message);
        };
        
        this.ws.onclose = () => {
            console.log('Disconnected from LADEX server');
            this.updateConnectionStatus(false);
            setTimeout(() => this.connectWebSocket(), 3000);
        };
        
        this.ws.onerror = (error) => {
            console.error('WebSocket error:', error);
        };
    }

    joinSession() {
        const message = {
            type: 'join',
            session_id: this.sessionId,
            user_agent: navigator.userAgent
        };
        this.sendMessage(message);
    }

    sendMessage(message) {
        if (this.ws && this.ws.readyState === WebSocket.OPEN) {
            this.ws.send(JSON.stringify(message));
        }
    }

    handleServerMessage(message) {
        console.log('Received message:', message);
        
        switch (message.type) {
            case 'peer_joined':
                this.handlePeerJoined(message);
                break;
            case 'peer_left':
                this.handlePeerLeft(message);
                break;
            case 'file_list_update':
                this.updateFileList(message.files);
                break;
            case 'download_request':
                this.handleDownloadRequest(message);
                break;
            case 'file_metadata':
                this.handleFileMetadata(message);
                break;
            case 'file_chunk':
                this.handleFileChunk(message);
                break;
            case 'error':
                this.showError(message.message);
                break;
            case 'pong':
                console.log('Pong received');
                break;
        }
    }

    handlePeerJoined(message) {
        this.peers.set(message.peer.session_id, message.peer);
        this.updatePeerStatus(message.total_peers);
    }

    handlePeerLeft(message) {
        this.peers.delete(message.session_id);
        this.updatePeerStatus(message.total_peers);
    }

    updateConnectionStatus(connected) {
        const statusElement = document.getElementById('connection-status');
        if (connected) {
            statusElement.textContent = 'Connected';
            statusElement.className = 'status-connected';
        } else {
            statusElement.textContent = 'Disconnected';
            statusElement.className = 'status-disconnected';
        }
    }

    updatePeerStatus(count) {
        document.getElementById('peer-status').textContent = `Connected peers: ${count}`;
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
        });

        document.getElementById('file-input').addEventListener('change', (e) => {
            this.handleFileUpload(e.target.files, true);
        });

        document.getElementById('cancel-transfer').addEventListener('click', () => {
            this.cancelActiveTransfer();
        });
    }

    async handleFileUpload(files, isFolder) {
        if (!files || files.length === 0) return;

        try {
            if (isFolder) {
                await this.handleFolderUpload(files);
            } else {
                for (const file of files) {
                    await this.uploadFile(file);
                }
            }
        } catch (error) {
            this.showError(`Upload failed: ${error.message}`);
        }
    }

    async handleFolderUpload(files) {
        if (typeof JSZip === 'undefined') {
            this.showError('JSZip library not loaded. Cannot zip folders.');
            return;
        }

        const zip = new JSZip();
        let totalSize = 0;

        for (const file of files) {
            const relativePath = file.webkitRelativePath || file.name;
            zip.file(relativePath, file);
            totalSize += file.size;
        }

        const folderName = files[0].webkitRelativePath?.split('/')[0] || 'folder';
        const zipBlob = await zip.generateAsync({ type: 'blob' });
        
        const zipFile = new File([zipBlob], `${folderName}.zip`, { 
            type: 'application/zip' 
        });

        await this.uploadFile(zipFile);
    }

    async uploadFile(file) {
        const fileId = this.generateFileId();
        const fileMetadata = {
            id: fileId,
            name: file.name,
            size: file.size,
            mime_type: file.type || 'application/octet-stream',
            uploader_id: this.sessionId,
            hosts: new Set([this.sessionId]),
            uploaded_at: new Date().toISOString()
        };

        this.storeFile(fileId, file);
        
        const message = {
            type: 'file_upload',
            session_id: this.sessionId,
            file: {
                ...fileMetadata,
                hosts: [this.sessionId]
            }
        };
        
        this.sendMessage(message);
    }

    generateFileId() {
        return 'file_' + Math.random().toString(36).substr(2, 12) + '_' + Date.now();
    }

    storeFile(fileId, file) {
        this.files.set(fileId, file);
    }

    updateFileList(files) {
        const tbody = document.getElementById('files-list');
        
        if (!files || files.length === 0) {
            tbody.innerHTML = '<tr class="no-files"><td colspan="5">No files shared yet. Upload some files to get started!</td></tr>';
            return;
        }

        const filesHtml = files.map(file => {
            const hosts = Array.isArray(file.hosts) ? file.hosts : Array.from(file.hosts || []);
            const isDownloadable = hosts.length > 0;
            
            return `
                <tr>
                    <td class="file-name">${file.name}</td>
                    <td class="file-type">${file.mime_type}</td>
                    <td class="file-size">${this.formatSize(file.size)}</td>
                    <td>
                        <div class="file-hosts">
                            ${hosts.map(host => `<span class="host-badge">${host}</span>`).join('')}
                        </div>
                    </td>
                    <td class="file-actions">
                        ${isDownloadable ? 
                            `<button class="btn download" onclick="app.downloadFile('${file.id}')">⬇️ Download</button>` :
                            '<span style="color: #a0aec0;">No hosts</span>'
                        }
                    </td>
                </tr>
            `;
        }).join('');

        tbody.innerHTML = filesHtml;
    }

    formatSize(bytes) {
        const sizes = ['B', 'KB', 'MB', 'GB'];
        if (bytes === 0) return '0 B';
        const i = Math.floor(Math.log(bytes) / Math.log(1024));
        return Math.round(bytes / Math.pow(1024, i) * 100) / 100 + ' ' + sizes[i];
    }

    async downloadFile(fileId) {
        console.log(`Requesting download for file: ${fileId}`);
        this.pendingDownloads = this.pendingDownloads || new Set();
        this.pendingDownloads.add(fileId);

        const message = {
            type: 'request_download',
            session_id: this.sessionId,
            file_id: fileId
        };
        this.sendMessage(message);
        console.log('Download request sent:', message);
    }

    parseSizeString(sizeStr) {
        const units = { 'B': 1, 'KB': 1024, 'MB': 1024*1024, 'GB': 1024*1024*1024 };
        const match = sizeStr.match(/^([\d.]+)\s*([A-Z]+)$/);
        if (match) {
            return parseFloat(match[1]) * (units[match[2]] || 1);
        }
        return 0;
    }

    async handleDownloadRequest(message) {
        const { from_session_id, file_id, requester_session_id } = message;

        // If this is a response to our download request
        if (requester_session_id === this.sessionId && this.pendingDownloads && this.pendingDownloads.has(file_id)) {
            this.pendingDownloads.delete(file_id);
            
            // Set up to receive the file from the host
            this.activeDownloads = this.activeDownloads || new Map();
            this.activeDownloads.set(file_id, {
                chunks: [],
                expectedChunks: 0,
                receivedChunks: 0,
                fileName: '',
                mimeType: '',
                fromPeer: from_session_id
            });
            
            console.log(`Waiting to receive file ${file_id} from ${from_session_id}`);
            return;
        }
        
        // If we are the host being requested to send a file
        if (from_session_id === this.sessionId) {
            const file = this.files.get(file_id);
            if (!file) {
                console.error('Requested file not found:', file_id);
                return;
            }

            await this.sendFileToRequester(requester_session_id, file_id, file);
        }
    }

    setupRTC() {
        this.rtcConfig = {
            iceServers: [
                { urls: 'stun:stun.l.google.com:19302' }
            ]
        };
    }

    async sendFileToRequester(requesterSessionId, fileId, file) {
        try {
            console.log(`Sending file ${fileId} to ${requesterSessionId}`);
            
            const chunkSize = 64 * 1024;
            const totalChunks = Math.ceil(file.size / chunkSize);

            const metadataMessage = {
                type: 'file_metadata',
                session_id: this.sessionId,
                file_id: fileId,
                file_name: file.name,
                file_size: file.size,
                mime_type: file.type,
                total_chunks: totalChunks,
                target_session_id: requesterSessionId
            };
            this.sendMessage(metadataMessage);
            
            this.showProgress(`Sending ${file.name}`, 0);
            
            const arrayBuffer = await this.fileToArrayBuffer(file);
            const uint8Array = new Uint8Array(arrayBuffer);
            
            for (let chunkIndex = 0; chunkIndex < totalChunks; chunkIndex++) {
                const start = chunkIndex * chunkSize;
                const end = Math.min(start + chunkSize, uint8Array.length);
                const chunkData = uint8Array.slice(start, end);
                
                const base64Data = this.arrayBufferToBase64(chunkData);
                
                const chunkMessage = {
                    type: 'file_chunk',
                    session_id: this.sessionId,
                    file_id: fileId,
                    chunk_index: chunkIndex,
                    total_chunks: totalChunks,
                    data: base64Data,
                    target_session_id: requesterSessionId
                };
                
                this.sendMessage(chunkMessage);
                
                const progress = Math.round(((chunkIndex + 1) / totalChunks) * 100);
                this.showProgress(`Sending ${file.name}`, progress);
                
                await new Promise(resolve => setTimeout(resolve, 10));
            }
            
            this.hideProgress();
            console.log(`File ${fileId} sent successfully to ${requesterSessionId}`);
            
        } catch (error) {
            console.error('Error sending file:', error);
            this.hideProgress();
            this.showError(`Failed to send file: ${error.message}`);
        }
    }

    async fileToArrayBuffer(file) {
        return new Promise((resolve, reject) => {
            const reader = new FileReader();
            reader.onload = () => resolve(reader.result);
            reader.onerror = reject;
            reader.readAsArrayBuffer(file);
        });
    }

    arrayBufferToBase64(buffer) {
        let binary = '';
        const bytes = new Uint8Array(buffer);
        const len = bytes.byteLength;
        for (let i = 0; i < len; i++) {
            binary += String.fromCharCode(bytes[i]);
        }
        return btoa(binary);
    }

    async fileToBase64(file) {
        return new Promise((resolve, reject) => {
            const reader = new FileReader();
            reader.onload = () => {
                const base64 = reader.result.split(',')[1];
                resolve(base64);
            };
            reader.onerror = reject;
            reader.readAsDataURL(file);
        });
    }

    handleFileMetadata(message) {
        if (message.target_session_id !== this.sessionId) {
            return;
        }
        
        console.log(`Receiving file metadata for ${message.file_id} from ${message.from_session_id}`);
        
        this.activeDownloads = this.activeDownloads || new Map();
        this.activeDownloads.set(message.file_id, {
            chunks: new Array(message.total_chunks),
            expectedChunks: message.total_chunks,
            receivedChunks: 0,
            fileName: message.file_name,
            mimeType: message.mime_type,
            fileSize: message.file_size,
            fromPeer: message.from_session_id
        });
        
        this.showProgress(`Downloading ${message.file_name}`, 0);
    }

    handleFileChunk(message) {
        if (message.target_session_id !== this.sessionId) {
            return;
        }
        
        const download = this.activeDownloads.get(message.file_id);
        if (!download) {
            console.error(`Received chunk for unknown download: ${message.file_id}`);
            return;
        }
        
        if (!message.data || typeof message.data !== 'string') {
            console.error(`Invalid chunk data received for ${message.file_id}, chunk ${message.chunk_index}`);
            return;
        }
        
        download.chunks[message.chunk_index] = message.data;
        download.receivedChunks++;
        
        console.log(`Received chunk ${message.chunk_index + 1}/${download.expectedChunks} for ${download.fileName}`);
        
        const progress = Math.round((download.receivedChunks / download.expectedChunks) * 100);
        this.showProgress(`Downloading ${download.fileName}`, progress);
        
        if (download.receivedChunks === download.expectedChunks) {
            this.assembleAndDownloadFile(message.file_id, download);
        }
    }

    async assembleAndDownloadFile(fileId, download) {
        try {
            console.log(`Assembling file ${fileId} from ${download.receivedChunks} chunks`);
            
            // Check for missing chunks
            const missingChunks = [];
            for (let i = 0; i < download.expectedChunks; i++) {
                if (!download.chunks[i]) {
                    missingChunks.push(i);
                }
            }
            
            if (missingChunks.length > 0) {
                throw new Error(`Missing chunks: ${missingChunks.join(', ')}`);
            }
            
            // Convert each chunk from base64 back to binary and combine
            const allBytes = [];
            for (let i = 0; i < download.expectedChunks; i++) {
                const chunkBase64 = download.chunks[i];
                try {
                    const binaryString = atob(chunkBase64);
                    for (let j = 0; j < binaryString.length; j++) {
                        allBytes.push(binaryString.charCodeAt(j));
                    }
                } catch (error) {
                    throw new Error(`Failed to decode chunk ${i}: ${error.message}`);
                }
            }
            
            console.log(`Total bytes assembled: ${allBytes.length}, expected: ${download.fileSize}`);
            
            const uint8Array = new Uint8Array(allBytes);
            const blob = new Blob([uint8Array], { type: download.mimeType });
            const file = new File([blob], download.fileName, { type: download.mimeType });
            
            this.storeFile(fileId, file);
            
            const url = URL.createObjectURL(blob);
            const a = document.createElement('a');
            a.href = url;
            a.download = download.fileName;
            a.style.display = 'none';
            document.body.appendChild(a);
            a.click();
            document.body.removeChild(a);
            URL.revokeObjectURL(url);

            this.hideProgress();
            
            this.activeDownloads.delete(fileId);
            
            const message = {
                type: 'file_downloaded',
                session_id: this.sessionId,
                file_id: fileId
            };
            this.sendMessage(message);
            
            console.log(`File ${download.fileName} downloaded successfully`);
            
        } catch (error) {
            console.error('Error assembling file:', error);
            this.hideProgress();
            this.showError(`Failed to download file: ${error.message}`);
            this.activeDownloads.delete(fileId);
        }
    }

    setupRTC() {
        this.rtcConfig = {
            iceServers: [
                { urls: 'stun:stun.l.google.com:19302' }
            ]
        };
    }

    showProgress(filename, percentage) {
        const modal = document.getElementById('progress-modal');
        const filenameEl = document.getElementById('progress-filename');
        const percentageEl = document.getElementById('progress-percentage');
        const fillEl = document.getElementById('progress-fill');
        
        filenameEl.textContent = filename;
        percentageEl.textContent = `${percentage}%`;
        fillEl.style.width = `${percentage}%`;
        
        modal.style.display = 'block';
    }

    hideProgress() {
        document.getElementById('progress-modal').style.display = 'none';
    }

    cancelActiveTransfer() {
        this.activeTransfers.clear();
        this.hideProgress();
    }

    showError(message) {
        alert(`Error: ${message}`);
        console.error(message);
    }
}

document.addEventListener('DOMContentLoaded', () => {
    window.app = new LADEXApp();
});

window.addEventListener('beforeunload', () => {
    if (window.app && window.app.ws) {
        window.app.ws.close();
    }
});
