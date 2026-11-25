//! Scoring the page classifier, and calibrating its confidence.
//!
//! Everything else here scores **DAG output**: what a workflow produced from a page.
//! Nothing scored the thing that *chose* the workflow, and the routing table's
//! `min_classifier_confidence` was therefore tuned blind — two gaps the component specs
//! both named and neither closed.
//!
//! Two numbers come out of this, and they answer different questions:
//!
//! - **Accuracy**, per class, because a wrong class costs quality or speed and which
//!   direction matters: a scientific paper read as a form takes a cheap DAG and loses
//!   its tables, a form read as a paper takes an expensive one and loses nothing.
//! - **Calibration**: within each confidence band, how often the classifier was right.
//!   A classifier that is 60 % accurate when it reports 0.9 is not a classifier whose
//!   threshold can be set from its confidence, and that is exactly what the routing
//!   table does with it.
//!
//! The label is the dataset's own `doc_type_hint` (or a per-page `doc_type` in the ground
//! truth). The classifier must therefore be run **without** that hint, or it would be
//! scored against its own input — `RunConfig::ignore_doc_type_hint` is what arranges
//! that, and the command that scores sets it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ds_core::{ClassifierTier, DocType, PageProfile};

/// Confidence bands for the calibration table. Chosen so the band the routing table's
/// default threshold (0.6) falls in has a boundary there.
const BANDS: [(f32, f32); 5] = [(0.0, 0.2), (0.2, 0.4), (0.4, 0.6), (0.6, 0.8), (0.8, 1.001)];

/// One labelled page: what it is, and what the classifier said.
#[derive(Debug, Clone)]
pub struct Judged {
    pub page_id: String,
    pub label: DocType,
    pub profile: PageProfile,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClassCounts {
    /// Labelled this class and classified as it.
    pub tp: usize,
    /// Classified as this class and labelled something else.
    pub fp: usize,
    /// Labelled this class and classified as something else.
    pub fna: usize,
}

impl ClassCounts {
    pub fn precision(&self) -> Option<f32> {
        let d = self.tp + self.fp;
        (d > 0).then(|| self.tp as f32 / d as f32)
    }

    pub fn recall(&self) -> Option<f32> {
        let d = self.tp + self.fna;
        (d > 0).then(|| self.tp as f32 / d as f32)
    }

    pub fn f1(&self) -> Option<f32> {
        match (self.precision(), self.recall()) {
            (Some(p), Some(r)) if p + r > 0.0 => Some(2.0 * p * r / (p + r)),
            _ => None,
        }
    }
}

/// One confidence band's accuracy: the calibration curve, bucketed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Band {
    pub low: f32,
    pub high: f32,
    pub pages: usize,
    pub correct: usize,
    /// Mean reported confidence in this band — the x of the calibration point.
    pub mean_confidence: f32,
}

