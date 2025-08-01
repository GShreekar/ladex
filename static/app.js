const SimplePeer = window.SimplePeer;
const JSZip     = window.JSZip;

// UUID generation with fallback for older browsers
function generateUUID() {
    try {
        if (crypto && crypto.randomUUID) {
            return crypto.randomUUID();
        }
    } catch (e) {
        console.warn('crypto.randomUUID not available, using fallback');
    }
    
    // Fallback UUID v4 generator
    return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, function(c) {
        const r = Math.random() * 16 | 0;
        const v = c == 'x' ? r : (r & 0x3 | 0x8);
        return v.toString(16);
    });
}

// Initialize application with error handling
function initializeApp() {
    // Check for required browser features
    if (!window.WebSocket) {
        alert('WebSocket support required. Please use a modern browser.');
        return false;
    }
    
    if (!window.SimplePeer) {
        console.error('SimplePeer library not loaded');
        return false;
    }
    
    if (!window.JSZip) {
        console.error('JSZip library not loaded');
        return false;
    }
    
    return true;
}

// Only proceed if browser is compatible
if (!initializeApp()) {
    throw new Error('Browser not compatible with LADEX');
}

const sessionId = generateUUID();
let ws;

try {
    ws = new WebSocket(`ws://${window.location.host}/ws`);
} catch (error) {
    console.error('Failed to create WebSocket connection:', error);
    alert('Failed to connect to LADEX server. Please check your connection.');
}

let peers = {};
let fileSystemAccessSupported = 'showSaveFilePicker' in window;
let downloadBuffers = {};
let metadataCache = {};

// Get DOM elements with error checking
const fileList        = document.getElementById('file-list');
const uploadFiles   = document.getElementById('upload-files');
const uploadFolders = document.getElementById('upload-folders');
const peerStatus      = document.getElementById('peer-status');
const textInput       = document.getElementById('text-input');
const sendTextBtn     = document.getElementById('send-text-btn');

// Check if required DOM elements exist
if (!fileList || !peerStatus) {
    console.error('Required DOM elements not found');
    alert('Page not loaded correctly. Please refresh.');
}

// Warn if upload elements are missing
if (!uploadFiles) {
    console.warn('File upload input not found');
}
if (!uploadFolders) {
    console.warn('Folder upload input not found');
}

if (peerStatus) {
    peerStatus.textContent = '🔄 Connecting to coordinator...';
    peerStatus.style.color = '#00d4ff';
}

// Check for File System Access API support
if (!fileSystemAccessSupported) {
    console.info('File System Access API not supported. Will use blob download fallback for large files.');
}

function updatePeerStatus() {
    if (peerStatus) {
        const allPeers = Object.keys(peers);
        const otherPeers = allPeers.filter(peerId => peerId !== sessionId);
        const peerCount = otherPeers.length;
        
        const baseText = peerStatus.textContent.includes('Connected') ? '🟢 Connected to coordinator' : 
                        peerStatus.textContent.includes('Disconnected') ? '🔴 Disconnected' :
                        peerStatus.textContent;
        
        if (peerCount > 0) {
            peerStatus.textContent = `${baseText} - ${peerCount} peer${peerCount === 1 ? '' : 's'} online`;
        } else {
            peerStatus.textContent = `${baseText} - No other peers`;
        }
    }
}

function getMetadataById(id) {
    return metadataCache[id];
}

// Make functions globally accessible for onclick handlers
window.getMetadataById = getMetadataById;

