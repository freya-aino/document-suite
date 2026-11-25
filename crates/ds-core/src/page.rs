use serde::{Deserialize, Serialize};

use crate::LanguageTag;

/// Coarse document type, decided per page (pages of one file may differ).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocType {
    ScientificPaper,
    Report,
    Invoice,
    Receipt,
    Form,
    Contract,
    Letter,
    Slide,
    Drawing,
    HandwrittenNote,
    Other,
}

/// Which classifier tier produced the profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierTier {
    /// Rules over PDF structure and text layer only (CPU, no model call).
    Rules,
    /// Rules plus the small vision classifier.
    Vision,
}

/// Output of page classification; the only input the router needs about the page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageProfile {
    pub language: LanguageTag,
    pub doc_type: DocType,
    /// Page has a usable native text layer (already OCR'd or born-digital).
    pub born_digital: bool,
    /// Fraction of visible text area covered by the native text layer, 0..1.
    pub text_layer_coverage: f32,
    pub has_handwriting: bool,
    pub has_images: bool,
    pub has_tables: bool,
    pub has_formulas: bool,
    pub has_charts: bool,
    /// Detected page rotation in degrees (0, 90, 180, 270).
    pub rotation_deg: u16,
    /// Overall classifier confidence, 0..1.
    pub confidence: f32,
    pub tier: ClassifierTier,
}

/// Where a document's type came from. Recorded because the sources are not equally
/// trustworthy, and a reviewer looking at a stored type needs to know which it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocTypeSource {
    /// Supplied by the caller. Trusted over anything inferred.
    Caller,
    /// A model that looked at several pages at once.
    Model,
    /// Aggregated from per-page classification.
    PageVote,
}

/// A document's type with the confidence behind it.
///
/// The document-level analogue of [`PageProfile`]. Lives here, not in `ds-classify`, so
/// the stores can carry it without depending on the classifier.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DocumentType {
    pub doc_type: DocType,
    /// Share of the evidence behind the winner, `0..=1`. Not a probability: it says how
    /// much of what was looked at agreed.
    pub confidence: f32,
    pub source: DocTypeSource,
}
