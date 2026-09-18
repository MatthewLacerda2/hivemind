//! Node identity, the peer address book and recipient expansion.
//!
//! A [`NodeId`] is the SHA-256 fingerprint of a node's DER-encoded certificate
//! (SPEC §6.1). Identity is per machine; the free-text `owner` label is what
//! makes `to: matthew` fan out across all of Matthew's machines (SPEC §8).

use std::fmt::{self, Write as _};
use std::str::FromStr;

use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use crate::crypto::{deserialize_bytes, serialize_bytes};

/// Characters per dash-separated group in the display form.
const GROUP: usize = 4;
/// A 32-byte digest is 52 unpadded base32 characters, so 13 groups of four.
const GROUPS: usize = 13;
/// How many characters of the base32 body the short form keeps (SPEC §6.1).
const SHORT_LEN: usize = 8;

/// The identity of one machine: the SHA-256 fingerprint of its DER-encoded
/// self-signed certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId([u8; 32]);

impl NodeId {
    /// Fingerprint a DER-encoded certificate.
    #[must_use]
    pub fn from_certificate_der(der: &[u8]) -> Self {
        Self(Sha256::digest(der).into())
    }

    /// The raw fingerprint bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The short form humans compare by eye and pass to `hivemind pair`.
    #[must_use]
    pub fn short(&self) -> String {
        let mut encoded = BASE32_NOPAD.encode(&self.0);
        encoded.truncate(SHORT_LEN);
        encoded.make_ascii_lowercase();
        encoded
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut encoded = BASE32_NOPAD.encode(&self.0);
        encoded.make_ascii_lowercase();

        f.write_str("hm1:")?;
        for (i, group) in encoded.as_bytes().chunks(GROUP).enumerate() {
            if i > 0 {
                f.write_char('-')?;
            }
            // Base32 output is ASCII, so each chunk is valid UTF-8. Mapping the
            // error rather than unwrapping keeps the no-panic rule intact even
            // though this branch is unreachable.
            f.write_str(str::from_utf8(group).map_err(|_| fmt::Error)?)?;
        }
        Ok(())
    }
}

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_bytes(&self.0, s)
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_bytes::<D, 32>(d).map(Self)
    }
}

/// Why a string could not be read as a [`NodeId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NodeIdParseError {
    /// The string did not start with `hm1:`.
    #[error("expected a node id starting with `hm1:`")]
    MissingPrefix,
    /// The body was not 52 base32 characters in groups of four.
    #[error("expected 13 groups of 4 base32 characters after `hm1:`")]
    MalformedBody,
}

impl FromStr for NodeId {
    type Err = NodeIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let body = s
            .strip_prefix("hm1:")
            .ok_or(NodeIdParseError::MissingPrefix)?;

        let groups: Vec<&str> = body.split('-').collect();
        if groups.len() != GROUPS || groups.iter().any(|g| g.len() != GROUP) {
            return Err(NodeIdParseError::MalformedBody);
        }

        // The canonical form is lowercase, but a human retyping a fingerprint
        // should not be punished for their shift key.
        let joined = groups.concat().to_ascii_uppercase();
        let bytes = BASE32_NOPAD
            .decode(joined.as_bytes())
            .map_err(|_| NodeIdParseError::MalformedBody)?;

        let digest: [u8; 32] = bytes
            .try_into()
            .map_err(|_| NodeIdParseError::MalformedBody)?;
        Ok(Self(digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Independently computed: `printf 'hivemind test certificate' | shasum -a 256`.
    const CERT: &[u8] = b"hivemind test certificate";
    const FINGERPRINT_HEX: &str =
        "b6990bba3a91115598f0c0601df91546b8e6f6aa1bed6fefca220484329b0b6d";
    /// Independently computed with Python's `base64.b32encode`, lowercased and
    /// stripped of padding.
    const DISPLAY: &str = "hm1:w2mq-xor2-seiv-lghq-ybqb-36iv-i24o-n5vk-dpww-736k-eici-imu3-bnwq";

    #[test]
    fn node_id_is_the_sha256_of_the_certificate_der() {
        let id = NodeId::from_certificate_der(CERT);
        assert_eq!(hex::encode(id.as_bytes()), FINGERPRINT_HEX);
    }

    #[test]
    fn node_id_displays_as_hm1_prefixed_lowercase_base32_in_groups_of_four() {
        let id = NodeId::from_certificate_der(CERT);
        assert_eq!(id.to_string(), DISPLAY);
    }

    #[test]
    fn node_id_display_covers_the_whole_fingerprint() {
        // 52 base32 characters is 260 bits of alphabet space carrying all 256
        // bits of the digest. Nothing is truncated away.
        let id = NodeId::from_certificate_der(CERT);
        let body: String = id.to_string()["hm1:".len()..].replace('-', "");
        assert_eq!(body.len(), 52);
    }

    #[test]
    fn node_id_round_trips_through_its_display_form() {
        let id = NodeId::from_certificate_der(CERT);
        let parsed: NodeId = id.to_string().parse().expect("display form must parse");
        assert_eq!(parsed, id);
    }

    #[test]
    fn node_id_short_form_is_the_first_eight_base32_characters() {
        let id = NodeId::from_certificate_der(CERT);
        assert_eq!(id.short(), "w2mqxor2");
    }

    proptest::proptest! {
        /// The example above pins one fingerprint; this covers the rest of the
        /// input space, because a display form that loses information for some
        /// digests would be worse than one that loses it for all of them.
        #[test]
        fn every_node_id_round_trips_through_its_display_form(bytes: [u8; 32]) {
            let id = NodeId(bytes);
            let parsed: NodeId = id.to_string().parse().expect("display form must parse");
            proptest::prop_assert_eq!(parsed, id);
        }
    }

    #[test]
    fn parsing_a_string_without_the_hm1_prefix_is_rejected() {
        let without_prefix = &DISPLAY["hm1:".len()..];
        assert_eq!(
            without_prefix.parse::<NodeId>(),
            Err(NodeIdParseError::MissingPrefix)
        );
    }

    #[test]
    fn parsing_a_truncated_node_id_is_rejected() {
        let truncated = &DISPLAY[..DISPLAY.len() - 5];
        assert_eq!(
            truncated.parse::<NodeId>(),
            Err(NodeIdParseError::MalformedBody)
        );
    }

    #[test]
    fn parsing_a_node_id_with_a_non_base32_character_is_rejected() {
        // `1` is not in the RFC 4648 base32 alphabet.
        let corrupted = DISPLAY.replace("w2mq", "w1mq");
        assert_eq!(
            corrupted.parse::<NodeId>(),
            Err(NodeIdParseError::MalformedBody)
        );
    }
}