impl Band {
    pub fn accuracy(&self) -> Option<f32> {
        (self.pages > 0).then(|| self.correct as f32 / self.pages as f32)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifierScore {
    pub pages: usize,
    pub correct: usize,
    /// Pages classified by rules alone, i.e. with no vision call. Reported because the
    /// two tiers are not the same classifier and a mixed accuracy hides which is weak.
    pub rules_only: usize,
    pub rules_only_correct: usize,
    pub by_class: BTreeMap<String, ClassCounts>,
    /// `label → predicted → count`, for the pairs that matter.
    pub confusion: BTreeMap<String, BTreeMap<String, usize>>,
    pub bands: Vec<Band>,
}

impl ClassifierScore {
    pub fn accuracy(&self) -> Option<f32> {
        (self.pages > 0).then(|| self.correct as f32 / self.pages as f32)
    }

    /// Accuracy weighted equally per class rather than per page, which is the number that
    /// survives an unbalanced corpus — and every document corpus is unbalanced.
    pub fn balanced_accuracy(&self) -> Option<f32> {
        let recalls: Vec<f32> = self.by_class.values().filter_map(|c| c.recall()).collect();
        (!recalls.is_empty()).then(|| recalls.iter().sum::<f32>() / recalls.len() as f32)
    }

    /// How far the reported confidence is from the accuracy it implies, averaged over
    /// bands and weighted by pages: the **expected calibration error**.
    ///
    /// Zero means a confidence of 0.8 is right 80 % of the time. The routing table's
    /// `min_classifier_confidence` is only a meaningful threshold when this is small,
    /// and the number was never measured before this.
    pub fn calibration_error(&self) -> Option<f32> {
        let total: usize = self.bands.iter().map(|b| b.pages).sum();
        if total == 0 {
            return None;
        }
        let err: f32 = self
            .bands
            .iter()
            .filter(|b| b.pages > 0)
            .map(|b| (b.pages as f32 / total as f32) * (b.mean_confidence - b.accuracy().unwrap_or(0.0)).abs())
            .sum();
        Some(err)
    }
}

/// Scores a set of labelled, classified pages.
pub fn score(judged: &[Judged]) -> ClassifierScore {
    let name = |d: DocType| serde_json::to_value(d).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "other".into());
    let mut s = ClassifierScore {
        pages: judged.len(),
        correct: 0,
        rules_only: 0,
        rules_only_correct: 0,
        by_class: BTreeMap::new(),
        confusion: BTreeMap::new(),
        bands: BANDS
            .iter()
            .map(|(low, high)| Band { low: *low, high: *high, pages: 0, correct: 0, mean_confidence: 0.0 })
            .collect(),
    };
    // Running sums for the bands' mean confidence.
    let mut band_conf = vec![0f32; s.bands.len()];

    for j in judged {
        let (label, predicted) = (j.label, j.profile.doc_type);
        let right = label == predicted;
        s.correct += right as usize;
        if j.profile.tier == ClassifierTier::Rules {
            s.rules_only += 1;
            s.rules_only_correct += right as usize;
        }
        s.by_class.entry(name(label)).or_default().fna += (!right) as usize;
        s.by_class.entry(name(label)).or_default().tp += right as usize;
        if !right {
            s.by_class.entry(name(predicted)).or_default().fp += 1;
        }
        *s.confusion.entry(name(label)).or_default().entry(name(predicted)).or_default() += 1;

        let c = j.profile.confidence.clamp(0.0, 1.0);
        if let Some(i) = s.bands.iter().position(|b| c >= b.low && c < b.high) {
            s.bands[i].pages += 1;
            s.bands[i].correct += right as usize;
            band_conf[i] += c;
        }
    }
    for (band, sum) in s.bands.iter_mut().zip(band_conf) {
        band.mean_confidence = if band.pages > 0 { sum / band.pages as f32 } else { 0.0 };
    }
    s
}

/// The Markdown section for `report.md`.
pub fn render(s: &ClassifierScore) -> String {
    let mut md = String::new();
    md.push_str("## Page classifier\n\n");
    if s.pages == 0 {
        md.push_str("No labelled pages: nothing to score. The label is the manifest's `doc_type_hint` \
                     or a `doc_type` in the ground truth, and the classifier is run without it.\n\n");
        return md;
    }
    let pct = |v: Option<f32>| v.map(|x| format!("{:.1} %", x * 100.0)).unwrap_or_else(|| "–".into());
    md.push_str(&format!("- Pages scored: **{}**, of which {} were classified by rules alone\n", s.pages, s.rules_only));
    md.push_str(&format!("- Accuracy: **{}** (balanced across classes: {})\n", pct(s.accuracy()), pct(s.balanced_accuracy())));
    if s.rules_only > 0 {
        let rules_acc = Some(s.rules_only_correct as f32 / s.rules_only as f32);
        md.push_str(&format!("- Rules-only pages: {} — the two tiers are different classifiers, and a mixed number hides which is weak\n", pct(rules_acc)));
    }
    md.push_str(&format!(
        "- Expected calibration error: **{}** — how far the reported confidence is from the accuracy it implies. \
         `min_classifier_confidence` in the routing table is only a meaningful threshold while this is small.\n\n",
        pct(s.calibration_error())
    ));

    md.push_str("| Class | Precision | Recall | F1 | Labelled | Predicted |\n|---|---|---|---|---|---|\n");
    for (class, c) in &s.by_class {
        md.push_str(&format!(
            "| `{class}` | {} | {} | {} | {} | {} |\n",
            pct(c.precision()),
            pct(c.recall()),
            pct(c.f1()),
            c.tp + c.fna,
            c.tp + c.fp
        ));
    }
    md.push('\n');

    md.push_str("### Calibration\n\n| Confidence | Pages | Mean reported | Accuracy |\n|---|---|---|---|\n");
    for b in s.bands.iter().filter(|b| b.pages > 0) {
        md.push_str(&format!(
            "| {:.1}–{:.1} | {} | {:.2} | {} |\n",
            b.low,
            b.high.min(1.0),
            b.pages,
            b.mean_confidence,
            pct(b.accuracy())
        ));
    }
    md.push('\n');

    // Only the confusions that happened, and only the ones worth a line.
    let mut rows: Vec<(String, String, usize)> = Vec::new();
    for (label, preds) in &s.confusion {
        for (predicted, n) in preds {
            if predicted != label {
                rows.push((label.clone(), predicted.clone(), *n));
            }
        }
    }
    rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    if !rows.is_empty() {
        md.push_str("### Confusions\n\nWhich way the classifier was wrong, worst first. The direction matters: a paper \
                     read as a form takes a cheap DAG and loses its tables, a form read as a paper takes an expensive one \
                     and loses nothing.\n\n");
        md.push_str("| Labelled | Classified as | Pages |\n|---|---|---|\n");
        for (label, pred, n) in rows.iter().take(15) {
            md.push_str(&format!("| `{label}` | `{pred}` | {n} |\n"));
        }
        md.push('\n');
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::LanguageTag;

    fn judged(label: DocType, predicted: DocType, confidence: f32, tier: ClassifierTier) -> Judged {
        Judged {
            page_id: format!("{label:?}/{predicted:?}/{confidence}"),
            label,
            profile: PageProfile {
                language: LanguageTag::parse("en").expect("lang"),
                doc_type: predicted,
                born_digital: true,
                text_layer_coverage: 1.0,
                has_handwriting: false,
                has_images: false,
                has_tables: false,
                has_formulas: false,
                has_charts: false,
                rotation_deg: 0,
                confidence,
                tier,
            },
        }
    }

    #[test]
    fn a_perfect_classifier_scores_perfectly_and_is_calibrated() {
        let pages: Vec<Judged> = (0..10)
            .map(|i| {
                let c = if i % 2 == 0 { DocType::ScientificPaper } else { DocType::Invoice };
                judged(c, c, 1.0, ClassifierTier::Vision)
            })
            .collect();
        let s = score(&pages);
        assert_eq!(s.accuracy(), Some(1.0));
        assert_eq!(s.balanced_accuracy(), Some(1.0));
        assert_eq!(s.calibration_error(), Some(0.0), "always right and always certain is calibrated");
        assert!(s.confusion.values().all(|p| p.len() == 1));
    }

    #[test]
    fn precision_and_recall_separate_the_two_ways_of_being_wrong() {
        // Four invoices, two of them read as forms; two forms, both read correctly.
        let pages = vec![
            judged(DocType::Invoice, DocType::Invoice, 0.9, ClassifierTier::Vision),
            judged(DocType::Invoice, DocType::Invoice, 0.9, ClassifierTier::Vision),
            judged(DocType::Invoice, DocType::Form, 0.9, ClassifierTier::Vision),
            judged(DocType::Invoice, DocType::Form, 0.9, ClassifierTier::Vision),
            judged(DocType::Form, DocType::Form, 0.9, ClassifierTier::Vision),
            judged(DocType::Form, DocType::Form, 0.9, ClassifierTier::Vision),
        ];
        let s = score(&pages);
        assert_eq!(s.accuracy(), Some(4.0 / 6.0));
        let invoice = &s.by_class["invoice"];
        assert_eq!((invoice.tp, invoice.fp, invoice.fna), (2, 0, 2));
        assert_eq!(invoice.recall(), Some(0.5), "half the invoices were missed");
        assert_eq!(invoice.precision(), Some(1.0), "nothing else was called an invoice");
        let form = &s.by_class["form"];
        assert_eq!((form.tp, form.fp, form.fna), (2, 2, 0));
        assert_eq!(form.precision(), Some(0.5));
        assert_eq!(form.recall(), Some(1.0));

        // The confusion is directional, and the report says which way.
        assert_eq!(s.confusion["invoice"]["form"], 2);
        assert!(!s.confusion.contains_key("report"));
        let md = render(&s);
        assert!(md.contains("### Confusions"));
        assert!(md.contains("| `invoice` | `form` | 2 |"), "{md}");
    }

    /// The number the routing table's threshold depends on: a classifier that is confident
    /// and wrong is worse than one that is unsure and wrong, because the table only
    /// re-routes on the second.
    #[test]
    fn calibration_error_catches_confident_mistakes() {
        let overconfident: Vec<Judged> = (0..10)
            .map(|i| {
                let label = DocType::Invoice;
                let predicted = if i < 5 { DocType::Invoice } else { DocType::Form };
                judged(label, predicted, 0.95, ClassifierTier::Vision)
            })
            .collect();
        let s = score(&overconfident);
        assert_eq!(s.accuracy(), Some(0.5));
        let err = s.calibration_error().expect("pages were scored");
        assert!(err > 0.4, "0.95 confidence at 50 % accuracy is badly calibrated, got {err}");

        // And an honest one, reporting a confidence that matches how often it is right.
        let honest: Vec<Judged> = (0..10)
            .map(|i| {
                let label = DocType::Invoice;
                let predicted = if i < 5 { DocType::Invoice } else { DocType::Form };
                judged(label, predicted, 0.5, ClassifierTier::Vision)
            })
            .collect();
        let err = score(&honest).calibration_error().expect("pages");
        assert!(err < 0.05, "a classifier reporting 0.5 and right half the time is calibrated, got {err}");
    }

    #[test]
    fn the_two_tiers_are_reported_apart() {
        let pages = vec![
            judged(DocType::Invoice, DocType::Invoice, 0.85, ClassifierTier::Rules),
            judged(DocType::Invoice, DocType::Other, 0.85, ClassifierTier::Rules),
            judged(DocType::Invoice, DocType::Invoice, 0.9, ClassifierTier::Vision),
        ];
        let s = score(&pages);
        assert_eq!((s.rules_only, s.rules_only_correct), (2, 1));
        assert!(render(&s).contains("Rules-only pages: 50.0 %"));
    }

    #[test]
    fn nothing_to_score_says_so_rather_than_claiming_a_number() {
        let s = score(&[]);
        assert_eq!(s.accuracy(), None);
        assert_eq!(s.calibration_error(), None);
        assert!(render(&s).contains("No labelled pages"));
    }
}
