//! The group key: what makes a node a member (SPEC §6.2, ADR 0013).
//!
//! There is one group per node, and being in it is knowing its key. The key is
//! 128 random bits, shown to people as a code they paste (`hm-…`), and proved
//! to other nodes with an HMAC rather than sent. A node that cannot produce the
//! proof is not in the group, whatever else it says about itself.
//!
//! This module is pure: it generates, parses, proves and stores. Deciding what
//! a verified proof *means* — pinning a certificate, admitting a peer — is the
//! service layer's job, because it is the one that holds the address book.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Serialize};

/// How many bytes of key there are.
///
/// 128 bits is what lets the proof be a plain HMAC. A joining node greets every
/// node it discovers, so a stranger on the LAN collects its proofs; with a key
/// a person chose, those could be brute-forced offline, and the answer would
/// be a PAKE. With 128 random bits there is nothing to brute-force.
pub const KEY_LEN: usize = 16;

/// How far a proof's timestamp may be from this node's clock, either way.
///
/// The proof is already bound to both certificates, so a captured one is no
/// use to anybody but the two nodes it was made between. The timestamp only
/// bounds how long a proof that leaked into a log stays meaningful. An hour is
/// generous on purpose: a laptop whose clock drifted a few minutes while it
/// slept should still be able to join, and nothing is bought by being strict.
pub const PROOF_WINDOW_MS: i64 = 60 * 60 * 1000;

/// Separates a group proof from any other HMAC this key might ever sign.
const DOMAIN: &[u8] = b"hivemind group proof v1\0";

/// What a code starts with, so that it is recognisable when pasted.
const CODE_PREFIX: &str = "hm-";

/// The file the key lives in, beside `peers.toml` (SPEC §4.3).
pub const GROUP_FILE: &str = "group.toml";

/// Why a group operation failed.
#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    /// What was typed is not a group code.
    #[error(
        "that is not a group code; it should look like `hm-xxxx-xxxx-…` with 26 letters and digits after `hm-`"
    )]
    InvalidCode,
    /// The operating system would not provide randomness.
    #[error("the operating system would not provide randomness for a group key")]
    NoRandomness,
    /// The file could not be read or written.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },
    /// The file is there but is not a group file.
    #[error("{path} is not a valid group file: {detail}")]
    Malformed {
        /// Which file.
        path: String,
        /// What was wrong with it.
        detail: String,
    },
}

/// Why a proof was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProofError {
    /// Made too long ago, or too far in the future, for this node's clock.
    #[error("the proof's timestamp is more than an hour from this node's clock")]
    Stale,
    /// Not made with this group's key, or not for these two certificates.
    #[error("the proof was not made with this group's key")]
    Mismatch,
}

/// The secret that makes a node a member.
///
/// Deliberately has no `Display`: the one way to turn it into text is
/// [`GroupKey::code`], which is a name nobody types by accident into a log line.
#[derive(Clone, PartialEq, Eq)]
pub struct GroupKey([u8; KEY_LEN]);

impl std::fmt::Debug for GroupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A `{:?}` of a struct holding the key must not print it.
        f.write_str("GroupKey(…)")
    }
}

impl GroupKey {
    /// A fresh key.
    ///
    /// # Errors
    /// [`GroupError::NoRandomness`] if the operating system will not provide it.
    pub fn generate() -> Result<Self, GroupError> {
        let mut bytes = [0u8; KEY_LEN];
        getrandom::fill(&mut bytes).map_err(|_| GroupError::NoRandomness)?;
        Ok(Self(bytes))
    }

    /// A key from known bytes. For tests and golden vectors.
    #[must_use]
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The code people paste into `hivemind pair`.
    ///
    /// Lowercase unpadded base32 in groups of four — the alphabet the node id
    /// already uses, so there is one way hivemind writes bytes for humans, not
    /// two.
    #[must_use]
    pub fn code(&self) -> String {
        let body = BASE32_NOPAD.encode(&self.0).to_ascii_lowercase();
        let groups: Vec<&str> = body
            .as_bytes()
            .chunks(4)
            .map(|chunk| std::str::from_utf8(chunk).expect("base32 is ASCII"))
            .collect();
        format!("{CODE_PREFIX}{}", groups.join("-"))
    }

