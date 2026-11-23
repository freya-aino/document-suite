//! Page classification. See `SPEC.md`.

pub mod document;
pub use document::{document_type, resolve as resolve_doc_type, PageVote};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use vs_core::{ClassifierTier, DocType, DocumentType, LanguageTag, PageProfile};
use vs_dag::PageImage;

/// Cheap signals computed by ingest from the PDF structure / image.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageSignals {
    pub text_chars: u32,
    pub text_layer_coverage: f32,
    /// Invisible text layer over a full-page image (pre-OCR'd scan).
    pub ocr_text_layer: bool,
    pub image_count: u32,
    pub image_area_ratio: f32,
    /// How far the source page is rotated from upright, clockwise: 0, 90, 180 or 270.
    ///
    /// **Not a correction to apply.** For a PDF it is the page's own `/Rotate`, which
    /// pdfium has already applied to the rendered bitmap; for a raster it is measured
    /// from the ink and the bitmap has *not* been turned. In both cases the page image a
    /// step receives may still be sideways — `page.orient` is what turns it.
    pub rotation_deg: u16,
    /// Fraction of the page darker than its Otsu threshold: how much ink is on it.
    ///
    /// The one measured signal a scan can offer. Without it a raster page reaches tier 1
    /// with nothing but a thumbnail, so a blank scan costs the same GPU call as a dense
    /// one. `0.0` also means "not measured", which is why it only ever *skips* work.
    #[serde(default)]
    pub ink_coverage: f32,
    /// Long straight ruled lines on the page, horizontal and vertical together.
    ///
    /// The signal that tells a form or a table from prose *before* any model looks. Only
    /// measured for rasters, where tier 0 had nothing else to go on; a PDF's own text
    /// layer and image objects are better evidence than pixels. `0` means "not
    /// measured", so it only ever adds information.
    #[serde(default)]
    pub ruling_lines: u16,
    /// Fraction of the page in the mid-grey band: how continuous-tone it is.
    ///
    /// A document is bimodal — paper and ink — and a photograph is not. `0.0` means "not
    /// measured" here too, so a PDF page is never called a photograph on this basis.
    #[serde(default)]
    pub tone_continuity: f32,
}

/// Output of the tier 1 vision classifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisionSignals {
    pub doc_type: DocType,
    pub doc_type_confidence: f32,
    pub p_handwriting: f32,
    pub p_tables: f32,
    pub p_formulas: f32,
    pub p_charts: f32,
    pub p_images: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClassifyConfig {
    pub born_digital_min_chars: u32,
    pub born_digital_min_coverage: f32,
    pub min_image_area_ratio: f32,
    pub trust_ocr_layer: bool,
    pub skip_vision_for_text_only_digital: bool,
    pub flag_threshold: f32,
    pub rules_only_confidence: f32,
    pub thumbnail_long_side_px: u32,
    /// Ink coverage at or below which a scan counts as blank, so tier 1 is skipped.
    /// Set to 0 to disable, which is also the effect on inputs that cannot measure it.
    pub blank_ink_coverage: f32,
    /// Ruled lines at or above which a raster page is treated as ruled — a form, a
    /// table, a ledger. Six is two horizontals and four verticals, or the other way
    /// round: fewer than that is an underline and a page edge.
    pub ruled_min_lines: u16,
    /// Confidence reported when the rules ran and nothing determined the document type:
    /// no caller hint, no ruled page, no vision model.
    ///
    /// Deliberately below the routing table's default `min_classifier_confidence` (0.6),
    /// so such a page takes the low-confidence DAG — which is what that DAG is for. The
    /// alternative, which is what happened before, is a page claiming 0.85 confidence in
    /// a document type of `other` that nothing ever looked for.
    pub unknown_doc_type_confidence: f32,
    /// Mid-tone fraction at or above which a raster page is treated as a photograph.
    ///
    /// Well above a scan's own grey — paper and shadow put a few per cent in the middle —
    /// and below a photograph's, which is typically over half.
    pub photo_min_tone_continuity: f32,
}

