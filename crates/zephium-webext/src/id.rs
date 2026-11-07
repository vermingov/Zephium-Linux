use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// A Chrome extension ID: 32 characters in `a..=p`, encoding the first 128
/// bits of the SHA-256 of the extension's public key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExtensionId(String);

impl ExtensionId {
    pub const LEN: usize = 32;

    pub fn parse(s: &str) -> Option<Self> {
        (s.len() == Self::LEN && s.bytes().all(|b| (b'a'..=b'p').contains(&b)))
            .then(|| Self(s.to_owned()))
    }

    pub fn from_public_key(spki_der: &[u8]) -> Self {
        let digest = Sha256::digest(spki_der);
        Self::from_hash_prefix(digest[..16].try_into().expect("SHA-256 is 32 bytes"))
    }

    /// The ID Chrome gives an unpacked extension that has no `key`: derived
    /// from where it was loaded from, so reloading it keeps the same ID.
    pub fn from_source_path(path: &str) -> Self {
        Self::from_public_key(path.as_bytes())
    }

    /// The ID a Firefox add-on gets: derived from its Gecko ID
    /// (`browser_specific_settings.gecko.id`), so the add-on keeps one ID and
    /// origin wherever its `.xpi` was installed from.
    pub fn from_gecko_id(gecko_id: &str) -> Self {
        let digest = Sha256::digest(format!("gecko:{gecko_id}").as_bytes());
        Self::from_hash_prefix(digest[..16].try_into().expect("SHA-256 is 32 bytes"))
    }

    pub(crate) fn from_hash_prefix(prefix: &[u8; 16]) -> Self {
        let id = prefix
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 0x0f])
            .map(|nibble| char::from(b'a' + nibble))
            .collect();
        Self(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExtensionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ExtensionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for ExtensionId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ExtensionId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid extension ID {s:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_well_formed_ids() {
        assert!(ExtensionId::parse("nngceckbapebfimnlniiiahkandclblb").is_some());
        assert!(ExtensionId::parse("nngceckbapebfimnlniiiahkandclbl").is_none());
        assert!(ExtensionId::parse("nngceckbapebfimnlniiiahkandclblq").is_none());
        assert!(ExtensionId::parse("NNGCECKBAPEBFIMNLNIIIAHKANDCLBLB").is_none());
    }

    #[test]
    fn maps_hash_nibbles_high_first() {
        let id = ExtensionId::from_hash_prefix(&[
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xf0, 0, 0, 0, 0, 0, 0, 0x0f,
        ]);
        assert_eq!(id.as_str(), "abcdefghijklmnoppaaaaaaaaaaaaaap");
    }

    #[test]
    fn derives_from_public_key_digest() {
        let id = ExtensionId::from_public_key(b"key");
        let digest = Sha256::digest(b"key");
        assert_eq!(id.as_str().len(), 32);
        assert_eq!(id.as_str().as_bytes()[0], b'a' + (digest[0] >> 4));
        assert_eq!(id.as_str().as_bytes()[31], b'a' + (digest[15] & 0x0f));
    }

    #[test]
    fn serde_round_trips_as_string() {
        let id = ExtensionId::parse("abcdefghijklmnopabcdefghijklmnop").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"abcdefghijklmnopabcdefghijklmnop\"");
        assert_eq!(serde_json::from_str::<ExtensionId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<ExtensionId>("\"zz\"").is_err());
    }
}

#[cfg(test)]
mod gecko_tests {
    use super::ExtensionId;

    #[test]
    fn gecko_ids_map_to_stable_chrome_style_ids() {
        let first = ExtensionId::from_gecko_id("uBlock0@raymondhill.net");
        assert_eq!(first, ExtensionId::from_gecko_id("uBlock0@raymondhill.net"));
        assert!(ExtensionId::parse(first.as_str()).is_some());
        assert_ne!(first, ExtensionId::from_gecko_id("{d10d0bf8-f5b5-c8b4-a8b2-2b9879e08c5d}"));
        // A Gecko ID never collides with the same text taken as a path.
        assert_ne!(first, ExtensionId::from_source_path("uBlock0@raymondhill.net"));
    }
}