    /// Prove possession of the key to the node holding `receiver_cert`.
    ///
    /// Bound to both certificates so the proof is worth nothing between any
    /// other two nodes, and to the time so a proof that leaks stops meaning
    /// anything. The exact bytes are in `docs/protocol.md`.
    #[must_use]
    pub fn prove(&self, sender_cert: &[u8], receiver_cert: &[u8], sent_at_ms: i64) -> [u8; 32] {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.0);
        let tag = ring::hmac::sign(&key, &proof_message(sender_cert, receiver_cert, sent_at_ms));
        let mut out = [0u8; 32];
        out.copy_from_slice(tag.as_ref());
        out
    }

    /// Check a proof made by the node holding `sender_cert`, for us.
    ///
    /// # Errors
    /// [`ProofError::Stale`] if the timestamp is outside [`PROOF_WINDOW_MS`] of
    /// `now_ms`, [`ProofError::Mismatch`] if the MAC is not this key's.
    pub fn verify(
        &self,
        sender_cert: &[u8],
        receiver_cert: &[u8],
        sent_at_ms: i64,
        mac: &[u8],
        now_ms: i64,
    ) -> Result<(), ProofError> {
        if now_ms.abs_diff(sent_at_ms) > PROOF_WINDOW_MS.unsigned_abs() {
            return Err(ProofError::Stale);
        }
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.0);
        // `ring` compares in constant time; comparing the bytes ourselves
        // would leak how much of a guess was right.
        ring::hmac::verify(
            &key,
            &proof_message(sender_cert, receiver_cert, sent_at_ms),
            mac,
        )
        .map_err(|_| ProofError::Mismatch)
    }
}

impl FromStr for GroupKey {
    type Err = GroupError;

    /// Parse a code as a person pasted it.
    ///
    /// Forgiving about everything that is presentation — case, the `hm-`
    /// prefix, hyphens, stray spaces from a chat window — and strict about
    /// the bits: exactly 128 of them, with the padding bits of the last
    /// character zero, so there is one spelling of each key.
    fn from_str(typed: &str) -> Result<Self, Self::Err> {
        let trimmed = typed.trim().to_ascii_lowercase();
        let body = trimmed.strip_prefix(CODE_PREFIX).unwrap_or(&trimmed);
        let compact: String = body
            .chars()
            .filter(|c| *c != '-' && !c.is_whitespace())
            .collect::<String>()
            .to_ascii_uppercase();

        let bytes = BASE32_NOPAD
            .decode(compact.as_bytes())
            .map_err(|_| GroupError::InvalidCode)?;
        let bytes: [u8; KEY_LEN] = bytes.try_into().map_err(|_| GroupError::InvalidCode)?;
        Ok(Self(bytes))
    }
}

/// The bytes under the HMAC (`docs/protocol.md`).
///
/// Every variable-length field is length-prefixed, so no pair of certificates
/// can be rearranged into another pair that produces the same message.
fn proof_message(sender_cert: &[u8], receiver_cert: &[u8], sent_at_ms: i64) -> Vec<u8> {
    let mut message =
        Vec::with_capacity(DOMAIN.len() + 4 + sender_cert.len() + 4 + receiver_cert.len() + 8);
    message.extend_from_slice(DOMAIN);
    for cert in [sender_cert, receiver_cert] {
        let len = u32::try_from(cert.len()).expect("a certificate is far smaller than 4 GiB");
        message.extend_from_slice(&len.to_be_bytes());
        message.extend_from_slice(cert);
    }
    message.extend_from_slice(&sent_at_ms.to_be_bytes());
    message
}

/// The group this node belongs to, as kept in `group.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The key.
    pub key: GroupKey,
    /// When this node joined or created it.
    pub joined_at: DateTime<Utc>,
}

/// `group.toml` on disk. The key is stored as its code, so the file is
/// something a person can read and, in an emergency, paste from.
#[derive(Serialize, Deserialize)]
struct GroupFile {
    code: String,
    joined_at: DateTime<Utc>,
}

impl Group {
    /// A group holding `key`, joined now.
    #[must_use]
    pub fn new(key: GroupKey) -> Self {
        Self {
            key,
            joined_at: Utc::now(),
        }
    }

    /// Read the group from `dir`, if this node is in one.
    ///
    /// # Errors
    /// [`GroupError::Io`] if the file exists and cannot be read, or
    /// [`GroupError::Malformed`] if it is not a group file.
    pub fn load(dir: &Path) -> Result<Option<Self>, GroupError> {
        let path = dir.join(GROUP_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(GroupError::Io {
                    context: format!("could not read {}", path.display()),
                    source,
                });
            }
        };