// Only set up WebSocket handlers if WebSocket was created successfully
if (ws) {
    ws.onmessage = (event) => {
        try {
            // Skip empty or non-string messages
            if (!event.data || typeof event.data !== 'string') {
                console.warn('Received non-string or empty message:', event.data);
                return;
            }
            
            const message = JSON.parse(event.data);
            
            // Validate message structure
            if (!message || typeof message !== 'object') {
                console.warn('Invalid message format received:', event.data);
                return;
            }
            
            // Handle error messages from server
            if (message.error) {
                console.error('Server error:', message.error);
                // Show user-friendly error message
                if (peerStatus) {
                    peerStatus.textContent = '⚠️ Server error - check console';
                    peerStatus.style.color = '#ffaa00';
                }
                return;
            }
            
            // Determine message type from Rust enum format
            let messageType = null;
            let messageData = null;
            
            if (message.MetadataUpdate !== undefined) {
                messageType = 'MetadataUpdate';
                messageData = message.MetadataUpdate;
            } else if (message.RequestDownload !== undefined) {
                messageType = 'RequestDownload';
                messageData = message.RequestDownload;
            } else if (message.Offer !== undefined) {
                messageType = 'Offer';
                messageData = message.Offer;
            } else if (message.Answer !== undefined) {
                messageType = 'Answer';
                messageData = message.Answer;
            } else if (message.IceCandidate !== undefined) {
                messageType = 'IceCandidate';
                messageData = message.IceCandidate;
            } else if (message.TextMessage !== undefined) {
                messageType = 'TextMessage';
                messageData = message.TextMessage;
            } else if (message.PeerDisconnected !== undefined) {
                messageType = 'PeerDisconnected';
                messageData = message.PeerDisconnected;
            } else {
                console.warn('Unknown message format. Full message:', message, 'Raw data:', event.data);
                return;
            }
            
            switch (messageType) {
                case 'MetadataUpdate':
                    if (messageData) {
                        updateFileList(messageData);
                    } else {
                        console.warn('MetadataUpdate missing data');
                    }
                    break;
                case 'RequestDownload':
                    if (messageData && messageData.metadata_id && messageData.requester_id) {
                        initiateDownload(messageData.metadata_id, messageData.requester_id);
                    } else {
                        console.warn('RequestDownload missing required fields');
                    }
                    break;
                case 'Offer':
                    if (messageData && messageData.metadata_id && messageData.from_peer && messageData.sdp) {
                        handleOffer(messageData.metadata_id, messageData.from_peer, messageData.sdp);
                    } else {
                        console.warn('Offer missing required fields');
                    }
                    break;
                case 'Answer':
                    if (messageData && messageData.metadata_id && messageData.from_peer && messageData.sdp) {
                        handleAnswer(messageData.metadata_id, messageData.from_peer, messageData.sdp);
                    } else {
                        console.warn('Answer missing required fields');
                    }
                    break;
                case 'IceCandidate':
                    if (messageData && messageData.metadata_id && messageData.from_peer && messageData.candidate) {
                        handleIceCandidate(messageData.metadata_id, messageData.from_peer, messageData.candidate);
                    } else {
                        console.warn('IceCandidate missing required fields');
                    }
                    break;
                case 'TextMessage':
                    if (messageData && messageData.content && messageData.sender && messageData.id) {
                        displayTextMessage(messageData.content, messageData.sender, messageData.id);
                    } else {
                        console.warn('TextMessage missing required fields');
                    }
                    break;
                case 'PeerDisconnected':
                    if (messageData) {
                        delete peers[messageData];
                        updatePeerStatus();
                    } else {
                        console.warn('PeerDisconnected missing data field');
                    }
                    break;
                default:
                    console.warn('Unknown message type:', message.type, 'Full message:', message);
            }
        } catch (e) {
            // Only log JSON parsing errors if the message is not empty
            if (event.data && event.data.trim() !== '') {
                console.error('Error parsing WebSocket message:', e, 'Raw data:', event.data);
            } else {
                console.debug('Received empty WebSocket message, ignoring');
            }
        }
    };

ws.onopen = async () => {
    console.log('WebSocket connected');
    if (peerStatus) {
        peerStatus.textContent = '🟢 Connected to coordinator';
        peerStatus.style.color = '#00ff88';
    }
    updatePeerStatus();
};

ws.onclose = () => {
    console.log('WebSocket disconnected');
    if (peerStatus) {
        peerStatus.textContent = '🔴 Disconnected - Attempting to reconnect...';
        peerStatus.style.color = '#ff6b6b';
    }
    
    // Attempt to reconnect after 3 seconds
    setTimeout(() => {
        if (ws && ws.readyState === WebSocket.CLOSED) {
            console.log('Reloading page to reconnect...');
            window.location.reload();
        }
    }, 3000);
};

ws.onerror = (error) => {
    console.error('WebSocket error:', error);
    if (peerStatus) {
        peerStatus.textContent = '⚠️ Connection error';
        peerStatus.style.color = '#ffaa00';
    }
};

} // End of if (ws) block

