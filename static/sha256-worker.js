// ============================================================================
// LADEX — Phase 11.3: SHA-256 integrity checksum Web Worker
//
// This worker computes SHA-256 digests off the main thread using SubtleCrypto
// so large-file hashing never blocks the UI.
//
// MESSAGE PROTOCOL (main → worker):
//   { cmd: 'hash_file', fileId, buffer: ArrayBuffer }
//       Hashes a complete file buffer (for files already in RAM — small files).
//       Posts back: { cmd: 'hash_result', fileId, sha256: '<hex>' }
//
//   { cmd: 'hash_blob', fileId, blob: File|Blob }
//       Not used via postMessage (blobs can't transfer); see hash_file instead.
//
// For large files the worker receives a pre-sliced ArrayBuffer transferred with
// the Transferable mechanism (zero-copy):
//   postMessage({ cmd: 'hash_file', fileId, buffer }, [buffer])
//
// ERROR:
//   { cmd: 'hash_error', fileId, error: '<message>' }
// ============================================================================

self.addEventListener('message', async (event) => {
    const { cmd, fileId, buffer } = event.data;

    if (cmd !== 'hash_file') {
        self.postMessage({ cmd: 'hash_error', fileId, error: `Unknown cmd: ${cmd}` });
        return;
    }

    try {
        // SubtleCrypto.digest is available in all workers
        const hashBuffer = await crypto.subtle.digest('SHA-256', buffer);
        const hex = Array.from(new Uint8Array(hashBuffer))
            .map(b => b.toString(16).padStart(2, '0'))
            .join('');
        self.postMessage({ cmd: 'hash_result', fileId, sha256: hex });
    } catch (err) {
        self.postMessage({ cmd: 'hash_error', fileId, error: String(err) });
    }
});
