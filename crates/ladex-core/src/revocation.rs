//! Signed revocations: one node's word that a key is no longer trusted, passed from node to node across the mesh.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::identity::{self, Identity};
use crate::trust::{hex_public_key, Standing, TrustStore, TrustedNode};
use crate::types::NodeId;

const SIGNATURE_LABEL: &[u8] = b"ladex-revoke";
/// The most revocations one mesh message may carry.
pub const MAX_PER_MESSAGE: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revocation {
    /// The key that is no longer trusted.
    #[serde(with = "hex_public_key")]
    pub public_key: VerifyingKey,
    /// The node that revoked it, and signed this.
    #[serde(with = "hex_public_key")]
    pub issuer: VerifyingKey,
    /// When the issuer revoked it, by its own clock; for display only.
    pub issued_at_ms: u64,
    #[serde(with = "hex_signature")]
    signature: Signature,
}

impl Revocation {
    /// Revokes `public_key` in this node's name.
    pub fn issue(identity: &Identity, public_key: &VerifyingKey, now_ms: u64) -> Self {
        let issuer = identity.public_key();
        let signature = identity.sign(&signed_bytes(public_key, &issuer, now_ms));
        Self { public_key: *public_key, issuer, issued_at_ms: now_ms, signature }
    }

    /// The id of the revoked node.
    pub fn node_id(&self) -> NodeId {
        identity::node_id_for(&self.public_key)
    }

    pub fn issuer_id(&self) -> NodeId {
        identity::node_id_for(&self.issuer)
    }

    /// Whether the issuer really signed exactly this.
    pub fn is_signed(&self) -> bool {
        identity::verify(&self.issuer, &signed_bytes(&self.public_key, &self.issuer, self.issued_at_ms), &self.signature)
    }
}

fn signed_bytes(public_key: &VerifyingKey, issuer: &VerifyingKey, issued_at_ms: u64) -> Vec<u8> {
    [SIGNATURE_LABEL, b":", public_key.as_bytes(), issuer.as_bytes(), &issued_at_ms.to_be_bytes()].concat()
}

/// What became of a revocation another node sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Applied,
    AlreadyKnown,
    Refused(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    BadSignature,
    /// A node does not revoke itself on another's word; the others drop it instead.
    OfThisNode,
    /// Neither the issuer nor the node that passed it on is trusted here.
    NoAuthority,
    /// A node trusted only by passphrase cannot revoke one this node paired with.
    OutranksAuthority,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::BadSignature => "its signature is invalid",
            Refusal::OfThisNode => "it revokes this node",
            Refusal::NoAuthority => "neither its issuer nor the node that sent it is trusted here",
            Refusal::OutranksAuthority => "it revokes a paired node on the word of one trusted only by passphrase",
        })
    }
}

/// Checks a revocation that `sender` passed on and, if this node should honour it, keeps it.
pub fn receive(trust: &TrustStore, us: &Identity, sender: &str, revocation: &Revocation) -> anyhow::Result<Verdict> {
    if !revocation.is_signed() {
        return Ok(Verdict::Refused(Refusal::BadSignature));
    }
    if revocation.public_key == us.public_key() {
        return Ok(Verdict::Refused(Refusal::OfThisNode));
    }
    if trust.revocation(&revocation.node_id())?.is_some() {
        return Ok(Verdict::AlreadyKnown);
    }
    if revocation.issuer != us.public_key() {
        if let Some(refusal) = refusal(trust, sender, revocation)? {
            return Ok(Verdict::Refused(refusal));
        }
    }
    let verdict = if trust.revoke(revocation)? { Verdict::Applied } else { Verdict::AlreadyKnown };
    Ok(verdict)
}

// The issuer speaks for itself if trusted here; otherwise the trusted node that passed it on vouches for it.
fn refusal(trust: &TrustStore, sender: &str, revocation: &Revocation) -> anyhow::Result<Option<Refusal>> {
    let authority = match trust.standing(&revocation.issuer)? {
        Standing::Trusted(issuer) => *issuer,
        Standing::Revoked => return Ok(Some(Refusal::NoAuthority)),
        Standing::Unknown => match trusted(trust, sender)? {
            Some(sender) => sender,
            None => return Ok(Some(Refusal::NoAuthority)),
        },
    };
    if let Standing::Trusted(target) = trust.standing(&revocation.public_key)? {
        if target.trusted_via > authority.trusted_via {
            return Ok(Some(Refusal::OutranksAuthority));
        }
    }
    Ok(None)
}