function updateFileList(metadata) {
    // Cache metadata for later use
    metadataCache[metadata.id] = metadata;
    
    let row = fileList.querySelector(`[data-id="${metadata.id}"]`);
    if (!metadata.hosts || metadata.hosts.length === 0) {
        if (row) row.remove();
        delete metadataCache[metadata.id];
        return;
    }
    
    if (!row) {
        row = document.createElement('tr');
        row.dataset.id = metadata.id;
        
        const formatSize = (bytes) => {
            if (bytes === 0) return '0 B';
            const k = 1024;
            const sizes = ['B', 'KB', 'MB', 'GB'];
            const i = Math.floor(Math.log(bytes) / Math.log(k));
            return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
        };
        
        row.innerHTML = `
            <td>${metadata.name}</td>
            <td>${metadata.file_type}</td>
            <td>${formatSize(metadata.size)}</td>
            <td>
                ${metadata.content_type === 'TextMessage' ? 
                    `<button class="btn btn-sm btn-outline-info" onclick="viewTextMessage('${metadata.id}')">View</button>` :
                    `<button class="btn btn-sm btn-outline-success" onclick="downloadFile('${metadata.id}')">Download</button>`
                }
                <div class="progress mt-1" style="height: 5px; display: none;" data-progress="${metadata.id}">
                    <div class="progress-bar" role="progressbar" style="width: 0%"></div>
                </div>
            </td>
        `;
        fileList.insertBefore(row, fileList.firstChild);
    }
    
    peers[metadata.uploader_id] = true;
    if (metadata.hosts) {
        metadata.hosts.forEach((host) => (peers[host] = true));
    }
    updatePeerStatus();
}

// Text message functionality
if (sendTextBtn) {
    sendTextBtn.addEventListener('click', () => {
        sendTextMessage();
    });
}

if (textInput) {
    textInput.addEventListener('keypress', (e) => {
        if (e.key === 'Enter') {
            sendTextMessage();
        }
    });
}

function sendTextMessage() {
    const content = textInput.value.trim();
    if (!content) return;
    
    // Format message according to Rust enum structure
    const textMessage = {
        TextMessage: {
            id: generateUUID(),
            content: content,
            sender: sessionId
        }
    };
    
    // Also create metadata for the text message
    const metadata = {
        id: textMessage.TextMessage.id,
        name: `Message: ${content.substring(0, 30)}${content.length > 30 ? '...' : ''}`,
        file_type: 'text/plain',
        size: new Blob([content]).size,
        uploader_id: sessionId,
        hosts: [sessionId],
        content_type: 'TextMessage',
        text_content: content
    };
    
    const metadataMessage = {
        MetadataUpdate: metadata
    };
    
    if (ws && ws.readyState === WebSocket.OPEN) {
        try {
            ws.send(JSON.stringify(textMessage));
            ws.send(JSON.stringify(metadataMessage));
            
            if (textInput) {
                textInput.value = '';
            }
        } catch (error) {
            console.error('Failed to send text message:', error);
            alert('Failed to send message. Please check your connection.');
        }
    } else {
        console.error('WebSocket not connected');
        alert('Not connected to server. Please wait for connection.');
    }
}

