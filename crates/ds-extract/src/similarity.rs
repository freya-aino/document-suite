//! Vector and string similarity primitives shared by type normalisation and corpus
//! entity resolution.
//!
//! These are pure and deliberately dependency-free. They were written for the corpus
//! resolver and are kept here, below any store crate, so a change of vector store cannot
//! change how two entities are judged to be the same.

/// Cosine similarity in `-1..=1`, or `0.0` when either side is empty or the lengths
/// differ. Returning `0.0` on a length mismatch is intentional: vectors of different
/// dimension come from different embedding models, so they are not comparable and
/// truncating to the shorter one would invent a similarity.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
}

/// Jaro similarity over chars, in `0..=1`.
pub fn jaro(a: &str, b: &str) -> f32 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let (mut a_hit, mut b_hit) = (vec![false; a.len()], vec![false; b.len()]);
    let mut matches = 0usize;
    for (i, ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if !b_hit[j] && b[j] == *ca {
                a_hit[i] = true;
                b_hit[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let mut transpositions = 0usize;
    let mut j = 0;
    for (i, &hit) in a_hit.iter().enumerate() {
        if !hit {
            continue;
        }
        while !b_hit[j] {
            j += 1;
        }
        if a[i] != b[j] {
            transpositions += 1;
        }
        j += 1;
    }
    let m = matches as f32;
    (m / a.len() as f32 + m / b.len() as f32 + (m - (transpositions / 2) as f32) / m) / 3.0
}

/// Jaro-Winkler with the standard prefix scale 0.1 over at most 4 leading chars.
pub fn jaro_winkler(a: &str, b: &str) -> f32 {
    let j = jaro(a, b);
    let prefix = a.chars().zip(b.chars()).take(4).take_while(|(x, y)| x == y).count() as f32;
    j + prefix * 0.1 * (1.0 - j)
}

/// Name key for **corpus** matching: whitespace collapsed, lowercased. Nothing else.
///
/// This is load-bearing: it feeds `entity_hk = H(tenant | type_id | name)`, so two runs
/// that both miss the vector search still converge on one entity. Changing it orphans
/// every entity already stored.
///
/// It is deliberately *not* [`crate::resolve::normalise_name`], which additionally strips
/// surrounding punctuation and a leading article. That one is for matching mentions
/// inside one document, where being aggressive is cheap to get wrong; this one decides a
/// permanent key.
pub fn normalise_name(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_normalisation() {
        assert_eq!(normalise_name("  Vision   Transformer "), "vision transformer");
    }

    /// The corpus key keeps what in-document matching throws away.
    #[test]
    fn corpus_normalisation_differs_from_in_document() {
        assert_eq!(normalise_name("The Pile."), "the pile.");
        assert_eq!(crate::resolve::normalise_name("The Pile."), "pile");
    }

    #[test]
    fn jaro_winkler_reference_values() {
        assert_eq!(jaro_winkler("", ""), 1.0);
        assert_eq!(jaro_winkler("abc", ""), 0.0);
        assert_eq!(jaro_winkler("bert", "bert"), 1.0);
        assert!((jaro("martha", "marhta") - 0.9444).abs() < 1e-3);
        assert!((jaro_winkler("martha", "marhta") - 0.9611).abs() < 1e-3);
        assert!((jaro_winkler("dwayne", "duane") - 0.84).abs() < 1e-3);
        assert!(jaro_winkler("bert", "gpt-3") < 0.5);
    }

    #[test]
    fn cosine_basics() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0]), 1.0);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        // Different dimensions are not comparable.
        assert_eq!(cosine(&[1.0, 0.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
