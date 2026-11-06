//! Page classification. See `SPEC.md`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use vs_core::{ClassifierTier, DocType, LanguageTag, PageProfile};
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
    pub rotation_deg: u16,
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier0 {
    pub born_digital: bool,
    pub has_images: bool,
    pub needs_vision: bool,
}

pub fn tier0(s: &PageSignals, cfg: &ClassifyConfig) -> Tier0 {
    let born_digital = s.text_chars >= cfg.born_digital_min_chars
        && s.text_layer_coverage >= cfg.born_digital_min_coverage
        && (!s.ocr_text_layer || cfg.trust_ocr_layer);
    let has_images = s.image_count > 0 && s.image_area_ratio >= cfg.min_image_area_ratio;
    let needs_vision = !(born_digital && !has_images && cfg.skip_vision_for_text_only_digital);
    Tier0 { born_digital, has_images, needs_vision }
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
            doc_type: known_doc_type.unwrap_or(DocType::Other),
            born_digital: t0.born_digital,
            text_layer_coverage: s.text_layer_coverage,
            has_handwriting: false,
            has_images: t0.has_images,
            has_tables: false,
            has_formulas: false,
            has_charts: false,
            rotation_deg: s.rotation_deg,
            confidence: cfg.rules_only_confidence,
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
                has_tables: flag(v.p_tables),
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
        assert_eq!(t, Tier0 { born_digital: true, has_images: false, needs_vision: false });
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
