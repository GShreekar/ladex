// ============================================================================
// LADEX — Feature Flags
//
// Toggle these to enable/disable phases of the mesh migration without
// removing the old code mid-migration.  Each flag defaults to the safe/stable
// value.  Flip to true only after the corresponding phase is validated.
//
//   MESH_MODE              → Phases 1-7  (symmetric node mesh, decentralized signaling)
//   DISK_STREAMING_RECEIVE → Phase 8     (FSAA receive path, O(1) memory)
//   PARALLEL_DATACHANNELS  → Phase 9     (multiple DCs per transfer — deferred)
// ============================================================================

export const FEATURES = {
    // Phase 1-7: symmetric node mesh instead of central server.
    // Keep false until Phase 7 is fully validated on real hardware.
    MESH_MODE: false,

    // Phase 8: stream received chunks straight to disk via the File System
    // Access API (showSaveFilePicker + createWritable).  Defaults true
    // because it fixes a real bug in the current single-server version too
    // and has no dependency on the mesh work.
    DISK_STREAMING_RECEIVE: true,

    // Phase 9: open 2-4 DataChannels per transfer and round-robin chunks.
    // Deferred — only enable after single-channel profiling shows it is
    // the bottleneck.  Requires DISK_STREAMING_RECEIVE=true (random-access
    // writes via { type:"seek", position } are needed for out-of-order chunks).
    PARALLEL_DATACHANNELS: false,
};
