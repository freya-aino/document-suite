use serde::{Deserialize, Serialize};

use crate::{BBox, HashKey, PageProfile, Provenance, RunId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Title,
    Heading,
    Paragraph,
    List,
    Table,
    Figure,
    Chart,
    Formula,
    Caption,
    Footnote,
    Header,
    Footer,
    PageNumber,
    Reference,
    Handwriting,
    Stamp,
    Code,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockContent {
    Text { text: String },
    /// Table as HTML (`<table>` with rowspan/colspan); the canonical table form.
    Table { html: String },
    Formula { latex: String },
    Figure { crop_uri: String, description: Option<String> },
    Chart { crop_uri: String, data_html: Option<String>, description: Option<String> },
    Empty,
}

/// One layout region of a page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub block_hk: HashKey,
    pub index: u32,
    pub kind: BlockKind,
    pub bbox: BBox,
    /// Reading order on the page (0-based).
    pub reading_order: u32,
    pub content: BlockContent,
    /// Plain-text projection of `content`; evidence character spans refer to this.
    pub text: String,
    /// Heading level for `Heading` blocks (1 = top).
    pub level: Option<u8>,
    pub confidence: f32,
    pub provenance: Provenance,
}

/// One node of the DAG that produced a page, as it ran.
///
/// **Why this is in the stored result and not only in a log.** A block carries the
/// provenance of the node that *produced* it, which is enough to attribute a block and
/// not enough to attribute a page's time: a node that produced no block — an optional one
/// that failed, a conditioner, a verify pass that only re-scored — leaves no record at
/// all, and the executor's level structure is not recoverable from the blocks either. So
/// the one question a deployment most wants answered about latency, *was that our code or
/// the model*, could be answered in process and not from the stored page.
///
/// `level` is the executor's own: nodes sharing a level ran concurrently, so a page's
/// critical path is the sum over levels of the slowest node in each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeTrace {
    pub node_id: String,
    pub step_kind: String,
    /// `model@revision` for a model node; `None` is what makes a node host work.
    pub model_ref: Option<String>,
    pub prompt_ref: Option<String>,
    pub duration_ms: u64,
    /// Zero-based executor level: every node of a level ran at the same time.
    pub level: u32,
    /// `ok`, or `skipped_after_error` for an optional node that failed. A node that
    /// failed and was not optional has no trace, because the page has no result.
    pub outcome: String,
    /// What the step reported about how it ran (`StepOutput::metrics`) — for a model whose
    /// family is `auto`, which parse path won.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub metrics: std::collections::BTreeMap<String, serde_json::Value>,
}

impl NodeTrace {
    /// Whether this node's time was spent in a model rather than in our own code.
    pub fn is_model(&self) -> bool {
        self.model_ref.is_some()
    }
}

/// The critical path of a page's DAG, split into the part we are responsible for and the
/// part the model tier is: `(host_ms, model_ms)`.
///
/// Levels run concurrently, so a level costs its slowest node — and a level containing
/// both a model node and a host node is charged to whichever of the two was slower, not
/// to both. That is a deliberate under-count of the *other* side rather than an
/// over-count of the total: `host_ms + model_ms <= latency_ms` always, and the difference
/// is the executor's own overhead, which is worth seeing rather than hiding inside one of
/// the two numbers.
///
/// `host_ms` is the gateable quantity — it tests our code. `model_ms` is reported, and
/// comparable only against the same host's model probe.
pub fn critical_path_ms(traces: &[NodeTrace]) -> (u64, u64) {
    let last = traces.iter().map(|t| t.level).max();
    let Some(last) = last else { return (0, 0) };
    let (mut host, mut model) = (0u64, 0u64);
    for level in 0..=last {
        let slowest = traces.iter().filter(|t| t.level == level).max_by_key(|t| t.duration_ms);
        if let Some(t) = slowest {
            if t.is_model() {
                model += t.duration_ms;
            } else {
                host += t.duration_ms;
            }
        }
    }
    (host, model)
}

/// All blocks of one page produced by one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageResult {
    pub schema_version: u32,
    pub document_hk: HashKey,
    pub page_hk: HashKey,
    pub page_no: u32,
    pub run_id: RunId,
    pub render_width_px: u32,
    pub render_height_px: u32,
    pub profile: PageProfile,
    /// `dag_id@version` that produced this page.
    pub dag_ref: String,
    pub blocks: Vec<Block>,
    /// Every node that ran, in level order. Added in schema version 2; `default` so a
    /// version 1 result still reads, and empty on one.
    #[serde(default)]
    pub traces: Vec<NodeTrace>,
}

impl PageResult {
    /// 2 added `traces`. The bump is additive and `traces` is `#[serde(default)]`, so a
    /// stored version 1 page still deserialises — it simply has no node timings, which is
    /// the truth about it.
    pub const SCHEMA_VERSION: u32 = 2;