        let malformed = |detail: String| GroupError::Malformed {
            path: path.display().to_string(),
            detail,
        };
        let file: GroupFile = toml::from_str(&text).map_err(|e| malformed(e.to_string()))?;
        let key = file
            .code
            .parse()
            .map_err(|e: GroupError| malformed(e.to_string()))?;
        Ok(Some(Self {
            key,
            joined_at: file.joined_at,
        }))
    }

    /// Write the group to `dir`, atomically and readable only by its owner.
    ///
    /// # Errors
    /// [`GroupError::Io`] if the write or rename fails.
    pub fn save(&self, dir: &Path) -> Result<(), GroupError> {
        let path = dir.join(GROUP_FILE);
        let text = toml::to_string_pretty(&GroupFile {
            code: self.key.code(),
            joined_at: self.joined_at,
        })
        .map_err(|e| GroupError::Malformed {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;

        let temp: PathBuf = path.with_extension("toml.tmp");
        write_private(&temp, text.as_bytes())?;
        // Atomic, so a crash never leaves half a key — which would read as
        // "not in a group" and quietly drop this node out of it.
        std::fs::rename(&temp, &path).map_err(|source| GroupError::Io {
            context: format!("could not move {} into place", temp.display()),
            source,
        })
    }
}

/// Write a file only the owner can read.
///
/// Created 0600 rather than chmodded after, as the identity key is: between
/// the two there would be a moment when the group key is world-readable.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), GroupError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let io = |source| GroupError::Io {
        context: format!("could not write {}", path.display()),
        source,
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(io)?;
    file.write_all(bytes).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produced by `docs/reference/group_proof_reference.py`, which is written
    /// from `docs/protocol.md` rather than from this file. If these fail, the
    /// wire format changed: that needs an ADR, not a new vector.
    const GOLDEN_CODE: &str = "hm-aaaq-eaye-auda-ocaj-bifq-ydio-b4";
    const GOLDEN_PROOF_HEX: &str =
        "96317562f141081774fd826ac79b205f4f6afbdc112ab3b4106a51cf907a781f";
    const SENT_AT_MS: i64 = 1_750_000_000_000;

    fn golden_key() -> GroupKey {
        let mut bytes = [0u8; KEY_LEN];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::try_from(i).expect("small");
        }
        GroupKey::from_bytes(bytes)
    }

    #[test]
    fn the_code_matches_the_reference_vector() {
        assert_eq!(golden_key().code(), GOLDEN_CODE);
    }

    #[test]
    fn the_proof_matches_the_reference_vector() {
        let mac = golden_key().prove(b"sender certificate", b"receiver certificate", SENT_AT_MS);
        assert_eq!(data_encoding::HEXLOWER.encode(&mac), GOLDEN_PROOF_HEX);
    }

    #[test]
    fn a_code_parses_back_to_the_same_key() {
        let key = GroupKey::generate().expect("randomness");
        assert_eq!(key.code().parse::<GroupKey>().expect("parse"), key);
    }

    #[test]
    fn a_code_survives_being_pasted_carelessly() {
        // Case, the prefix, and whatever a chat window did to the hyphens are
        // presentation. The person pasting it should not have to care.
        let key = golden_key();
        for typed in [
            "HM-AAAQ-EAYE-AUDA-OCAJ-BIFQ-YDIO-B4",
            "aaaq-eaye-auda-ocaj-bifq-ydio-b4",
            "aaaqeayeaudaocajbifqydiob4",
            "  hm-aaaq eaye auda ocaj bifq ydio b4\n",
        ] {
            assert_eq!(typed.parse::<GroupKey>().expect(typed), key, "{typed:?}");
        }
    }

    #[test]
    fn a_code_of_the_wrong_length_is_refused() {
        // One character short, and one too many.
        assert!(
            "hm-aaaq-eaye-auda-ocaj-bifq-ydio-b"
                .parse::<GroupKey>()
                .is_err()
        );
        assert!(
            "hm-aaaq-eaye-auda-ocaj-bifq-ydio-b4a"
                .parse::<GroupKey>()
                .is_err()
        );
    }

    #[test]
    fn a_code_with_nonzero_padding_bits_is_refused() {
        // `b4` ends the canonical spelling; `b5` differs only in the two bits
        // past the 128th. Accepting both would give one key two codes.
        assert!(
            "hm-aaaq-eaye-auda-ocaj-bifq-ydio-b5"
                .parse::<GroupKey>()
                .is_err()
        );
    }

    #[test]
    fn something_that_is_not_a_code_is_refused() {
        assert!("hm1:w2mq-xor2".parse::<GroupKey>().is_err());
        assert!("".parse::<GroupKey>().is_err());
        assert!(
            "hm-0000-0000-0000-0000-0000-0000-00"
                .parse::<GroupKey>()
                .is_err()
        );
    }

    #[test]
    fn a_proof_verifies_for_the_pair_it_was_made_for() {
        let key = golden_key();
        let mac = key.prove(b"a", b"b", SENT_AT_MS);
        assert_eq!(key.verify(b"a", b"b", SENT_AT_MS, &mac, SENT_AT_MS), Ok(()));
    }

    #[test]
    fn a_proof_made_with_another_key_is_refused() {
        let theirs = GroupKey::from_bytes([9u8; KEY_LEN]);
        let mac = theirs.prove(b"a", b"b", SENT_AT_MS);
        assert_eq!(
            golden_key().verify(b"a", b"b", SENT_AT_MS, &mac, SENT_AT_MS),
            Err(ProofError::Mismatch)
        );
    }

    #[test]
    fn a_proof_is_worthless_between_any_other_two_nodes() {
        // What stops a node that was greeted from replaying the greeting to
        // somebody else, or back at the sender.
        let key = golden_key();
        let mac = key.prove(b"a", b"b", SENT_AT_MS);
        for (sender, receiver) in [(&b"a"[..], &b"c"[..]), (b"c", b"b"), (b"b", b"a")] {
            assert_eq!(
                key.verify(sender, receiver, SENT_AT_MS, &mac, SENT_AT_MS),
                Err(ProofError::Mismatch),
                "{sender:?} → {receiver:?}"
            );
        }
    }

    #[test]
    fn certificates_cannot_be_rearranged_into_the_same_message() {
        // Without length prefixes "ab"+"c" and "a"+"bc" would sign alike.
        let key = golden_key();
        assert_ne!(
            key.prove(b"ab", b"c", SENT_AT_MS),
            key.prove(b"a", b"bc", SENT_AT_MS)
        );
    }

    #[test]
    fn a_proof_is_accepted_up_to_an_hour_either_way_and_not_beyond() {
        let key = golden_key();
        let mac = key.prove(b"a", b"b", SENT_AT_MS);
        for now in [SENT_AT_MS - PROOF_WINDOW_MS, SENT_AT_MS + PROOF_WINDOW_MS] {
            assert_eq!(key.verify(b"a", b"b", SENT_AT_MS, &mac, now), Ok(()));
        }
        for now in [
            SENT_AT_MS - PROOF_WINDOW_MS - 1,
            SENT_AT_MS + PROOF_WINDOW_MS + 1,
        ] {
            assert_eq!(
                key.verify(b"a", b"b", SENT_AT_MS, &mac, now),
                Err(ProofError::Stale)
            );
        }
    }

    #[test]
    fn a_key_never_prints_itself() {
        let key = golden_key();
        let printed = format!("{:?}", Group::new(key.clone()));
        assert!(!printed.contains("aaaq"), "the key leaked: {printed}");
    }

    #[test]
    fn a_node_without_a_group_file_is_in_no_group() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(Group::load(dir.path()).expect("load").is_none());
    }

    #[test]
    fn a_group_survives_a_save_and_load() {
        let dir = tempfile::tempdir().expect("dir");
        let group = Group::new(golden_key());
        group.save(dir.path()).expect("save");
        let loaded = Group::load(dir.path()).expect("load").expect("present");
        // Stored at the precision TOML keeps, which is the precision it had.
        assert_eq!(loaded.key, group.key);
        assert_eq!(loaded.joined_at, group.joined_at);
        assert!(!dir.path().join("group.toml.tmp").exists());
    }

    #[test]
    fn the_group_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("dir");
        Group::new(golden_key()).save(dir.path()).expect("save");
        let mode = std::fs::metadata(dir.path().join(GROUP_FILE))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the key must be as private as node.key"
        );
    }

    #[test]
    fn a_group_file_that_is_not_one_is_an_error_not_an_empty_group() {
        // Reading garbage as "no group" would silently drop this node out of
        // it; saying so lets a person fix the file.
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join(GROUP_FILE), "code = \"nonsense\"\n").expect("write");
        assert!(matches!(
            Group::load(dir.path()),
            Err(GroupError::Malformed { .. })
        ));
    }
}
