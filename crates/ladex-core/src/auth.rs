// Passphrase handling for both entry points: the browser login and the
// node-to-node mesh handshake.
//
// Mesh handshake (see mesh.rs): the two nodes run SPAKE2 with the passphrase as
// the password, then each proves it derived the same key with an HMAC over a
// transcript. The passphrase and anything derived from it never cross the wire,
// and a passive or active attacker gets at most one passphrase guess per
// connection, never an offline attack.
//
// The transcript includes a fingerprint of the server's TLS certificate as the
// client saw it. A man in the middle terminating TLS with their own certificate
// makes the two sides' transcripts differ, so the proofs fail.

use rand::Rng;
use ring::{digest, hmac};
use subtle::ConstantTimeEq;
use spake2::{Ed25519Group, Identity, Password, Spake2};

// Fixed SPAKE2 identities; the real node ids go into the transcript instead,
// because the dialing side doesn't know the server's node id up front.
const CLIENT_IDENTITY: &[u8] = b"ladex-v2 mesh client";
const SERVER_IDENTITY: &[u8] = b"ladex-v2 mesh server";

// Ed25519 SPAKE2 messages are 33 bytes; anything much larger isn't one.
const MAX_PAKE_MESSAGE_LEN: usize = 64;

// No look-alike characters (i, l, o, 0, 1): these get typed in from a terminal.
const PASSPHRASE_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

#[derive(Clone, Copy)]
pub enum Role {
    Client,
    Server,
}

impl Role {
    fn label(self) -> &'static [u8] {
        match self {
            Role::Client => b"client",
            Role::Server => b"server",
        }
    }
}

pub struct Pake {
    spake: Spake2<Ed25519Group>,
    message: Vec<u8>,
}

impl Pake {
    pub fn start(role: Role, passphrase: &str) -> Self {
        let password = Password::new(passphrase.as_bytes());
        let client = Identity::new(CLIENT_IDENTITY);
        let server = Identity::new(SERVER_IDENTITY);
        let (spake, message) = match role {
            Role::Client => Spake2::<Ed25519Group>::start_a(&password, &client, &server),
            Role::Server => Spake2::<Ed25519Group>::start_b(&password, &client, &server),
        };
        Self { spake, message }
    }

    pub fn message(&self) -> &[u8] {
        &self.message
    }

    // None if the peer's message is malformed.
    pub fn finish(self, peer_message: &[u8]) -> Option<SessionKey> {
        if peer_message.len() > MAX_PAKE_MESSAGE_LEN {
            return None;
        }
        let key = self.spake.finish(peer_message).ok()?;
        Some(SessionKey(hmac::Key::new(hmac::HMAC_SHA256, &key)))
    }
}

pub struct Transcript(Vec<u8>);

impl Transcript {
    // `tls_binding` is the server certificate's fingerprint, or empty when the
    // connection isn't TLS.
    pub fn new(
        client_node_id: &str,
        server_node_id: &str,
        client_pake: &[u8],
        server_pake: &[u8],
        tls_binding: &[u8],
    ) -> Self {
        let mut bytes = b"ladex-v2 mesh transcript".to_vec();
        for field in [client_node_id.as_bytes(), server_node_id.as_bytes(), client_pake, server_pake, tls_binding] {
            bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
            bytes.extend_from_slice(field);
        }
        Self(bytes)
    }
}

pub struct SessionKey(hmac::Key);

impl SessionKey {
    pub fn prove(&self, role: Role, transcript: &Transcript) -> Vec<u8> {
        hmac::sign(&self.0, &proof_input(role, transcript)).as_ref().to_vec()
    }

    // Constant-time check of the proof `role` sent.
    pub fn verify(&self, role: Role, transcript: &Transcript, proof: &[u8]) -> bool {
        hmac::verify(&self.0, &proof_input(role, transcript), proof).is_ok()
    }
}

fn proof_input(role: Role, transcript: &Transcript) -> Vec<u8> {
    let mut input = role.label().to_vec();
    input.push(b':');
    input.extend_from_slice(&transcript.0);
    input
}

pub fn tls_fingerprint(cert_der: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA256, cert_der).as_ref().to_vec()
}

// Constant-time comparison for the browser login passphrase and session cookie.
// Both sides are hashed first so the comparison doesn't leak the length either.
pub fn secrets_match(expected: &str, given: &str) -> bool {
    let expected = digest::digest(&digest::SHA256, expected.as_bytes());
    let given = digest::digest(&digest::SHA256, given.as_bytes());
    expected.as_ref().ct_eq(given.as_ref()).into()
}