function displayTextMessage(content, sender, messageId) {
    // Text messages are now handled through metadata updates
    console.log(`Text message from ${sender}: ${content}`);
}

function viewTextMessage(metadataId) {
    const metadata = getMetadataById(metadataId);
    if (metadata && metadata.text_content) {
        showTextModal(metadata.text_content, metadata.uploader_id);
    }
}

// Custom text modal functions
function showTextModal(content, sender) {
    const modal = document.getElementById('text-message-modal');
    const senderEl = document.getElementById('text-modal-sender');
    const contentEl = document.getElementById('text-modal-content');
    
    senderEl.textContent = `From: ${sender}`;
    contentEl.textContent = content;
    modal.style.display = 'flex';
}

function closeTextModal() {
    const modal = document.getElementById('text-message-modal');
    modal.style.display = 'none';
}

// Close modal when clicking outside
document.addEventListener('click', (event) => {
    const modal = document.getElementById('text-message-modal');
    if (event.target === modal) {
        closeTextModal();
    }
});

// Make functions globally accessible for onclick handlers
window.viewTextMessage = viewTextMessage;
window.closeTextModal = closeTextModal;

// Handle file upload
if (uploadFiles) {
    uploadFiles.addEventListener('change', async (event) => {
        const files = event.target.files;
        for (const file of files) {
            let metadata = {
                id: generateUUID(),
                name: file.name,
                file_type: file.type || 'application/octet-stream',
                size: file.size,
                uploader_id: sessionId,
                hosts: [sessionId],
                content_type: 'File',
                text_content: null,
            };
            
            if (ws && ws.readyState === WebSocket.OPEN) {
                try {
                    const metadataMessage = {
                        MetadataUpdate: metadata
                    };
                    ws.send(JSON.stringify(metadataMessage));
                    localStorage.setItem(`file_${metadata.id}`, JSON.stringify({ file }));
                    console.log(`File uploaded: ${file.name}`);
                } catch (error) {
                    console.error('Failed to send file metadata:', error);
                }
            }
        }
        event.target.value = '';
    });
}

// Handle folder upload
if (uploadFolders) {
    uploadFolders.addEventListener('change', async (event) => {
        const files = event.target.files;
        if (files.length > 0) {
            try {
                const folderName = files[0].webkitRelativePath.split('/')[0];
                const folderId = await uploadFolder(files, folderName);
                console.log(`Folder uploaded: ${folderName}`);
            } catch (error) {
                console.error('Failed to upload folder:', error);
            }
        }
        event.target.value = '';
    });
}

// Handle folder upload
async function uploadFolder(files, folderName) {
    if (!window.JSZip) {
        console.error('JSZip not available for folder compression');
        alert('Folder upload not supported - JSZip library not loaded');
        return;
    }

    const zip = new JSZip();
    for (const file of files) {
        const arrayBuffer = await file.arrayBuffer();
        zip.file(file.webkitRelativePath || file.name, arrayBuffer);
    }
    
    const blob = await zip.generateAsync({ type: 'blob' });
    const id = generateUUID();
    const metadata = {
        id,
        name: `${folderName}.zip`,
        file_type: 'application/zip',
        size: blob.size,
        uploader_id: sessionId,
        hosts: [sessionId],
        content_type: 'Folder',
        text_content: null,
    };
    
    if (ws && ws.readyState === WebSocket.OPEN) {
        try {
            const metadataMessage = {
                MetadataUpdate: metadata
            };
            ws.send(JSON.stringify(metadataMessage));
            localStorage.setItem(`file_${id}`, JSON.stringify({ blob }));
        } catch (error) {
            console.error('Failed to send folder metadata:', error);
        }
    }
    return id;
}

