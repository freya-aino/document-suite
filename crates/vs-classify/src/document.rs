//! Document-level type: one answer for a document, from many page answers.
//!
//! Pages are classified independently, which is what makes Stage 1 parallel, but the
//! extraction profile and the stored record are per document. Something has to reconcile
//! eleven page verdicts into one, and the obvious way — most frequent wins — is wrong in
//! the common cases:
//!
//! - A two-page invoice whose second page is a blank continuation ties 1–1, and a
//!   plurality vote breaks the tie by page order, which is luck rather than evidence.
//! - A 40-page contract with a scanned signature page the classifier read as
//!   `handwritten_note` at 0.51 confidence counts that page exactly as much as the 39
//!   pages it was sure about.
//! - `Other` is not a document type, it is the absence of one. Counting it as a type lets
//!   a document of mostly-unremarkable pages outvote the title page that identifies it.
//!
//! So: weight each vote by the classifier's own confidence, weight early pages more
//! heavily, and treat `Other` as an abstention.

use vs_core::{DocType, DocTypeSource, DocumentType};

/// One page's opinion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageVote {
    pub page_no: u32,
    pub doc_type: DocType,
    /// The classifier's confidence in `doc_type`, `0..=1`.
    pub confidence: f32,
}

/// Extra weight for the opening pages. A title page, letterhead or invoice header
/// identifies a document; page 23 of it usually looks like page 23 of anything.
fn page_weight(page_no: u32) -> f32 {
    match page_no {
        0 | 1 => 3.0,
        2 => 1.5,
        _ => 1.0,
    }
}

/// Aggregates page votes into one document type.
///
/// Returns `None` only when there are no votes at all. A document whose every page was
/// classified `Other` yields `Other` — the honest answer, not an absent one.
pub fn document_type(votes: &[PageVote]) -> Option<DocumentType> {
    if votes.is_empty() {
        return None;
    }
    // `Other` abstains: it means the classifier had no opinion, so letting it win would
    // discard the pages that did. Only if nothing else got a vote does it stand.
    let opinionated: Vec<&PageVote> = votes.iter().filter(|v| v.doc_type != DocType::Other).collect();
    if opinionated.is_empty() {
        return Some(DocumentType { doc_type: DocType::Other, confidence: 0.0, source: DocTypeSource::PageVote });
    }

    let mut weights: Vec<(DocType, f32)> = Vec::new();
    let mut total = 0.0f32;
    for v in &opinionated {
        let w = v.confidence.clamp(0.0, 1.0) * page_weight(v.page_no);
        total += w;
        match weights.iter_mut().find(|(t, _)| *t == v.doc_type) {
            Some((_, acc)) => *acc += w,
            None => weights.push((v.doc_type, w)),
        }
    }
    // Every opinionated page had zero confidence: an opinion nobody stands behind is no
    // better than an abstention.
    if total <= 0.0 {
        return Some(DocumentType { doc_type: DocType::Other, confidence: 0.0, source: DocTypeSource::PageVote });
    }
    // Ties break towards the type whose earliest page is earliest, which is at least
    // evidence rather than hash order.
    let earliest = |t: DocType| opinionated.iter().filter(|v| v.doc_type == t).map(|v| v.page_no).min().unwrap_or(u32::MAX);
    let (doc_type, weight) = weights
        .into_iter()
        .reduce(|a, b| {
            if b.1 > a.1 || (b.1 == a.1 && earliest(b.0) < earliest(a.0)) {
                b
            } else {
                a
            }
        })
        .expect("non-empty");
    Some(DocumentType { doc_type, confidence: (weight / total).clamp(0.0, 1.0), source: DocTypeSource::PageVote })
}

