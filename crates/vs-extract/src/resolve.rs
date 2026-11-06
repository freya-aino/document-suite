//! Entity resolution and type normalisation.

use async_trait::async_trait;

use vs_core::{HashKey, TenantId};

use crate::model::DocEntity;
use crate::ExtractError;

/// Finds abbreviation definitions of the form `Long Form (LF)`.
/// Returns `(long_form, short_form)` pairs.
pub fn abbreviation_definitions(text: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut rest = text;
    let mut offset = 0;
    while let Some(open) = rest.find('(') {
        let Some(close_rel) = rest[open..].find(')') else { break };
        let short = rest[open + 1..open + close_rel].trim();
        let before = &text[..offset + open];
        if (2..=10).contains(&short.chars().count()) && short.chars().any(|c| c.is_uppercase()) && !short.contains(' ') {
            let initials: Vec<char> = short.chars().filter(|c| c.is_alphabetic()).map(|c| c.to_ascii_lowercase()).collect();
            let words: Vec<&str> = before.split_whitespace().collect();
            let n = initials.len();
            if n > 0 && words.len() >= n {
                let cand = &words[words.len() - n..];
                let matches = cand
                    .iter()
                    .zip(&initials)
                    .all(|(w, c)| w.chars().next().map(|f| f.to_ascii_lowercase() == *c).unwrap_or(false));
                if matches {
                    out.push((cand.join(" "), short.to_string()));
                }
            }
        }
        offset += open + close_rel + 1;
        rest = &text[offset..];
    }
    out
}

/// Maps open types/predicates to the ontology; creates pending types when nothing matches.
#[async_trait]
pub trait TypeNormalizer: Send + Sync {
    async fn entity_type(&self, raw: &str) -> Result<String, ExtractError>;
    async fn predicate(&self, raw: &str) -> Result<String, ExtractError>;
}

/// Decision for one document entity against the corpus.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Link { entity_hk: HashKey, score: f32 },
    New { entity_hk: HashKey },
}

/// Cross-document canonicalisation against HelixDB (implemented in vs-helix / vs-worker).
#[async_trait]
pub trait CorpusResolver: Send + Sync {
    async fn resolve(&self, tenant: &TenantId, entity: &DocEntity, context: &str) -> Result<Resolution, ExtractError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_abbreviations() {
        let t = "We use Optical Character Recognition (OCR) and a Vision Language Model (VLM). See (Fig. 2).";
        let defs = abbreviation_definitions(t);
        assert_eq!(defs, vec![
            ("Optical Character Recognition".to_string(), "OCR".to_string()),
            ("Vision Language Model".to_string(), "VLM".to_string()),
        ]);
    }
}
