import SimplePeer from './libs/simple-peer.min.js';
import JSZip from './libs/jszip.min.js';
import QRCode from './libs/qrcode.min.js';

const sessionId = crypto.randomUUID();
const ws = new WebSocket(`ws://${window.location.hostname}:8080/ws`);
let peers = {}
let fileSystemAccessSupported = 'showSaveFilePicker' in window;

const fileList = document.getElementById('file-list');
const uploadInput = document.getElementById('upload-input');
const qrCodeContainer = document.getElementById('qrcode');
const peerStatus = document.getElementById('peer-status');

function generateQRCode() {
    const url = `http://${window.location.hostname}:8080`;
    new QRCode(qrCodeContainer, {
        text: url,
        width: 128,
        height: 128,
    });
}

function updatePeerStatus() {
    peerStatus.textContent = `Connected peers: ${Object.keys(peers).length}`;
}

ws.onmessage = (event) => {
    try {
        const message = JSON.parse(event.data);
        switch (message.type) {
            case 'MetadataUpdate':
                updateFileList(message.data);
                break;
            case 'RequestDownload':
                initiateDownload(message.metadata_id, message.requester_id);
                break;
            case 'Offer':
                handleOffer(message.metadata_id, message.from_peer, message.sdp);
                break;
            case 'Answer':
                handleAnswer(message.metadata_id, message.from_peer, message.sdp);
                break;
            case 'IceCandidate':
                handleIceCandidate(message.metadata_id, message.from_peer, message.candidate);
                break;
            case 'PeerDisconnected':
                delete peers[message.data];
                updatePeerStatus();
                break;
        }
    } catch (e) {
        console.error('Error parsing WebSocket message: ', e);
    }
};

ws.onopen = () => {
    console.log('WebSocket connected');
    generateQRCode();
    updatePeerStatus();
};

ws.onclose = () => {
    console.log('WebSocket disconnected');
    peerStatus.textContent = 'Disconnected from coordinator';
};

function updateFileList(metadata) {
    let row = fileList.querySelector(`[data-id="${metadata.id}"]`);
    if (!metadata.hosts || metadata.hosts.length === 0) {
        if (row) row.remove();
        return;
    }
    if (!row) {
        row = document.createElement('tr');
        row.dataset.id = metadata.id;
        row.innerHTML = `
            <td>${metadata.name}</td>
            <td>${metadata.file_type}</td>
            <td>${(metadata.size / 1024 / 1024).toFixed(2)} MB</td>
            <td><button onclick="downloadFile('${metadata.id}')">Download</button></td>
        `;
        fileList.appendChild(row);
    }
    peers[metadata.uploader_id] = true;
    metadata.hosts.forEach((host) => (peers[host] = true));
    updatePeerStatus();
}

// Handle file/folder upload
uploadInput.addEventListener('change', async (event) => {
    const files = event.target.files;
    for (const file of files) {
        let metadata = {
            id: crypto.randomUUID(),
            name: file.name,
            file_type: file.type || 'application/octet-stream',
            size: file.size,
            uploader_id: sessionId,
            hosts: new Set([sessionId]),
        };
        ws.send(JSON.stringify({ type: 'MetadataUpdate', data: metadata }));
        localStorage.setItem(`file_${metadata.id}`, JSON.stringify({ file }));
    }
    event.target.value = '';
});

// Handle folder zipping
async function zipFolder(files) {
    const zip = new JSZip();
    for (const file of files) {
        const arrayBuffer = await file.arrayBuffer();
        zip.file(file.webkitRelativePath || file.name, arrayBuffer);
    }
    const blob = await zip.generateAsync({ type: 'blob' });
    const id = crypto.randomUUID();
    const metadata = {
        id,
        name: 'folder.zip',
        file_type: 'application/zip',
        size: blob.size,
        uploader_id: sessionId,
        hosts: new Set([sessionId]),
    };
    ws.send(JSON.stringify({ type: 'MetadataUpdate', data: metadata }));
    localStorage.setItem(`file_${id}`, JSON.stringify({ blob }));
    return id;
}

