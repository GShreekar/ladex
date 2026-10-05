//! Passphrase helpers for the browser login, and the TLS fingerprint the mesh handshake binds to.

use rand::Rng;
use ring::digest;
use subtle::ConstantTimeEq;

// No look-alike characters (i, l, o, 0, 1): these get typed in from a terminal.
const PASSPHRASE_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

pub fn tls_fingerprint(cert_der: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA256, cert_der).as_ref().to_vec()
}

/// Constant-time comparison of two secrets; both are hashed first so the length doesn't leak either.
pub fn secrets_match(expected: &str, given: &str) -> bool {
    let expected = digest::digest(&digest::SHA256, expected.as_bytes());
    let given = digest::digest(&digest::SHA256, given.as_bytes());
    expected.as_ref().ct_eq(given.as_ref()).into()
}

/// 12 characters from a 31-symbol alphabet (~59 bits), grouped for readability.
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
