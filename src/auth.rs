// ============================================================================
// LADEX — Phase 7: Shared-Passphrase Mesh Auth
//
// Derives a deterministic 32-byte hash from a plaintext passphrase using
// PBKDF2-HMAC-SHA256 with a fixed, publicly-known salt and 10 000 iterations.
//
// Design notes (from ROADMAP.md §7):
//
//   • This is NOT protecting a user password against an attacker who has
//     stolen a credential database — it is a shared mesh secret, equivalent
//     to a WPA2 PSK.  The goal is (a) to make rainbow-table attacks on the
//     announce/hello hash impractical, and (b) to produce a short,
//     deterministic, hex-encodable value that all nodes on the mesh can
//     compare without transmitting the passphrase itself.
//
//   • Salt is publicly known ("ladex-v1-mesh-salt").  This is intentional:
//     changing it is a breaking wire-format change (increment PROTOCOL_VERSION).
//     The salt's purpose is domain-separation + rainbow-table resistance,
//     not secrecy.
//
//   • 10 000 iterations is the minimum NIST recommends for PBKDF2-SHA256
//     for low-value shared secrets.  It's fast enough that the 2-second
//     announce window is not a concern (≪1 ms on any modern CPU).
//
//   • Three-check enforcement (ROADMAP.md §7.2):
//     1. Discovery pre-filter: skip peer if announce.passphrase_hash != ours.
//     2. Hello/HelloAck handshake: reject if Hello.passphrase_hash != ours.
//     3. No-passphrase policy: empty string is treated as a distinct passphrase,
//        so secured and unsecured meshes never accidentally mix.
// ============================================================================

use ring::pbkdf2;
use std::num::NonZeroU32;

/// Fixed public salt — domain-separation only, NOT a secret.
/// Changing this is a wire-format breaking change → bump PROTOCOL_VERSION.
const SALT: &[u8] = b"ladex-v1-mesh-salt";

/// Number of PBKDF2 iterations.  10 000 per NIST SP 800-132 minimum
/// for shared secrets.
const ITERATIONS: u32 = 10_000;

/// Output length: 256 bits (SHA-256 output size).
const OUTPUT_LEN: usize = 32;

/// Derive the mesh passphrase hash from a plaintext passphrase.
///
/// Returns a lowercase hex-encoded 64-character string (32 bytes × 2 hex digits).
///
/// When `passphrase` is `None` or empty, returns `""` (the no-passphrase token).
/// This is intentional: empty-passphrase nodes only connect to other
/// empty-passphrase nodes.
pub fn derive_hash(passphrase: Option<&str>) -> String {
    let p = match passphrase {
        None => return String::new(),
        Some(s) if s.is_empty() => return String::new(),
        Some(s) => s,
    };

    let iterations = NonZeroU32::new(ITERATIONS).unwrap();
    let mut out = [0u8; OUTPUT_LEN];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        SALT,
        p.as_bytes(),
        &mut out,
    );
    hex::encode(out)
}

/// Compare two passphrase hashes for mesh admission.
///
/// Returns `true` iff both hashes are equal (including both being empty).
/// A non-empty hash never matches an empty hash — this enforces the
/// "secured and unsecured meshes never mix" rule.
pub fn hashes_match(ours: &str, theirs: &str) -> bool {
    ours == theirs
}
