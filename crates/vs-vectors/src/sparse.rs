//! Lexical sparse vectors, computed here rather than by a model.
//!
//! This replaces the BM25 index the previous graph store provided, so exact-term matching
//! still works: invoice numbers, formula tags, citation keys, part numbers — the tokens a
//! dense embedder blurs away.
//!
//! The collection declares `modifier: idf`, so Qdrant supplies the inverse-document-
//! frequency half of BM25 from its own corpus statistics. What we send is the term
//! frequency half. That split is the whole reason this needs no model: IDF is the part
//! that requires knowing the corpus, and the server already does.
//!
//! Deliberately not BM42, which derives term importance from a transformer's attention
//! and ships through FastEmbed (Python). See `SPEC.md`.

/// A sparse vector as Qdrant wants it: parallel index and value arrays.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sparse {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

impl Sparse {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    pub fn len(&self) -> usize {
        self.indices.len()
    }
}

/// Longest token we index. Beyond this a "token" is a base64 blob or a scanning
/// artefact, not a word, and it would only ever match itself.
const MAX_TOKEN_LEN: usize = 40;

/// Tokenises for lexical matching: lowercase, split on anything not alphanumeric.
///
/// Digits are kept and never split from their word, because the tokens this exists to
/// find are things like `INV-2026-0042`, `GPT-4`, `eq. (3)` and `H2SO4`. A tokeniser that
/// dropped digits would defeat the purpose.
///
/// Single characters are dropped: they carry almost no selectivity and appear in nearly
/// every document, so they cost index space and contribute noise.
pub fn tokenise(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            push_token(&mut out, std::mem::take(&mut cur));
        }
    }
    push_token(&mut out, cur);
    out
}

fn push_token(out: &mut Vec<String>, t: String) {
    if t.chars().count() > 1 && t.len() <= MAX_TOKEN_LEN {
        out.push(t);
    }
}

/// FNV-1a over the token's bytes.
///
/// Any stable hash would do; what matters is that it is **fixed for all time and across
/// processes**. A points file written today must stay searchable by a worker built next
/// year, so this must never be swapped for `DefaultHasher`, whose output is not
/// guaranteed stable between Rust releases.
pub fn token_index(token: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in token.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Sparse vector for one text: term frequencies, log-saturated.
///
/// The weight is `1 + ln(tf)` rather than raw `tf`. Raw term frequency lets one word
/// repeated twenty times dominate a document's score, which is the problem BM25's
/// saturation term solves. Proper BM25 saturation also needs the average document length
/// across the corpus, which we do not have here — the log is the standard
/// corpus-statistics-free substitute and is monotonic in `tf`, so ranking still improves
/// with repetition, just with diminishing returns.
pub fn encode(text: &str) -> Sparse {
    let mut counts: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for token in tokenise(text) {
        *counts.entry(token_index(&token)).or_insert(0) += 1;
    }
    // Sorted so the same text always produces byte-identical output, which makes stored
    // points comparable and tests deterministic.
    let mut pairs: Vec<(u32, u32)> = counts.into_iter().collect();
    pairs.sort_unstable_by_key(|(i, _)| *i);
    let mut s = Sparse { indices: Vec::with_capacity(pairs.len()), values: Vec::with_capacity(pairs.len()) };
    for (index, tf) in pairs {
        s.indices.push(index);
        s.values.push(1.0 + (tf as f32).ln());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_alphanumeric_tokens_together() {
        assert_eq!(tokenise("Invoice INV-2026-0042 for GPT-4"), ["invoice", "inv", "2026", "0042", "for", "gpt"]);
    }

    /// The identifiers this index exists to find must survive tokenisation.
    #[test]
    fn digits_are_kept() {
        assert!(tokenise("see eq. (3) and H2SO4").contains(&"h2so4".to_string()));
        assert!(tokenise("total 1499.95 EUR").contains(&"1499".to_string()));
    }

    #[test]
    fn drops_single_chars_and_overlong_blobs() {
        assert_eq!(tokenise("a b ab"), ["ab"]);
        let blob = "x".repeat(MAX_TOKEN_LEN + 1);
        assert!(tokenise(&blob).is_empty());
        let longest = "y".repeat(MAX_TOKEN_LEN);
        assert_eq!(tokenise(&longest), [longest]);
    }

    #[test]
    fn empty_text_gives_empty_vector() {
        assert!(encode("").is_empty());
        assert!(encode("   ,.;  ").is_empty());
    }

    #[test]
    fn term_frequency_saturates() {
        let once = encode("alpha");
        let twice = encode("alpha alpha");
        let many = encode(&"alpha ".repeat(20));
        assert_eq!((once.len(), twice.len(), many.len()), (1, 1, 1));
        assert_eq!(once.values[0], 1.0);
        // Monotonic in tf, but 20 occurrences are worth far less than 20x one.
        assert!(twice.values[0] > once.values[0]);
        assert!(many.values[0] > twice.values[0]);
        assert!(many.values[0] < 4.0 * once.values[0]);
    }

    #[test]
    fn output_is_deterministic_and_sorted() {
        let a = encode("the quick brown fox jumps over the lazy dog");
        let b = encode("dog lazy the over jumps fox brown quick the");
        assert_eq!(a.indices, b.indices, "token order must not change the index set");
        assert!(a.indices.windows(2).all(|w| w[0] < w[1]), "indices must be sorted and unique");
    }

    /// Pinned so a future refactor cannot silently change stored points' index space.
    #[test]
    fn token_index_is_pinned() {
        assert_eq!(token_index(""), 0x811c_9dc5);
        assert_eq!(token_index("a"), 0xe40c_292c);
        assert_eq!(token_index("invoice"), token_index("invoice"));
        assert_ne!(token_index("invoice"), token_index("invoices"));
    }
}
