//! Visual relations: figure/chart descriptions as a source, with `Evidence::Region`.
//!
//! Two paths:
//! 1. A `figure.describe` block whose description holds the step's JSON (`entities`,
//!    `relations`) yields entities and binary assertions directly (confidence 0.7).
//! 2. Assertions that the LLM grounded in a figure/chart block's description text get their
//!    span evidence replaced by the figure's region and their confidence capped at 0.7.

use std::collections::HashMap;

use serde::Deserialize;
use vs_core::{BlockContent, BlockKind, Evidence, HashKey};
use vs_stitch::StitchedDocument;

use crate::ground::locate_span;
use crate::model::{Arg, ArgValue, Assertion, AssertionKind, DocEntity, ExtractionOutput, Fragment, Mention, Qualifiers};

pub const VISUAL_CONFIDENCE: f32 = 0.7;

#[derive(Debug, Deserialize)]
struct FigureJson {
    #[serde(default)]
    entities: Vec<FigureEntity>,
    #[serde(default)]
    relations: Vec<FigureRelation>,
}

#[derive(Debug, Deserialize)]
struct FigureEntity {
    label: String,
    #[serde(rename = "type", default)]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct FigureRelation {
    subject: String,
    predicate: String,
    object: String,
}

fn description_of(content: &BlockContent) -> Option<&str> {
    match content {
        BlockContent::Figure { description: Some(d), .. } | BlockContent::Chart { description: Some(d), .. } => Some(d.as_str()),
        _ => None,
    }
}

/// Entities/relations from structured figure descriptions (path 1).
pub fn extract_figures(doc: &StitchedDocument, provenance_idx: usize) -> Fragment {
    let mut frag = Fragment::default();
    for (i, b) in doc.blocks.iter().enumerate() {
        if !doc.is_body(i) {
            continue;
        }
        let Some(desc) = description_of(&b.block.content) else { continue };
        let Ok(fig) = serde_json::from_str::<FigureJson>(desc.trim()) else { continue };
        if fig.entities.is_empty() {
            continue;
        }
        let pre = format!("fig{i}");
        let region = Evidence::Region { page_hk: doc.blocks[i].page_hk.clone(), bbox: b.block.bbox };
        let mut by_label: HashMap<String, String> = HashMap::new();
        for (k, e) in fig.entities.iter().enumerate() {
            let label = e.label.trim();
            if label.is_empty() {
                continue;
            }
            let (cs, ce) = locate_span(&b.block.text, label).unwrap_or((0, 0));
            let mid = format!("{pre}.m{}", k + 1);
            let eid = format!("{pre}.e{}", k + 1);
            frag.mentions.push(Mention {
                id: mid.clone(),
                block_hk: b.block.block_hk.clone(),
                char_start: cs,
                char_end: ce,
                surface: label.to_string(),
                raw_type: e.kind.clone(),
                confidence: VISUAL_CONFIDENCE,
            });
            frag.entities.push(DocEntity { local_id: eid.clone(), name: label.to_string(), raw_type: e.kind.clone(), type_id: None, aliases: vec![], mentions: vec![mid], entity_hk: None });
            by_label.entry(label.to_lowercase()).or_insert(eid);
        }
        for (k, r) in fig.relations.iter().enumerate() {
            let (Some(s), Some(o)) = (by_label.get(&r.subject.trim().to_lowercase()), by_label.get(&r.object.trim().to_lowercase())) else { continue };
            if r.predicate.trim().is_empty() {
                continue;
            }
            frag.assertions.push(Assertion {
                local_id: format!("{pre}.a{}", k + 1),
                predicate: r.predicate.trim().to_string(),
                predicate_id: None,
                kind: AssertionKind::Binary,
                args: vec![
                    Arg { role: "subject".into(), value: ArgValue::Entity { local_id: s.clone() } },
                    Arg { role: "object".into(), value: ArgValue::Entity { local_id: o.clone() } },
                ],
                qualifiers: Qualifiers::default(),
                evidence: vec![region.clone()],
                confidence: VISUAL_CONFIDENCE,
                verified: false,
                provenance_idx,
            });
        }
    }
    frag
}