// 12 characters from a 31-symbol alphabet (~59 bits), grouped for readability.
pub fn generate_passphrase() -> String {
    let mut rng = rand::thread_rng();
    let mut passphrase = String::with_capacity(14);
    for i in 0..12 {
        if i > 0 && i % 4 == 0 {
            passphrase.push('-');
        }
        passphrase.push(PASSPHRASE_ALPHABET[rng.gen_range(0..PASSPHRASE_ALPHABET.len())] as char);
    }
    passphrase
}

pub fn weakness(passphrase: &str) -> Option<&'static str> {
    if passphrase.chars().count() < 8 {
        Some("it is shorter than 8 characters")
    } else if passphrase.chars().all(|c| c.is_ascii_digit()) {
        Some("it is digits only")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Runs the handshake the way mesh.rs does and returns whether each side
    // accepted the other's proof.
    fn handshake(client_pw: &str, server_pw: &str, client_tls: &[u8], server_tls: &[u8]) -> (bool, bool) {
        let client = Pake::start(Role::Client, client_pw);
        let server = Pake::start(Role::Server, server_pw);
        let (client_msg, server_msg) = (client.message().to_vec(), server.message().to_vec());

        let server_key = server.finish(&client_msg).unwrap();
        let server_transcript = Transcript::new("client-node", "server-node", &client_msg, &server_msg, server_tls);
        let server_proof = server_key.prove(Role::Server, &server_transcript);

        let client_key = client.finish(&server_msg).unwrap();
        let client_transcript = Transcript::new("client-node", "server-node", &client_msg, &server_msg, client_tls);
        let client_accepts_server = client_key.verify(Role::Server, &client_transcript, &server_proof);

        let client_proof = client_key.prove(Role::Client, &client_transcript);
        let server_accepts_client = server_key.verify(Role::Client, &server_transcript, &client_proof);
        (client_accepts_server, server_accepts_client)
    }

    #[test]
    fn matching_passphrase_and_tls_binding_succeeds() {
        assert_eq!(handshake("correct horse", "correct horse", b"cert-a", b"cert-a"), (true, true));
    }

    #[test]
    fn wrong_passphrase_fails_both_ways() {
        assert_eq!(handshake("correct horse", "battery staple", b"cert-a", b"cert-a"), (false, false));
    }

    #[test]
    fn open_mesh_with_empty_passphrase_works() {
        assert_eq!(handshake("", "", b"cert-a", b"cert-a"), (true, true));
    }

    #[test]
    fn man_in_the_middle_with_a_different_certificate_fails() {
        // The attacker relays the SPAKE2 messages untouched but terminates TLS
        // with their own certificate, so the client sees a different one.
        assert_eq!(handshake("correct horse", "correct horse", b"attacker-cert", b"real-cert"), (false, false));
    }

    #[test]
    fn proof_from_the_wrong_role_is_rejected() {
        let client = Pake::start(Role::Client, "pw");
        let server = Pake::start(Role::Server, "pw");
        let (client_msg, server_msg) = (client.message().to_vec(), server.message().to_vec());
        let key = client.finish(&server_msg).unwrap();
        let transcript = Transcript::new("c", "s", &client_msg, &server_msg, b"");
        // A reflected client proof must not pass as a server proof.
        let client_proof = key.prove(Role::Client, &transcript);
        assert!(!key.verify(Role::Server, &transcript, &client_proof));
    }

    #[test]
    fn a_recorded_proof_is_useless_in_a_new_handshake() {
        let handshake_transcript = |client: &Pake, server: &Pake| {
            Transcript::new("c", "s", client.message(), server.message(), b"")
        };
        let (client, server) = (Pake::start(Role::Client, "pw"), Pake::start(Role::Server, "pw"));
        let recorded_proof = {
            let transcript = handshake_transcript(&client, &server);
            let key = server.finish(client.message()).unwrap();
            key.prove(Role::Server, &transcript)
        };

        let (new_client, new_server) = (Pake::start(Role::Client, "pw"), Pake::start(Role::Server, "pw"));
        let transcript = handshake_transcript(&new_client, &new_server);
        let new_key = new_client.finish(new_server.message()).unwrap();
        assert!(!new_key.verify(Role::Server, &transcript, &recorded_proof));
    }

    #[test]
    fn an_eavesdropper_who_only_saw_the_wire_cannot_prove_knowledge() {
        let client = Pake::start(Role::Client, "pw");
        let server = Pake::start(Role::Server, "pw");
        let transcript = Transcript::new("c", "s", client.message(), server.message(), b"");
        let real_proof = server.finish(client.message()).unwrap().prove(Role::Server, &transcript);

        // The eavesdropper saw both PAKE messages but guesses the wrong passphrase.
        let guesser = Pake::start(Role::Server, "wrong guess");
        let guessed_proof = guesser.finish(client.message()).unwrap().prove(Role::Server, &transcript);
        assert_ne!(real_proof, guessed_proof);
    }

    #[test]
    fn malformed_pake_messages_are_rejected() {
        assert!(Pake::start(Role::Server, "pw").finish(&[]).is_none());
        assert!(Pake::start(Role::Server, "pw").finish(&[0u8; 200]).is_none());
        assert!(Pake::start(Role::Server, "pw").finish(&[1u8; 33]).is_none());
    }

    #[test]
    fn transcript_fields_cannot_be_shifted_between_each_other() {
        let a = Transcript::new("ab", "c", b"", b"", b"");
        let b = Transcript::new("a", "bc", b"", b"", b"");
        assert_ne!(a.0, b.0);
    }

    #[test]
    fn a_secret_that_only_extends_the_real_one_does_not_match() {
        assert!(!secrets_match("abcd-efgh", "abcd-efgh-extra"));
        assert!(!secrets_match("abcd-efgh-extra", "abcd-efgh"));
    }

    #[test]
    fn secret_comparison_works_on_non_ascii_text() {
        assert!(secrets_match("pässwörd-ключ", "pässwörd-ключ"));
        assert!(!secrets_match("pässwörd-ключ", "passwörd-ключ"));
    }

    #[test]
    fn the_same_certificate_always_gets_the_same_fingerprint() {
        assert_eq!(tls_fingerprint(b"cert-a"), tls_fingerprint(b"cert-a"));
    }

    #[test]
    fn different_certificates_get_different_fingerprints() {
        assert_ne!(tls_fingerprint(b"cert-a"), tls_fingerprint(b"cert-b"));
        assert_eq!(tls_fingerprint(b"cert-a").len(), 32);
    }

    #[test]
    fn a_proof_of_the_wrong_length_is_rejected() {
        let key = Pake::start(Role::Client, "pw").finish(Pake::start(Role::Server, "pw").message()).unwrap();
        let transcript = Transcript::new("c", "s", b"x", b"y", b"");
        assert!(!key.verify(Role::Client, &transcript, &[]));
        assert!(!key.verify(Role::Client, &transcript, &[0u8; 31]));
        assert!(!key.verify(Role::Client, &transcript, &[0u8; 64]));
    }

    #[test]
    fn a_proof_for_one_transcript_fails_on_another() {
        let (client, server) = (Pake::start(Role::Client, "pw"), Pake::start(Role::Server, "pw"));
        let key = client.finish(server.message()).unwrap();
        let proof = key.prove(Role::Client, &Transcript::new("c", "s", b"x", b"y", b""));
        assert!(!key.verify(Role::Client, &Transcript::new("c", "other", b"x", b"y", b""), &proof));
        assert!(!key.verify(Role::Client, &Transcript::new("c", "s", b"x", b"y", b"cert"), &proof));
    }

    #[test]
    fn a_node_id_cannot_be_swapped_without_breaking_the_proof() {
        let (client_msg, server_msg) = (b"x".as_slice(), b"y".as_slice());
        let honest = Transcript::new("node_a", "node_b", client_msg, server_msg, b"");
        let impersonated = Transcript::new("node_evil", "node_b", client_msg, server_msg, b"");
        assert_ne!(honest.0, impersonated.0);
    }

    #[test]
    fn the_two_roles_have_different_labels() {
        assert_ne!(Role::Client.label(), Role::Server.label());
    }

    #[test]
    fn passphrase_length_is_counted_in_characters_not_bytes() {
        assert!(weakness("ééééééé").is_some());
        assert!(weakness("éééééééé").is_none());
    }

    #[test]
    fn a_passphrase_of_exactly_eight_letters_is_acceptable() {
        assert!(weakness("abcdefgh").is_none());
    }

    #[test]
    fn digits_with_a_separator_are_not_flagged_as_digits_only() {
        assert!(weakness("1234-5678").is_none());
    }

    #[test]
    fn secret_comparison() {
        assert!(secrets_match("abcd-efgh", "abcd-efgh"));
        assert!(!secrets_match("abcd-efgh", "abcd-efgi"));
        assert!(!secrets_match("abcd-efgh", ""));
    }

    #[test]
    fn generated_passphrases_are_well_formed_and_distinct() {
        let a = generate_passphrase();
        let b = generate_passphrase();
        assert_eq!(a.len(), 14);
        assert!(a.split('-').all(|g| g.len() == 4 && g.bytes().all(|c| PASSPHRASE_ALPHABET.contains(&c))));
        assert_ne!(a, b);
        assert!(weakness(&a).is_none());
    }

    #[test]
    fn weak_passphrases_are_flagged() {
        assert!(weakness("123456").is_some());
        assert!(weakness("12345678").is_some());
        assert!(weakness("short").is_some());
        assert!(weakness("a decent passphrase").is_none());
    }
}