    /// `(host_ms, model_ms)` for this page, or `None` on a result old enough to have no
    /// traces — which must not be reported as zero.
    pub fn critical_path_ms(&self) -> Option<(u64, u64)> {
        (!self.traces.is_empty()).then(|| critical_path_ms(&self.traces))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(node: &str, level: u32, ms: u64, model: Option<&str>) -> NodeTrace {
        NodeTrace {
            node_id: node.into(),
            step_kind: "s".into(),
            model_ref: model.map(str::to_string),
            prompt_ref: None,
            duration_ms: ms,
            level,
            outcome: "ok".into(),
            metrics: Default::default(),
        }
    }

    #[test]
    fn a_level_costs_its_slowest_node_and_not_the_sum() {
        // Two model calls side by side are one model call's worth of latency. Summing
        // them is the mistake that makes a parallel two-parser DAG look twice as slow as
        // it is, which would have the search reject the shape for the wrong reason.
        let traces = [trace("a", 0, 400, Some("m@1")), trace("b", 0, 900, Some("m@1")), trace("merge", 1, 30, None)];
        assert_eq!(critical_path_ms(&traces), (30, 900));
    }

    #[test]
    fn a_level_is_charged_to_whichever_side_was_slower() {
        // A level holding a model node and a host node is one wait, not two. Charging
        // both would make `host_ms + model_ms` exceed the page's own latency, and
        // `host_ms` is meant to be gateable — an inflated one fails on a slow model.
        let mixed = [trace("model", 0, 500, Some("m@1")), trace("cpu", 0, 20, None)];
        assert_eq!(critical_path_ms(&mixed), (0, 500), "the model dominated its level");
        let host_bound = [trace("model", 0, 10, Some("m@1")), trace("cpu", 0, 700, None)];
        assert_eq!(critical_path_ms(&host_bound), (700, 0), "and here our own code did");
    }

    #[test]
    fn levels_add_up_across_the_page() {
        let traces = [
            trace("parse", 0, 1000, Some("m@1")),
            trace("layout", 0, 300, Some("m@1")),
            trace("ocr", 1, 600, Some("m@1")),
            trace("merge", 2, 40, None),
            trace("order", 3, 10, None),
        ];
        // 1000 + 600 of model, 40 + 10 of ours.
        assert_eq!(critical_path_ms(&traces), (50, 1600));
    }

    #[test]
    fn a_page_without_traces_reports_nothing_rather_than_zero() {
        // The distinction that matters for a stored result written before schema 2: zero
        // host milliseconds is a claim, and an absent measurement is not.
        assert_eq!(critical_path_ms(&[]), (0, 0));
        let mut page = PageResult {
            schema_version: 1,
            document_hk: HashKey::of(&["d"]),
            page_hk: HashKey::of(&["p"]),
            page_no: 1,
            run_id: RunId::new(),
            render_width_px: 10,
            render_height_px: 10,
            profile: crate::PageProfile {
                language: crate::LanguageTag::parse("en").expect("en"),
                doc_type: crate::DocType::Other,
                born_digital: true,
                text_layer_coverage: 1.0,
                has_handwriting: false,
                has_images: false,
                has_tables: false,
                has_formulas: false,
                has_charts: false,
                rotation_deg: 0,
                confidence: 1.0,
                tier: crate::ClassifierTier::Rules,
            },
            dag_ref: "d@1".into(),
            blocks: vec![],
            traces: vec![],
        };
        assert_eq!(page.critical_path_ms(), None);
        page.traces.push(trace("parse", 0, 7, Some("m@1")));
        assert_eq!(page.critical_path_ms(), Some((0, 7)));
    }

    #[test]
    fn a_version_one_page_still_deserialises() {
        // The whole reason `traces` is `#[serde(default)]`: stored pages exist.
        let v1 = serde_json::json!({
            "schema_version": 1,
            "document_hk": "a".repeat(64),
            "page_hk": "b".repeat(64),
            "page_no": 1,
            "run_id": "00000000-0000-0000-0000-000000000001",
            "render_width_px": 10,
            "render_height_px": 10,
            "profile": {
                "language": "en", "doc_type": "other", "born_digital": true, "text_layer_coverage": 1.0,
                "has_handwriting": false, "has_images": false, "has_tables": false, "has_formulas": false,
                "has_charts": false, "rotation_deg": 0, "confidence": 1.0, "tier": "rules"
            },
            "dag_ref": "d@1",
            "blocks": []
        });
        let page: PageResult = serde_json::from_value(v1).expect("a version 1 page must still read");
        assert!(page.traces.is_empty());
        assert_eq!(page.critical_path_ms(), None);
    }
}