/// The document type to use, preferring the most trustworthy source available.
///
/// A caller who says "these are invoices" knows more than a classifier guessing from
/// pixels, and a model that saw several pages at once knows more than a vote over pages
/// that each saw only themselves.
pub fn resolve(caller: Option<DocType>, model: Option<DocumentType>, votes: &[PageVote]) -> Option<DocumentType> {
    if let Some(doc_type) = caller {
        return Some(DocumentType { doc_type, confidence: 1.0, source: DocTypeSource::Caller });
    }
    model.or_else(|| document_type(votes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vote(page_no: u32, doc_type: DocType, confidence: f32) -> PageVote {
        PageVote { page_no, doc_type, confidence }
    }

    #[test]
    fn no_pages_means_no_answer() {
        assert_eq!(document_type(&[]), None);
    }

    #[test]
    fn a_confident_majority_wins() {
        let votes = vec![
            vote(1, DocType::ScientificPaper, 0.9),
            vote(2, DocType::ScientificPaper, 0.9),
            vote(3, DocType::ScientificPaper, 0.85),
        ];
        let got = document_type(&votes).unwrap();
        assert_eq!(got.doc_type, DocType::ScientificPaper);
        assert_eq!(got.confidence, 1.0, "unanimous pages mean all the weight agreed");
        assert_eq!(got.source, DocTypeSource::PageVote);
    }

    /// The case a plurality vote gets wrong: one unsure page against many sure ones.
    #[test]
    fn one_unsure_page_does_not_outweigh_many_sure_ones() {
        let mut votes = vec![vote(1, DocType::Contract, 0.95)];
        votes.extend((2..6).map(|p| vote(p, DocType::Contract, 0.9)));
        // A scanned signature page the classifier barely called handwriting.
        votes.push(vote(6, DocType::HandwrittenNote, 0.51));
        let got = document_type(&votes).unwrap();
        assert_eq!(got.doc_type, DocType::Contract);
        assert!(got.confidence > 0.85, "confidence was {}", got.confidence);
    }

    /// The other case it gets wrong: a two-page document tied 1-1.
    #[test]
    fn the_first_page_breaks_a_tie_on_evidence() {
        let got = document_type(&[vote(1, DocType::Invoice, 0.8), vote(2, DocType::Letter, 0.8)]).unwrap();
        assert_eq!(got.doc_type, DocType::Invoice, "the page that identifies a document is the first one");
        // Page weights are 3.0 and 1.5, so the share is 2/3: a real majority, and
        // reported as a less-than-certain one, which is the honest reading of a document
        // whose two pages disagree.
        assert!((got.confidence - 2.0 / 3.0).abs() < 0.01, "confidence was {}", got.confidence);
    }

    #[test]
    fn other_abstains_rather_than_voting() {
        // One identifying page, nine pages the classifier had no opinion about.
        let mut votes = vec![vote(1, DocType::Invoice, 0.9)];
        votes.extend((2..11).map(|p| vote(p, DocType::Other, 0.85)));
        let got = document_type(&votes).unwrap();
        assert_eq!(got.doc_type, DocType::Invoice, "`Other` is the absence of a type, not a type");
        assert_eq!(got.confidence, 1.0);
    }

    #[test]
    fn all_other_is_reported_honestly() {
        let votes: Vec<PageVote> = (1..4).map(|p| vote(p, DocType::Other, 0.85)).collect();
        let got = document_type(&votes).unwrap();
        assert_eq!((got.doc_type, got.confidence), (DocType::Other, 0.0));
    }

    /// An opinion with no confidence behind it is no better than an abstention.
    #[test]
    fn zero_confidence_votes_do_not_decide() {
        let got = document_type(&[vote(1, DocType::Slide, 0.0), vote(2, DocType::Slide, 0.0)]).unwrap();
        assert_eq!((got.doc_type, got.confidence), (DocType::Other, 0.0));
    }

    #[test]
    fn confidence_is_a_share_of_the_evidence() {
        // Equal weight either way on later pages: a genuinely mixed document.
        let got = document_type(&[vote(4, DocType::Form, 0.8), vote(5, DocType::Report, 0.8)]).unwrap();
        assert!((got.confidence - 0.5).abs() < 0.01, "a 50/50 split should read as 0.5, got {}", got.confidence);
    }

    #[test]
    fn out_of_range_confidence_is_clamped() {
        let got = document_type(&[vote(1, DocType::Form, 5.0), vote(2, DocType::Report, -1.0)]).unwrap();
        assert_eq!(got.doc_type, DocType::Form);
        assert!((0.0..=1.0).contains(&got.confidence));
    }

    #[test]
    fn the_caller_outranks_everything() {
        let votes = vec![vote(1, DocType::Letter, 0.99)];
        let model = Some(DocumentType { doc_type: DocType::Report, confidence: 0.9, source: DocTypeSource::Model });
        let got = resolve(Some(DocType::Invoice), model, &votes).unwrap();
        assert_eq!((got.doc_type, got.confidence, got.source), (DocType::Invoice, 1.0, DocTypeSource::Caller));
    }

    #[test]
    fn a_model_outranks_the_page_vote() {
        let votes = vec![vote(1, DocType::Letter, 0.99)];
        let model = DocumentType { doc_type: DocType::Report, confidence: 0.7, source: DocTypeSource::Model };
        assert_eq!(resolve(None, Some(model), &votes).unwrap().source, DocTypeSource::Model);
        // With no model, the vote stands.
        assert_eq!(resolve(None, None, &votes).unwrap().doc_type, DocType::Letter);
        assert_eq!(resolve(None, None, &[]), None);
    }
}
