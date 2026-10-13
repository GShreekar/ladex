//! One-time invitations: a long token for the QR code and a short code to type, either of which admits one guest.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use data_encoding::BASE32_NOPAD;
use rand::RngCore;

use super::Permission;
use crate::auth::secrets_match;

pub const INVITE_LIFETIME: Duration = Duration::from_secs(10 * 60);
const MAX_LIVE_INVITES: usize = 16;
const TOKEN_BYTES: usize = 16;
// 5 bytes make exactly 8 base32 characters (40 bits).
const CODE_BYTES: usize = 5;

/// A newly issued invitation, shown to the host once.
#[derive(Debug, Clone)]
pub struct IssuedInvite {
    pub token: String,
    pub code: String,
    pub permission: Permission,
    pub expires_in: Duration,
}

struct Record {
    token: String,
    code: String,
    permission: Permission,
    expires_at: Instant,
}

pub struct Invites {
    lifetime: Duration,
    live: Mutex<Vec<Record>>,
}

impl Default for Invites {
    fn default() -> Self {
        Self::new()
    }
}

impl Invites {
    pub fn new() -> Self {
        Self::with_lifetime(INVITE_LIFETIME)
    }

    fn with_lifetime(lifetime: Duration) -> Self {
        Self { lifetime, live: Mutex::new(Vec::new()) }
    }

    pub fn issue(&self, permission: Permission) -> IssuedInvite {
        let mut token_bytes = [0u8; TOKEN_BYTES];
        let mut code_bytes = [0u8; CODE_BYTES];
        rand::thread_rng().fill_bytes(&mut token_bytes);
        rand::thread_rng().fill_bytes(&mut code_bytes);
        let (token, code) = (hex::encode(token_bytes), BASE32_NOPAD.encode(&code_bytes));

        let mut live = self.live.lock().unwrap();
        let now = Instant::now();
        live.retain(|r| r.expires_at > now);
        if live.len() >= MAX_LIVE_INVITES {
            live.remove(0);
        }
        live.push(Record { token: token.clone(), code: code.clone(), permission, expires_at: now + self.lifetime });
        IssuedInvite { token, code, permission, expires_in: self.lifetime }
    }

    /// Spends the invitation this token or code belongs to, and says what permission it carried.
    pub fn redeem(&self, secret: &str) -> Option<Permission> {
        let as_token = secret.trim().to_ascii_lowercase();
        let as_code: String = secret.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_uppercase();

        let mut live = self.live.lock().unwrap();
        let now = Instant::now();
        live.retain(|r| r.expires_at > now);
        // Every live invitation is compared, so how long this takes does not say which one was close.
        let mut found = None;
        for (index, record) in live.iter().enumerate() {
            let is_match = secrets_match(&record.token, &as_token) | secrets_match(&record.code, &as_code);
            if is_match {
                found = Some(index);
            }
        }
        found.map(|index| live.remove(index).permission)
    }
}

/// A code as it is read out: `ABCD-EFGH`.
pub fn display_code(code: &str) -> String {
    match code.split_at_checked(4) {
        Some((first, second)) => format!("{first}-{second}"),
        None => code.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_admits_one_guest_only_once() {
        let invites = Invites::new();
        let invite = invites.issue(Permission::Upload);
        assert_eq!(invites.redeem(&invite.token), Some(Permission::Upload));
        assert_eq!(invites.redeem(&invite.token), None);
    }

    #[test]
    fn spending_the_code_also_spends_the_token() {
        let invites = Invites::new();
        let invite = invites.issue(Permission::ReadOnly);
        assert_eq!(invites.redeem(&invite.code), Some(Permission::ReadOnly));
        assert_eq!(invites.redeem(&invite.token), None);
    }

    #[test]
    fn a_typed_code_may_be_lowercase_and_have_a_dash() {
        let invites = Invites::new();
        let invite = invites.issue(Permission::UploadWithApproval);
        let typed = display_code(&invite.code).to_lowercase();
        assert_eq!(invites.redeem(&typed), Some(Permission::UploadWithApproval));
    }

    #[test]
    fn a_wrong_secret_spends_nothing() {
        let invites = Invites::new();
        let invite = invites.issue(Permission::ReadOnly);
        assert_eq!(invites.redeem("AAAAAAAA"), None);
        assert_eq!(invites.redeem(""), None);
        assert_eq!(invites.redeem(&invite.token), Some(Permission::ReadOnly));
    }

    #[test]
    fn an_expired_invite_is_refused() {
        let invites = Invites::with_lifetime(Duration::ZERO);
        let invite = invites.issue(Permission::Upload);
        assert_eq!(invites.redeem(&invite.token), None);
    }

    #[test]
    fn invites_are_128_bit_tokens_and_8_character_codes() {
        let invite = Invites::new().issue(Permission::ReadOnly);
        assert_eq!(invite.token.len(), 32);
        assert_eq!(invite.code.len(), 8);
        assert!(invite.code.bytes().all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b)));
    }

    #[test]
    fn the_oldest_invite_makes_room_when_too_many_are_open() {
        let invites = Invites::new();
        let first = invites.issue(Permission::ReadOnly);
        let others: Vec<_> = (0..MAX_LIVE_INVITES).map(|_| invites.issue(Permission::ReadOnly)).collect();
        assert_eq!(invites.redeem(&first.token), None);
        assert_eq!(invites.redeem(&others[0].token), Some(Permission::ReadOnly));
    }
}