// Add drag and drop functionality
const uploadArea = document.querySelector('.upload-area');
if (uploadArea) {
    uploadArea.addEventListener('dragover', (e) => {
        e.preventDefault();
        uploadArea.style.borderColor = '#00d4ff';
        uploadArea.style.backgroundColor = 'rgba(0, 212, 255, 0.1)';
    });
    
    uploadArea.addEventListener('dragleave', (e) => {
        e.preventDefault();
        uploadArea.style.borderColor = '';
        uploadArea.style.backgroundColor = '';
    });
    
    uploadArea.addEventListener('drop', async (e) => {
        e.preventDefault();
        uploadArea.style.borderColor = '';
        uploadArea.style.backgroundColor = '';
        
        const items = Array.from(e.dataTransfer.items);
        
        for (const item of items) {
            if (item.kind === 'file') {
                const entry = item.webkitGetAsEntry();
                if (entry) {
                    if (entry.isDirectory) {
                        // Handle folder drop
                        const files = await getAllFilesFromDirectory(entry);
                        if (files.length > 0) {
                            try {
                                const folderId = await uploadFolder(files, entry.name);
                                console.log(`Folder dropped and uploaded: ${entry.name}`);
                            } catch (error) {
                                console.error('Failed to upload dropped folder:', error);
                            }
                        }
                    } else {
                        // Handle file drop
                        const file = item.getAsFile();
                        let metadata = {
                            id: generateUUID(),
                            name: file.name,
                            file_type: file.type || 'application/octet-stream',
                            size: file.size,
                            uploader_id: sessionId,
                            hosts: [sessionId],
                            content_type: 'File',
                            text_content: null,
                        };
                        
                        if (ws && ws.readyState === WebSocket.OPEN) {
                            try {
                                const metadataMessage = {
                                    MetadataUpdate: metadata
                                };
                                ws.send(JSON.stringify(metadataMessage));
                                localStorage.setItem(`file_${metadata.id}`, JSON.stringify({ file }));
                                console.log(`File dropped and uploaded: ${file.name}`);
                            } catch (error) {
                                console.error('Failed to send dropped file metadata:', error);
                            }
                        }
                    }
                }
            }
        }
    });
}

// Helper function to get all files from a directory entry
async function getAllFilesFromDirectory(dirEntry) {
    const files = [];
    
    async function traverseFileTree(item, path = '') {
        return new Promise((resolve, reject) => {
            if (item.isFile) {
                item.file((file) => {
                    file.webkitRelativePath = path + file.name;
                    files.push(file);
                    resolve();
                }, reject);
            } else if (item.isDirectory) {
                const dirReader = item.createReader();
                dirReader.readEntries(async (entries) => {
                    for (const entry of entries) {
                        await traverseFileTree(entry, path + item.name + '/');
                    }
                    resolve();
                }, reject);
            }
        });
    }
    
    await traverseFileTree(dirEntry);
    return files;
}

// Download file request
window.downloadFile = (metadataId) => {
    console.log('Download requested for metadata ID:', metadataId);
    
    if (!ws || ws.readyState !== WebSocket.OPEN) {
        console.error('WebSocket not connected');
        alert('Not connected to server. Please wait for connection.');
        return;
    }
    
    const metadata = getMetadataById(metadataId);
    if (!metadata) {
        console.error('Metadata not found for ID:', metadataId);
        alert('File metadata not found. Please refresh the page.');
        return;
    }
    
    if (!metadata.hosts || metadata.hosts.length === 0) {
        console.error('No hosts available for file:', metadataId);
        alert('No hosts available for this file.');
        return;
    }
    
    try {
        const downloadMessage = {
            RequestDownload: {
                metadata_id: metadataId,
                requester_id: sessionId
            }
        };
        console.log('Sending download request:', downloadMessage);
        ws.send(JSON.stringify(downloadMessage));
        
        // Show feedback to user
        const button = document.querySelector(`button[onclick="downloadFile('${metadataId}')"]`);
        if (button) {
            const originalText = button.textContent;
            button.textContent = 'Requesting...';
            button.disabled = true;
            
            // Reset button after 5 seconds
            setTimeout(() => {
                button.textContent = originalText;
                button.disabled = false;
            }, 5000);
        }
    } catch (error) {
        console.error('Failed to request download:', error);
        alert('Failed to request download. Please check your connection.');
    }
};