// Download file request
window.downloadFile = (metadataId) => {
    ws.send(
        JSON.stringify({
            type: 'RequestDownload',
            metadata_id: metadataId,
            requester_id: sessionId,
        })
    );
};

// Initiate download as a host
async function initiateDownload(metadataId, requesterId) {
    const fileData = JSON.parse(localStorage.getItem(`file_${metadataId}`));
    if (!fileData) return;
    const peer = new SimplePeer({ initiator: true, trickle: true });
    peer.on('signal', (data) => {
        ws.send(
            JSON.stringify({
                type: 'Offer',
                metadata_id: metadataId,
                from_peer: sessionId,
                sdp: JSON.stringify(data),
            })
        );
    });
    peer.on('connect', async () => {
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
        peer.send(JSON.stringify({ type: 'end' }));
    });
    peer.on('error', (err) => console.error('Peer error: ', err));
    peers[requesterId] = peer;
}

// Handle WebRTC offer
async function handleOffer(metadataId, fromPeer, sdp) {
    const peer = new SimplePeer({ initiator: false, trickle: true });
    peer.on('signal', (data) => {
        ws.send(
            JSON.stringify({
                type: data.type === 'answer' ? 'Answer' : 'IceCandidate',
                metadata_id: metadataId,
                from_peer: sessionId,
                [data.type === 'answer' ? 'sdp' : 'candidate']: JSON.stringify(data),
            })
        );
    });
    peer.on('connect', () => console.log('Peer connected'));
    peer.on('data', async (data) => {
        if (typeof data === 'string' && data.startsWith('{')) {
            const msg = JSON.parse(data);
            if (msg.type === 'end') {
                peer.destroy();
                ws.send(
                    JSON.stringify({
                        type: 'MetadataUpdate',
                        data: { id: metadataId, hosts: new Set([sessionId]) },
                    })
                );
            }
            return;
        }
        await saveChunk(metadataId, data);
    });
    peer.signal(JSON.parse(sdp));
    peer.on('error', (err) => console.error('Peer error: ', err));
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

// Save file chunks on disk
let fileHandles = {};
let totalReceived = {};
async function saveChunk(metadataId, chunk) {
    if (!fileHandles[metadataId] && fileSystemAccessSupported) {
        fileHandles[metadataId] = await window.showSaveFilePicker({
            suggestedName: `download_${metadataId}`,
        });
    }
    totalReceived[metadataId] = (totalReceived[metadataId] || 0) + chunk.length;
    if (fileSystemAccessSupported) {
        const writable = await fileHandles[metadataId].createWritable({
            keepExistingData: true
        });
        await writable.seek(totalReceived[metadataId] - chunk.length);
        await writable.write(chunk);
        await writable.close();
    } else {
        const blob = new Blob([(localStorage.getItem(`chunks_${metadataId}`) || '') + chunk]);
        localStorage.setItem(`chunks_${metadataId}`, blob);
        if (totalReceived[metadataId] >= JSON.parse(localStorage.getItem(`file_${metadataId}`)).size) {
            const url = URL.createObjectURL(blob);
            const a = document.createElement('a');
            a.href = url;
            a.download = `download_${metadataId}`;
            a.click();
            URL.revokeObjectURL(url);
            localStorage.removeItem(`chunks_${metadataId}`);
        }
    }
    updateProgress(metadataId, totalReceived[metadataId], JSON.parse(localStorage.getItem(`file_${metadataId}`)).size);
}

// Update progress bar
function updateProgress(metadataId, current, total) {
    let progressBar = document.querySelector(`[data-progress="${metadataId}"]`);
    if (!progressBar) {
        progressBar = document.createElement('progress');
        progressBar.dataset.progress = metadataId;
        progressBar.max = 100;
        const row = fileList.querySelector(`[data-id="${metadataId}"]`);
        if (row) row.appendChild(progressBar);
    }
    progressBar.value = (current / total) * 100;
}