impl Default for ClassifyConfig {
    fn default() -> Self {
        Self {
            born_digital_min_chars: 50,
            born_digital_min_coverage: 0.8,
            min_image_area_ratio: 0.02,
            trust_ocr_layer: false,
            skip_vision_for_text_only_digital: true,
            flag_threshold: 0.5,
            rules_only_confidence: 0.85,
            thumbnail_long_side_px: 768,
            // Deliberately low: a page with a faint signature or a pale logo must not be
            // mistaken for blank, so this only catches pages with essentially nothing.
            blank_ink_coverage: 0.002,
            unknown_doc_type_confidence: 0.3,
            ruled_min_lines: 6,
            photo_min_tone_continuity: 0.35,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier0 {
    pub born_digital: bool,
    pub has_images: bool,
    pub needs_vision: bool,
    /// Measured as having essentially no ink. Only ever set for inputs that report
    /// `ink_coverage`, so a PDF page is never flagged blank this way.
    pub blank: bool,
    /// Ruled like a form or a table, from `ruling_lines`. Raster inputs only.
    ///
    /// This is what tier 0 could not say before: its only measured signal was how much
    /// ink a page had, so a form, a photograph and a page of prose all reached the vision
    /// tier indistinguishable — and on a deployment with no vision model, every
    /// rules-only page claimed to have no tables whatever was printed on it.
    pub ruled: bool,
    /// Continuous-tone like a photograph, from `tone_continuity`. Raster inputs only.
    pub photographic: bool,
}

pub fn tier0(s: &PageSignals, cfg: &ClassifyConfig) -> Tier0 {
    let born_digital = s.text_chars >= cfg.born_digital_min_chars
        && s.text_layer_coverage >= cfg.born_digital_min_coverage
        && (!s.ocr_text_layer || cfg.trust_ocr_layer);
    let has_images = s.image_count > 0 && s.image_area_ratio >= cfg.min_image_area_ratio;
    // `> 0.0` keeps an unmeasured page (ink_coverage defaulting to 0.0) out of this:
    // "we did not look" must not be read as "there is nothing there".
    let blank = cfg.blank_ink_coverage > 0.0 && s.ink_coverage > 0.0 && s.ink_coverage <= cfg.blank_ink_coverage;
    // Both are raster-only measurements, and both read 0 as "not measured" rather than as
    // "none", so a PDF page is never called ruled or photographic on this basis.
    let ruled = !blank && cfg.ruled_min_lines > 0 && s.ruling_lines >= cfg.ruled_min_lines;
    let photographic =
        !blank && s.tone_continuity > 0.0 && s.tone_continuity >= cfg.photo_min_tone_continuity && !ruled;
    let text_only_digital = born_digital && !has_images && cfg.skip_vision_for_text_only_digital;
    Tier0 {
        born_digital,
        // A photograph *is* an image, whatever the container says about embedded image
        // objects — which for a scan is "one, the whole page", and for a phone photo the
        // same. The measurement is what distinguishes the two cases.
        has_images: (has_images || photographic) && !blank,
        needs_vision: !(text_only_digital || blank),
        blank,
        ruled,
        photographic,
    }
}

/// Combines tier 0 and (optional) tier 1 into a profile.
pub fn combine(
    language: LanguageTag,
    s: &PageSignals,
    t0: Tier0,
    vision: Option<&VisionSignals>,
    known_doc_type: Option<DocType>,
    cfg: &ClassifyConfig,
) -> PageProfile {
    match vision {
        None => PageProfile {
            language,
            // A ruled page with no caller hint and no vision model is a form: that is
            // what ruling lines mean, and `other` was the answer only because nothing
            // measured them.
            doc_type: known_doc_type.unwrap_or(if t0.ruled { DocType::Form } else { DocType::Other }),
            born_digital: t0.born_digital,
            text_layer_coverage: s.text_layer_coverage,
            has_handwriting: false,
            has_images: t0.has_images,
            // Rules on a page are a table or a form's field grid. Without this, a
            // rules-only deployment told every router that no page had a table.
            has_tables: t0.ruled,
            has_formulas: false,
            has_charts: false,
            rotation_deg: s.rotation_deg,
            // The confidence is about the **document type**, because that is what the
            // routing table thresholds on (`min_classifier_confidence`) — and on a
            // rules-only page the type is either known or guessed, with nothing in
            // between. Reporting `rules_only_confidence` for a page whose type nobody
            // determined is how a page with no class at all used to sail past a threshold
            // meant to catch exactly that: measured at 85 % expected calibration error on
            // the first corpus this was ever scored against (`vs-adapt classify-score`).
            //
            // The flags are a different matter and are as reliable as ever: tier 0
            // measures them. This number does not speak for them.
            confidence: if known_doc_type.is_some() || t0.ruled { cfg.rules_only_confidence } else { cfg.unknown_doc_type_confidence },
            tier: ClassifierTier::Rules,
        },
        Some(v) => {
            let flag = |p: f32| p >= cfg.flag_threshold;
            let certainty = |p: f32| ((p - 0.5).abs() * 2.0).clamp(0.0, 1.0);
            let ps = [v.p_handwriting, v.p_tables, v.p_formulas, v.p_charts, v.p_images];
            let mean_certainty = ps.iter().map(|p| certainty(*p)).sum::<f32>() / ps.len() as f32;
            PageProfile {
                language,
                doc_type: v.doc_type,
                born_digital: t0.born_digital,
                text_layer_coverage: s.text_layer_coverage,
                has_handwriting: flag(v.p_handwriting),
                has_images: t0.has_images || flag(v.p_images),
                // The model's answer, or the page's own rules. A model that misses a
                // faint field grid is the common failure here, and a measurement that
                // disagrees with it is evidence, not noise.
                has_tables: flag(v.p_tables) || t0.ruled,
                has_formulas: flag(v.p_formulas),
                has_charts: flag(v.p_charts),
                rotation_deg: s.rotation_deg,
                confidence: (v.doc_type_confidence * mean_certainty).clamp(0.0, 1.0),
                tier: ClassifierTier::Vision,
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClassifyError {
    #[error("vision classifier failed: {0}")]
    Vision(String),
}

/// Document-level classifier; implemented in `vs-models` on top of a small VLM.
///
/// Separate from [`VisionClassifier`] because it answers a different question: not "what
/// is this page" but "what is this document", which needs several pages in one call.
#[async_trait]
pub trait DocumentClassifier: Send + Sync {
    /// Classifies a document from its opening page thumbnails, in page order.
    async fn classify(&self, thumbnails: &[PageImage], language: &LanguageTag) -> Result<DocumentType, ClassifyError>;
}

/// Tier 1 classifier; implemented in `vs-models` on top of a small VLM.
#[async_trait]
pub trait VisionClassifier: Send + Sync {
    async fn classify(&self, thumbnail: &PageImage, language: &LanguageTag) -> Result<VisionSignals, ClassifyError>;
}

pub struct Classifier {
    pub cfg: ClassifyConfig,
    pub vision: Arc<dyn VisionClassifier>,
}

impl Classifier {
    /// `thumbnail` must already be downscaled to `cfg.thumbnail_long_side_px`.
    pub async fn classify(
        &self,
        language: LanguageTag,
        signals: &PageSignals,
        thumbnail: &PageImage,
        known_doc_type: Option<DocType>,
    ) -> Result<PageProfile, ClassifyError> {
        let t0 = tier0(signals, &self.cfg);
        let vision = if t0.needs_vision { Some(self.vision.classify(thumbnail, &language).await?) } else { None };
        Ok(combine(language, signals, t0, vision.as_ref(), known_doc_type, &self.cfg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digital() -> PageSignals {
        PageSignals { text_chars: 2000, text_layer_coverage: 0.95, ..Default::default() }
    }

    #[test]
    fn text_only_digital_skips_vision() {
        let t = tier0(&digital(), &ClassifyConfig::default());
        assert_eq!(
            t,
            Tier0 { born_digital: true, has_images: false, needs_vision: false, blank: false, ruled: false, photographic: false },
            "a PDF page reports neither ruling lines nor tone, so tier 0 must claim nothing about either"
        );
    }

    /// The confidence on a rules-only page is about the **document type**, because that
    /// is what the routing table thresholds on. A page whose type nobody determined used
    /// to claim 0.85 — and so sailed past `min_classifier_confidence`, which exists to
    /// catch exactly that page. Measured at 85 % expected calibration error the first
    /// time a corpus was scored against it (`vs-adapt classify-score`).
    #[test]
    fn a_rules_only_page_whose_type_nobody_determined_says_so() {
        let cfg = ClassifyConfig::default();
        let digital = PageSignals { text_chars: 2000, text_layer_coverage: 0.95, ..Default::default() };
        let t0 = tier0(&digital, &cfg);
        let lang = || LanguageTag::parse("en").expect("lang");

        let guessed = combine(lang(), &digital, t0, None, None, &cfg);
        assert_eq!(guessed.doc_type, DocType::Other);
        assert!(
            guessed.confidence < 0.6,
            "a page with no determined type must fall below the routing table's default threshold, got {}",
            guessed.confidence
        );

        // A caller who said what it is, and a page whose rules *did* determine a type,
        // both report the rules-only confidence — they determined something.
        let hinted = combine(lang(), &digital, t0, None, Some(DocType::Invoice), &cfg);
        assert_eq!(hinted.confidence, cfg.rules_only_confidence);
        let scan = PageSignals { image_count: 1, image_area_ratio: 1.0, ink_coverage: 0.08, ruling_lines: 12, ..Default::default() };
        let ruled = combine(lang(), &scan, tier0(&scan, &cfg), None, None, &cfg);
        assert_eq!(ruled.doc_type, DocType::Form);
        assert_eq!(ruled.confidence, cfg.rules_only_confidence, "ruling lines are a determination, not a guess");
    }

    /// What tier 0 could not say before: whether a page nobody has read is a form, a
    /// photograph, or prose. Its only measured signal was how much ink there was.
    #[test]
    fn a_ruled_scan_is_a_form_and_a_continuous_tone_one_is_a_photograph() {
        let cfg = ClassifyConfig::default();
        let scan = |lines: u16, tone: f32| PageSignals {
            image_count: 1,
            image_area_ratio: 1.0,
            ink_coverage: 0.08,
            ruling_lines: lines,
            tone_continuity: tone,
            ..Default::default()
        };

        let form = tier0(&scan(12, 0.02), &cfg);
        assert!(form.ruled && !form.photographic);
        let photo = tier0(&scan(0, 0.6), &cfg);
        assert!(photo.photographic && !photo.ruled);
        assert!(photo.has_images, "a photograph is an image whatever the container says");
        let prose = tier0(&scan(1, 0.02), &cfg);
        assert!(!prose.ruled && !prose.photographic, "one line is an underline, not a ruled page: {prose:?}");

        // A ruled page printed on grey stock, or a shadowed scan of a form: ruled wins,
        // because rules are a positive finding and mid-tone is an absence of contrast.
        let shadowed_form = tier0(&scan(12, 0.6), &cfg);
        assert!(shadowed_form.ruled && !shadowed_form.photographic);

        // The rules-only profile is where this matters most: with no vision model, every
        // page used to claim it had no tables.
        let profile = combine(LanguageTag::parse("en").expect("lang"), &scan(12, 0.02), form, None, None, &cfg);
        assert!(profile.has_tables, "a ruled page with no vision model still has a table on it");
        assert_eq!(profile.doc_type, DocType::Form);
        // And a caller's own hint still beats the measurement.
        let hinted = combine(LanguageTag::parse("en").expect("lang"), &scan(12, 0.02), form, None, Some(DocType::Invoice), &cfg);
        assert_eq!(hinted.doc_type, DocType::Invoice);
    }

    #[test]
    fn a_blank_scan_skips_the_vision_call() {
        let cfg = ClassifyConfig::default();
        let scan = |ink: f32| PageSignals { image_count: 1, image_area_ratio: 1.0, ink_coverage: ink, ..Default::default() };
        let t = tier0(&scan(0.0005), &cfg);
        assert!(t.blank && !t.needs_vision && !t.has_images, "a blank scan is not worth a GPU call: {t:?}");
        // A faint signature is not blank.
        assert!(!tier0(&scan(0.01), &cfg).blank);
        // Unmeasured (0.0) must not be mistaken for blank — it means nobody looked.
        let unmeasured = tier0(&scan(0.0), &cfg);
        assert!(!unmeasured.blank && unmeasured.needs_vision);
    }

    #[test]
    fn ocr_layer_not_trusted_by_default() {
        let s = PageSignals { ocr_text_layer: true, image_count: 1, image_area_ratio: 1.0, ..digital() };
        let t = tier0(&s, &ClassifyConfig::default());
        assert!(!t.born_digital && t.needs_vision);
        let cfg = ClassifyConfig { trust_ocr_layer: true, ..Default::default() };
        assert!(tier0(&s, &cfg).born_digital);
    }

    #[test]
    fn combine_with_vision() {
        let cfg = ClassifyConfig::default();
        let s = PageSignals::default(); // scan, no text layer
        let t0 = tier0(&s, &cfg);
        let v = VisionSignals {
            doc_type: DocType::Form,
            doc_type_confidence: 0.9,
            p_handwriting: 0.95,
            p_tables: 0.9,
            p_formulas: 0.05,
            p_charts: 0.0,
            p_images: 0.1,
        };
        let p = combine(LanguageTag::parse("nl").unwrap(), &s, t0, Some(&v), None, &cfg);
        assert!(p.has_handwriting && p.has_tables && !p.has_formulas && !p.born_digital);
        assert!(p.confidence > 0.6 && p.confidence <= 0.9);
        assert_eq!(p.tier, ClassifierTier::Vision);
    }
}