// Initiate download as a host
async function initiateDownload(metadataId, requesterId) {
    console.log('Initiating download for metadata:', metadataId, 'requester:', requesterId);
    
    const fileData = JSON.parse(localStorage.getItem(`file_${metadataId}`));
    if (!fileData) {
        console.error('File data not found in localStorage for metadata:', metadataId);
        return;
    }
    
    console.log('Found file data, creating peer connection...');
    const peer = new SimplePeer({ initiator: true, trickle: true });
    
    peer.on('signal', (data) => {
        console.log('Sending offer signal for metadata:', metadataId);
        const offerMessage = {
            Offer: {
                metadata_id: metadataId,
                from_peer: sessionId,
                sdp: JSON.stringify(data)
            }
        };
        ws.send(JSON.stringify(offerMessage));
    });
    
    peer.on('connect', async () => {
        console.log('Peer connected, starting file transfer for metadata:', metadataId);
        try {
            const file = fileData.file || new File([fileData.blob], fileData.name || 'download');
            const stream = file.stream();
            const reader = stream.getReader();
            let totalSent = 0;
            
            while (true) {
                const { done, value } = await reader.read();
                if (done) break;
                peer.send(value);
                totalSent += value.length;
                updateProgress(metadataId, totalSent, file.size);
            }
            
            console.log('File transfer complete, sending end signal');
            peer.send(JSON.stringify({ type: 'end' }));
        } catch (error) {
            console.error('Error during file transfer:', error);
        }
    });
    
    peer.on('error', (err) => {
        console.error('Peer error during download initiation:', err);
    });
    
    peers[requesterId] = peer;
}

// Handle WebRTC offer
async function handleOffer(metadataId, fromPeer, sdp) {
    console.log('Handling offer for metadata:', metadataId, 'from peer:', fromPeer);
    
    const peer = new SimplePeer({ initiator: false, trickle: true });
    
    peer.on('signal', (data) => {
        if (data.type === 'answer') {
            console.log('Sending answer signal for metadata:', metadataId);
            const answerMessage = {
                Answer: {
                    metadata_id: metadataId,
                    from_peer: sessionId,
                    sdp: JSON.stringify(data)
                }
            };
            ws.send(JSON.stringify(answerMessage));
        } else {
            console.log('Sending ICE candidate for metadata:', metadataId);
            const iceCandidateMessage = {
                IceCandidate: {
                    metadata_id: metadataId,
                    from_peer: sessionId,
                    candidate: JSON.stringify(data)
                }
            };
            ws.send(JSON.stringify(iceCandidateMessage));
        }
    });
    
    peer.on('connect', () => {
        console.log('Peer connected for download, metadata:', metadataId);
    });
    
    peer.on('data', async (data) => {
        if (typeof data === 'string' && data.startsWith('{')) {
            try {
                const msg = JSON.parse(data);
                if (msg.type === 'end') {
                    console.log('Download complete for metadata:', metadataId);
                    peer.destroy();
                    const metadataMessage = {
                        MetadataUpdate: { id: metadataId, hosts: [sessionId] }
                    };
                    ws.send(JSON.stringify(metadataMessage));
                }
            } catch (error) {
                console.error('Error parsing end message:', error);
            }
            return;
        }
        await saveChunk(metadataId, data);
    });
    
    try {
        peer.signal(JSON.parse(sdp));
    } catch (error) {
        console.error('Error parsing SDP signal:', error);
        return;
    }
    
    peer.on('error', (err) => {
        console.error('Peer error during offer handling:', err);
    });
    
    peers[fromPeer] = peer;
}