fn trusted(trust: &TrustStore, node_id: &str) -> anyhow::Result<Option<TrustedNode>> {
    Ok(trust.get(node_id)?.filter(|node| !node.revoked))
}

mod hex_signature {
    use ed25519_dalek::Signature;
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(signature: &Signature, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(signature.to_bytes()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Signature, D::Error> {
        let bytes = hex::decode(String::deserialize(deserializer)?).map_err(D::Error::custom)?;
        Signature::from_slice(&bytes).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::TrustedVia;

    struct Node {
        _dir: tempfile::TempDir,
        identity: Identity,
        trust: TrustStore,
    }

    impl Node {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let trust = TrustStore::open(dir.path()).unwrap();
            Node { _dir: dir, identity: Identity::generate(), trust }
        }

        fn trusts(&self, other: &Identity, via: TrustedVia) {
            self.trust.trust(&other.public_key(), "other", via, 100).unwrap();
        }

        fn receive(&self, sender: &Identity, revocation: &Revocation) -> Verdict {
            receive(&self.trust, &self.identity, sender.node_id(), revocation).unwrap()
        }

        fn is_revoked(&self, other: &Identity) -> bool {
            self.trust.standing(&other.public_key()).unwrap() == Standing::Revoked
        }
    }

    fn revoke(issuer: &Identity, target: &Identity) -> Revocation {
        Revocation::issue(issuer, &target.public_key(), 1_000)
    }

    #[test]
    fn a_revocation_names_the_revoked_node_and_its_issuer() {
        let (issuer, target) = (Identity::generate(), Identity::generate());
        let revocation = revoke(&issuer, &target);
        assert_eq!((revocation.node_id(), revocation.issuer_id()), (target.node_id().to_string(), issuer.node_id().to_string()));
        assert!(revocation.is_signed());
    }

    #[test]
    fn changing_any_signed_field_breaks_the_signature() {
        let (issuer, target) = (Identity::generate(), Identity::generate());
        let revocation = revoke(&issuer, &target);
        let other = Identity::generate().public_key();
        for forged in [
            Revocation { public_key: other, ..revocation.clone() },
            Revocation { issuer: other, ..revocation.clone() },
            Revocation { issued_at_ms: 2_000, ..revocation.clone() },
        ] {
            assert!(!forged.is_signed());
        }
    }

    #[test]
    fn a_revocation_survives_the_wire() {
        let revocation = revoke(&Identity::generate(), &Identity::generate());
        let json = serde_json::to_string(&revocation).unwrap();
        assert_eq!(serde_json::from_str::<Revocation>(&json).unwrap(), revocation);
    }

    #[test]
    fn a_trusted_issuer_revokes_a_node() {
        let (node, issuer, target) = (Node::new(), Identity::generate(), Identity::generate());
        node.trusts(&issuer, TrustedVia::Passphrase);
        node.trusts(&target, TrustedVia::Passphrase);
        assert_eq!(node.receive(&issuer, &revoke(&issuer, &target)), Verdict::Applied);
        assert!(node.is_revoked(&target));
    }

    #[test]
    fn a_key_this_node_never_met_stays_revoked_once_it_shows_up() {
        let (node, issuer, target) = (Node::new(), Identity::generate(), Identity::generate());
        node.trusts(&issuer, TrustedVia::Passphrase);
        node.receive(&issuer, &revoke(&issuer, &target));
        assert!(node.trust.trust(&target.public_key(), "thief", TrustedVia::Passphrase, 200).is_err());
        assert!(node.is_revoked(&target));
    }

    #[test]
    fn a_revocation_already_held_is_not_applied_twice() {
        let (node, issuer, target) = (Node::new(), Identity::generate(), Identity::generate());
        node.trusts(&issuer, TrustedVia::Passphrase);
        let revocation = revoke(&issuer, &target);
        node.receive(&issuer, &revocation);
        assert_eq!(node.receive(&issuer, &revocation), Verdict::AlreadyKnown);
    }

    #[test]
    fn a_forged_revocation_is_refused() {
        let (node, issuer, target) = (Node::new(), Identity::generate(), Identity::generate());
        node.trusts(&issuer, TrustedVia::Passphrase);
        let forged = Revocation { issued_at_ms: 5, ..revoke(&issuer, &target) };
        assert_eq!(node.receive(&issuer, &forged), Verdict::Refused(Refusal::BadSignature));
        assert!(!node.is_revoked(&target));
    }

    #[test]
    fn a_stranger_cannot_revoke_anyone() {
        let (node, stranger, target) = (Node::new(), Identity::generate(), Identity::generate());
        node.trusts(&target, TrustedVia::Passphrase);
        assert_eq!(node.receive(&stranger, &revoke(&stranger, &target)), Verdict::Refused(Refusal::NoAuthority));
        assert!(!node.is_revoked(&target));
    }

    #[test]
    fn a_trusted_node_vouches_for_a_revocation_from_an_issuer_this_node_never_met() {
        let (node, sender, issuer, target) = (Node::new(), Identity::generate(), Identity::generate(), Identity::generate());
        node.trusts(&sender, TrustedVia::Passphrase);
        assert_eq!(node.receive(&sender, &revoke(&issuer, &target)), Verdict::Applied);
    }

    #[test]
    fn a_revoked_node_vouches_for_nothing() {
        let (node, sender, target) = (Node::new(), Identity::generate(), Identity::generate());
        node.trusts(&sender, TrustedVia::Passphrase);
        node.receive(&node.identity, &revoke(&node.identity, &sender));
        assert_eq!(node.receive(&sender, &revoke(&sender, &target)), Verdict::Refused(Refusal::NoAuthority));
    }

    #[test]
    fn a_revoked_issuer_is_refused_even_when_a_trusted_node_passes_it_on() {
        let (node, sender, revoked, target) = (Node::new(), Identity::generate(), Identity::generate(), Identity::generate());
        node.trusts(&sender, TrustedVia::Passphrase);
        node.receive(&node.identity, &revoke(&node.identity, &revoked));
        assert_eq!(node.receive(&sender, &revoke(&revoked, &target)), Verdict::Refused(Refusal::NoAuthority));
    }

    #[test]
    fn a_passphrase_node_cannot_revoke_a_paired_one() {
        let (node, issuer, paired) = (Node::new(), Identity::generate(), Identity::generate());
        node.trusts(&issuer, TrustedVia::Passphrase);
        node.trusts(&paired, TrustedVia::Pairing);
        assert_eq!(node.receive(&issuer, &revoke(&issuer, &paired)), Verdict::Refused(Refusal::OutranksAuthority));
        assert!(!node.is_revoked(&paired));
    }

    #[test]
    fn a_paired_node_can_revoke_a_passphrase_one() {
        let (node, issuer, target) = (Node::new(), Identity::generate(), Identity::generate());
        node.trusts(&issuer, TrustedVia::Pairing);
        node.trusts(&target, TrustedVia::Passphrase);
        assert_eq!(node.receive(&issuer, &revoke(&issuer, &target)), Verdict::Applied);
    }

    #[test]
    fn a_node_never_revokes_itself_on_anothers_word() {
        let (node, issuer) = (Node::new(), Identity::generate());
        node.trusts(&issuer, TrustedVia::Pairing);
        assert_eq!(node.receive(&issuer, &revoke(&issuer, &node.identity)), Verdict::Refused(Refusal::OfThisNode));
        assert!(node.trust.revocations().unwrap().is_empty());
    }

    #[test]
    fn this_nodes_own_revocations_need_no_one_elses_trust() {
        let (node, stranger, paired) = (Node::new(), Identity::generate(), Identity::generate());
        node.trusts(&paired, TrustedVia::Pairing);
        assert_eq!(node.receive(&stranger, &revoke(&node.identity, &paired)), Verdict::Applied);
    }

    #[test]
    fn revocations_are_kept_across_restarts_to_pass_on() {
        let dir = tempfile::tempdir().unwrap();
        let (issuer, target) = (Identity::generate(), Identity::generate());
        let revocation = revoke(&issuer, &target);
        TrustStore::open(dir.path()).unwrap().revoke(&revocation).unwrap();
        let reopened = TrustStore::open(dir.path()).unwrap();
        assert_eq!(reopened.revocations().unwrap(), [revocation]);
        assert_eq!(reopened.standing(&target.public_key()).unwrap(), Standing::Revoked);
    }

    #[test]
    fn the_trust_store_refuses_an_unsigned_revocation() {
        let node = Node::new();
        let forged = Revocation { issued_at_ms: 5, ..revoke(&Identity::generate(), &Identity::generate()) };
        assert!(node.trust.revoke(&forged).is_err());
    }
}
