//! Locating model-provided quotes in block text (hallucination guard).

/// Returns the `(char_start, char_end)` span of `quote` in `text`, in **characters**
/// (not bytes). Exact match first, then case-insensitive match with whitespace runs
/// collapsed. `None` if the quote is not in the text.
pub fn locate_span(text: &str, quote: &str) -> Option<(u32, u32)> {
    let quote = quote.trim();
    if quote.is_empty() {
        return None;
    }
    if let Some(byte_start) = text.find(quote) {
        let start = text[..byte_start].chars().count() as u32;
        return Some((start, start + quote.chars().count() as u32));
    }

    // Normalised search: map each normalised char back to its original char index.
    let (norm_text, map) = normalise_with_map(text);
    let (norm_quote, _) = normalise_with_map(quote);
    let norm_chars: Vec<char> = norm_text.chars().collect();
    let q: Vec<char> = norm_quote.chars().collect();
    if q.is_empty() || q.len() > norm_chars.len() {
        return None;
    }
    let pos = norm_chars.windows(q.len()).position(|w| w == q.as_slice())?;
    let start = map[pos];
    let end = map[pos + q.len() - 1] + 1;
    Some((start as u32, end as u32))
}

/// Lowercases and collapses whitespace runs to one space. Returns the normalised string and,
/// for each normalised char, the index of the original char it came from.
fn normalise_with_map(s: &str) -> (String, Vec<usize>) {
    let mut out = String::new();
    let mut map = Vec::new();
    let mut prev_space = true; // also trims leading whitespace
    for (i, c) in s.chars().enumerate() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                map.push(i);
                prev_space = true;
            }
        } else {
            for lc in c.to_lowercase() {
                out.push(lc);
                map.push(i);
            }
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
        map.pop();
    }
    (out, map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_in_chars() {
        let t = "Größe: BERT achieves 92.1 F1.";
        let (s, e) = locate_span(t, "BERT achieves 92.1").unwrap();
        let got: String = t.chars().skip(s as usize).take((e - s) as usize).collect();
        assert_eq!(got, "BERT achieves 92.1");
    }

    #[test]
    fn normalised_match() {
        let t = "Our  model\noutperforms the baseline.";
        let (s, e) = locate_span(t, "our model outperforms").unwrap();
        let got: String = t.chars().skip(s as usize).take((e - s) as usize).collect();
        assert_eq!(got, "Our  model\noutperforms");
    }

    #[test]
    fn missing_quote() {
        assert_eq!(locate_span("abc", "xyz"), None);
        assert_eq!(locate_span("abc", "  "), None);
    }
}