// Handle WebRTC answer
function handleAnswer(metadataId, fromPeer, sdp) {
    const peer = peers[fromPeer];
    if (peer) peer.signal(JSON.parse(sdp));
}

// Handle ICE candidate
function handleIceCandidate(metadataId, fromPeer, candidate) {
    const peer = peers[fromPeer];
    if (peer) peer.signal(JSON.parse(candidate));
}

// Enhanced file saving with better error handling
let fileHandles = {};
let totalReceived = {};

async function saveChunk(metadataId, chunk) {
    console.log('Saving chunk for metadata:', metadataId, 'size:', chunk.length);
    
    if (!fileHandles[metadataId]) {
        if (fileSystemAccessSupported) {
            try {
                const metadata = getMetadataById(metadataId);
                console.log('Opening file picker for:', metadata?.name || `download_${metadataId}`);
                fileHandles[metadataId] = await window.showSaveFilePicker({
                    suggestedName: metadata?.name || `download_${metadataId}`,
                    types: [{
                        description: 'All files',
                        accept: {'*/*': ['*']},
                    }],
                });
                console.log('File picker opened successfully');
            } catch (err) {
                if (err.name !== 'AbortError') {
                    console.warn('File System Access API failed, falling back to blob download:', err);
                    fileSystemAccessSupported = false;
                } else {
                    console.log('User cancelled file picker');
                    return;
                }
            }
        }
    }
    
    totalReceived[metadataId] = (totalReceived[metadataId] || 0) + chunk.length;
    
    if (fileSystemAccessSupported && fileHandles[metadataId]) {
        try {
            const writable = await fileHandles[metadataId].createWritable({
                keepExistingData: true
            });
            await writable.seek(totalReceived[metadataId] - chunk.length);
            await writable.write(chunk);
            await writable.close();
            console.log('Chunk written to file successfully');
        } catch (err) {
            console.error('Error writing to file:', err);
            // Fall back to blob method
            fileSystemAccessSupported = false;
            if (!downloadBuffers[metadataId]) {
                downloadBuffers[metadataId] = [];
            }
            downloadBuffers[metadataId].push(chunk);
        }
    } else {
        // Enhanced fallback with memory management
        if (!downloadBuffers[metadataId]) {
            downloadBuffers[metadataId] = [];
        }
        downloadBuffers[metadataId].push(chunk);
        
        const metadata = getMetadataById(metadataId);
        if (metadata && totalReceived[metadataId] >= metadata.size) {
            console.log('Download complete, creating blob for:', metadata.name);
            try {
                const blob = new Blob(downloadBuffers[metadataId]);
                const url = URL.createObjectURL(blob);
                const a = document.createElement('a');
                a.href = url;
                a.download = metadata.name || `download_${metadataId}`;
                document.body.appendChild(a);
                a.click();
                document.body.removeChild(a);
                URL.revokeObjectURL(url);
                
                // Clean up memory
                delete downloadBuffers[metadataId];
                console.log('Blob download completed successfully');
            } catch (error) {
                console.error('Error creating blob download:', error);
            }
        }
    }
    
    const metadata = getMetadataById(metadataId);
    if (metadata) {
        updateProgress(metadataId, totalReceived[metadataId], metadata.size);
    }
}

// Enhanced progress bar with speed calculation
function updateProgress(metadataId, current, total) {
    const progressContainer = document.querySelector(`[data-progress="${metadataId}"]`);
    if (progressContainer) {
        const progressBar = progressContainer.querySelector('.progress-bar');
        const percentage = Math.round((current / total) * 100);
        
        progressBar.style.width = `${percentage}%`;
        progressBar.textContent = `${percentage}%`;
        
        // Show progress container
        progressContainer.style.display = 'block';
        
        // Hide when complete
        if (percentage >= 100) {
            setTimeout(() => {
                progressContainer.style.display = 'none';
            }, 2000);
        }
    }
}