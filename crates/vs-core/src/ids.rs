use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

use crate::CoreError;

/// Data Vault hash key: lowercase hex SHA-256 over `|`-joined, trimmed parts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HashKey(String);

impl HashKey {
    pub fn of(parts: &[&str]) -> Self {
        let joined = parts.iter().map(|p| p.trim()).collect::<Vec<_>>().join("|");
        let digest = Sha256::digest(joined.as_bytes());
        HashKey(hex::encode(digest))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `H(tenant | sha256(original bytes))`
    pub fn document(tenant: &TenantId, content_sha256: &str) -> Self {
        Self::of(&[tenant.as_str(), content_sha256])
    }

    /// `H(document_hk | page_no)`
    pub fn page(document: &HashKey, page_no: u32) -> Self {
        Self::of(&[document.as_str(), &page_no.to_string()])
    }

    /// `H(page_hk | run_id | block_index)`
    pub fn block(page: &HashKey, run: &RunId, block_index: u32) -> Self {
        Self::of(&[page.as_str(), &run.to_string(), &block_index.to_string()])
    }

    /// `H(tenant | canonical_entity_key)`
    pub fn entity(tenant: &TenantId, canonical_key: &str) -> Self {
        Self::of(&[tenant.as_str(), canonical_key])
    }

    /// `H(run_id | predicate | sorted argument keys)`
    pub fn assertion(run: &RunId, predicate: &str, args: &[&HashKey]) -> Self {
        let mut keys: Vec<&str> = args.iter().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        let run_s = run.to_string();
        let mut parts = vec![run_s.as_str(), predicate];
        parts.extend(keys);
        Self::of(&parts)
    }
}

impl fmt::Display for HashKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Hex SHA-256 of raw bytes (used for original file content).
pub fn content_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// One processing run of one document (UUIDv7, time-ordered).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub uuid::Uuid);

impl RunId {
    pub fn new() -> Self {
        RunId(uuid::Uuid::now_v7())
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TenantId(pub String);

impl TenantId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// BCP-47 language tag supplied by the caller (e.g. `en`, `de-CH`, `zh-Hant`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LanguageTag(String);

impl LanguageTag {
    pub fn parse(s: &str) -> Result<Self, CoreError> {
        let ok = !s.is_empty()
            && s.split('-').all(|p| !p.is_empty() && p.len() <= 8 && p.chars().all(|c| c.is_ascii_alphanumeric()));
        if ok {
            Ok(LanguageTag(s.to_string()))
        } else {
            Err(CoreError::InvalidLanguage(s.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Primary language subtag, lowercased (`de-CH` -> `de`).
    pub fn primary(&self) -> String {
        self.0.split('-').next().unwrap_or("").to_ascii_lowercase()
    }
}

impl TryFrom<String> for LanguageTag {
    type Error = CoreError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        LanguageTag::parse(&s)
    }
}

impl From<LanguageTag> for String {
    fn from(l: LanguageTag) -> String {
        l.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_key_is_stable_and_trims() {
        let a = HashKey::of(&["tenant", " abc "]);
        let b = HashKey::of(&["tenant", "abc"]);
        assert_eq!(a, b);
        assert_eq!(a.as_str().len(), 64);
    }

    #[test]
    fn empty_parts_are_not_skipped() {
        assert_ne!(HashKey::of(&["a", "", "b"]), HashKey::of(&["a", "b"]));
    }

    #[test]
    fn assertion_key_ignores_argument_order() {
        let run = RunId::new();
        let x = HashKey::of(&["x"]);
        let y = HashKey::of(&["y"]);
        assert_eq!(
            HashKey::assertion(&run, "uses", &[&x, &y]),
            HashKey::assertion(&run, "uses", &[&y, &x])
        );
    }

    #[test]
    fn language_tag() {
        assert_eq!(LanguageTag::parse("de-CH").unwrap().primary(), "de");
        assert!(LanguageTag::parse("").is_err());
        assert!(LanguageTag::parse("en--US").is_err());
    }
}