/// Path 2: span evidence on figure/chart blocks becomes region evidence (confidence ≤ 0.7).
pub fn regionalise(out: &mut ExtractionOutput, doc: &StitchedDocument) {
    let figures: HashMap<&HashKey, (&HashKey, vs_core::BBox)> = doc
        .blocks
        .iter()
        .filter(|b| matches!(b.block.kind, BlockKind::Figure | BlockKind::Chart))
        .map(|b| (&b.block.block_hk, (&b.page_hk, b.block.bbox)))
        .collect();
    if figures.is_empty() {
        return;
    }
    for a in &mut out.assertions {
        let mut visual = false;
        for ev in &mut a.evidence {
            if let Evidence::Span { block_hk, .. } = ev {
                if let Some((page_hk, bbox)) = figures.get(block_hk) {
                    *ev = Evidence::Region { page_hk: (*page_hk).clone(), bbox: *bbox };
                    visual = true;
                }
            }
        }
        if visual {
            a.evidence.dedup();
            a.confidence = a.confidence.min(VISUAL_CONFIDENCE);
        }
    }
    for m in &mut out.mentions {
        if figures.contains_key(&m.block_hk) {
            m.confidence = m.confidence.min(VISUAL_CONFIDENCE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::DocBuilder;

    #[test]
    fn structured_description_yields_region_assertions() {
        let mut b = DocBuilder::new();
        let json = r#"{"description":"A pipeline.","entities":[{"label":"Encoder","type":"component"},{"label":"Decoder","type":"component"}],"relations":[{"subject":"Encoder","predicate":"feeds","object":"Decoder"},{"subject":"Encoder","predicate":"x","object":"Nowhere"}]}"#;
        let f = b.block_with(BlockKind::Figure, "A pipeline. Encoder Decoder", BlockContent::Figure { crop_uri: "s3://x".into(), description: Some(json.into()) });
        let frag = extract_figures(&b.doc, 2);
        assert_eq!(frag.entities.len(), 2);
        assert_eq!(frag.assertions.len(), 1);
        let a = &frag.assertions[0];
        assert_eq!(a.predicate, "feeds");
        assert_eq!(a.confidence, VISUAL_CONFIDENCE);
        assert_eq!(a.evidence, vec![Evidence::Region { page_hk: b.doc.blocks[f].page_hk.clone(), bbox: b.doc.blocks[f].block.bbox }]);
        assert_eq!((frag.mentions[1].char_start, frag.mentions[1].char_end), (20, 27));
    }

    #[test]
    fn plain_description_is_ignored_and_spans_become_regions() {
        let mut b = DocBuilder::new();
        let f = b.block_with(BlockKind::Figure, "Shows A beating B.", BlockContent::Figure { crop_uri: "s3://x".into(), description: Some("Shows A beating B.".into()) });
        assert!(extract_figures(&b.doc, 0).is_empty());
        let mut out = ExtractionOutput::default();
        out.assertions.push(Assertion {
            local_id: "a".into(),
            predicate: "beats".into(),
            predicate_id: None,
            kind: AssertionKind::Binary,
            args: vec![],
            qualifiers: Qualifiers::default(),
            evidence: vec![Evidence::Span { block_hk: b.hk(f), char_start: 0, char_end: 5 }, Evidence::Span { block_hk: b.hk(f), char_start: 6, char_end: 9 }],
            confidence: 0.95,
            verified: false,
            provenance_idx: 0,
        });
        regionalise(&mut out, &b.doc);
        assert_eq!(out.assertions[0].evidence.len(), 1);
        assert!(matches!(out.assertions[0].evidence[0], Evidence::Region { .. }));
        assert_eq!(out.assertions[0].confidence, 0.7);
    }
}
