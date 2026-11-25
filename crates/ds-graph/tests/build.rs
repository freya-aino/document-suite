//! `build()` and `validate()` end to end.
//!
//! This crate is the single structure both stores are written from, and until now it had
//! no test of the builder at all — so every invariant SPEC.md claimed was claimed on
//! trust. These assert the structure a document actually produces, including the
//! intra-page relations added in this phase.

use std::collections::HashSet;

use chrono::Utc;
use serde_json::json;

use ds_core::{BBox, Block, BlockContent, BlockKind, Evidence, HashKey, Provenance, RunId};
use ds_extract::{Arg, ArgValue, Assertion, AssertionKind, DocEntity, ExtractionOutput, Mention, Qualifiers};
use ds_graph::{build, DocGraph, EdgeKind, GraphError, NodeKind};
use ds_stitch::{DocBlock, Link, LinkKind, Section, StitchedDocument};

fn prov(run: RunId) -> Provenance {
    Provenance {
        run_id: run,
        dag_ref: "d@1".into(),
        node_id: "n".into(),
        step_kind: "page.parse_e2e".into(),
        model_ref: None,
        prompt_ref: None,
        params_hash: "0".repeat(64),
        started_at: Utc::now(),
        duration_ms: 1,
    }
}

fn bb(y0: f32, y1: f32) -> BBox {
    BBox::new(0.1, y0, 0.9, y1).unwrap()
}

struct Fixture {
    doc: StitchedDocument,
    ext: ExtractionOutput,
    keys: Vec<HashKey>,
}

/// One page: a heading, a paragraph, a results table and a chart, plus two entities and
/// one verified assertion between them.
fn fixture() -> Fixture {
    let run = RunId::new();
    let document_hk = HashKey::of(&["doc"]);
    let page_hk = HashKey::of(&["page", "1"]);
    let keys: Vec<HashKey> = (0..4).map(|i| HashKey::of(&["b", &i.to_string()])).collect();

    let mk = |i: usize, kind: BlockKind, text: &str, content: BlockContent| DocBlock {
        block: Block {
            block_hk: keys[i].clone(),
            index: i as u32,
            kind,
            bbox: bb(0.1 * i as f32, 0.1 * i as f32 + 0.08),
            reading_order: i as u32,
            content,
            text: text.into(),
            level: (kind == BlockKind::Heading).then_some(1),
            confidence: 0.9,
            provenance: prov(run),
        },
        page_no: 1,
        page_hk: page_hk.clone(),
        global_order: i as u32,
        // Single-column: `repair_reading_order` records a column only where it found
        // several, so a page like this one carries none.
        column: None,
        page_columns: 0,
    };

    let table_html = "<table><tr><th>Model</th><th>F1</th></tr><tr><td>BERT</td><td>88.5</td></tr></table>";
    let blocks = vec![
        mk(0, BlockKind::Heading, "1 Results", BlockContent::Text { text: "1 Results".into() }),
        mk(1, BlockKind::Paragraph, "BERT reaches 88.5 F1 on GLUE.", BlockContent::Text { text: "BERT reaches 88.5 F1 on GLUE.".into() }),
        mk(2, BlockKind::Table, "Model\tF1\nBERT\t88.5", BlockContent::Table { html: table_html.into() }),
        mk(
            3,
            BlockKind::Chart,
            "",
            BlockContent::Chart {
                crop_uri: "crop://c".into(),
                data_html: None,
                // Structured output, as `figure.describe` stores it.
                description: Some(json!({ "description": "F1 by model", "text_in_image": "BERT\nGLUE score" }).to_string()),
            },
        ),
    ];

    let doc = StitchedDocument {
        document_hk,
        run_id: run,
        blocks,
        furniture: HashSet::new(),
        sections: vec![Section { heading: keys[0].clone(), level: 1, title: "1 Results".into(), body: vec![1, 2, 3], parent: None }],
        links: vec![Link::new(LinkKind::ValueOf, keys[1].clone(), keys[0].clone(), 0.8)],
        missing_pages: vec![],
    };

    let ext = ExtractionOutput {
        mentions: vec![Mention {
            id: "m1".into(),
            block_hk: keys[1].clone(),
            char_start: 0,
            char_end: 4,
            surface: "BERT".into(),
            raw_type: "method".into(),
            confidence: 0.9,
        }],
        entities: vec![
            DocEntity {
                local_id: "e1".into(),
                name: "BERT".into(),
                raw_type: "method".into(),
                type_id: Some("method".into()),
                aliases: vec![],
                mentions: vec!["m1".into()],
                entity_hk: Some(HashKey::of(&["t", "method|bert"])),
                context: String::new(),
            },
            DocEntity {
                local_id: "e2".into(),
                name: "GLUE".into(),
                raw_type: "dataset".into(),
                type_id: Some("dataset".into()),
                aliases: vec![],
                mentions: vec![],
                entity_hk: Some(HashKey::of(&["t", "dataset|glue"])),
                context: String::new(),
            },
        ],
        assertions: vec![Assertion {
            local_id: "a1".into(),
            predicate: "evaluated on".into(),
            predicate_id: Some("evaluated_on".into()),
            kind: AssertionKind::Binary,
            args: vec![
                Arg { role: "subject".into(), value: ArgValue::Entity { local_id: "e1".into() } },
                Arg { role: "object".into(), value: ArgValue::Entity { local_id: "e2".into() } },
            ],
            qualifiers: Qualifiers::default(),
            evidence: vec![Evidence::Span { block_hk: keys[1].clone(), char_start: 0, char_end: 28 }],
            confidence: 0.85,
            verified: true,
            provenance_idx: 0,
        }],
        provenance: vec![prov(run)],
        dropped_ungrounded: 0,
        dropped_unverified: 0,
    };
    Fixture { doc, ext, keys }
}

fn edges_of(g: &DocGraph, kind: EdgeKind) -> Vec<(&str, &str)> {
    g.edges.iter().filter(|e| e.kind == kind).map(|e| (e.from.as_str(), e.to.as_str())).collect()
}

#[test]
fn a_whole_document_validates() {
    let f = fixture();
    let g = build(&f.doc, &f.ext, Some("A paper"));
    g.validate().expect("a well-formed document must validate");
}

#[test]
fn structure_layer_is_complete() {
    let f = fixture();
    let g = build(&f.doc, &f.ext, Some("A paper"));
    assert_eq!(g.nodes.iter().filter(|n| n.kind == NodeKind::Document).count(), 1);
    assert_eq!(g.nodes.iter().filter(|n| n.kind == NodeKind::Page).count(), 1);
    assert_eq!(g.nodes.iter().filter(|n| n.kind == NodeKind::Block).count(), 4);
    assert_eq!(g.nodes.iter().filter(|n| n.kind == NodeKind::Section).count(), 1);
    // Every block is on its page, and body blocks form one reading chain.
    assert_eq!(edges_of(&g, EdgeKind::OnPage).len(), 4);
    assert_eq!(edges_of(&g, EdgeKind::Next).len(), 3, "four body blocks make three Next edges");
    assert_eq!(edges_of(&g, EdgeKind::Contains).len(), 3);
}

/// The relations this phase added, which a page could not previously express.
#[test]
fn intra_page_relations_reach_the_graph() {
    let f = fixture();
    let g = build(&f.doc, &f.ext, Some("A paper"));

    // A stitch `ValueOf` link becomes a graph edge rather than being dropped.
    assert_eq!(edges_of(&g, EdgeKind::ValueOf), vec![(f.keys[1].as_str(), f.keys[0].as_str())]);

    // The results table is addressable cell by cell.
    let cells: Vec<&str> = g.nodes.iter().filter(|n| n.kind == NodeKind::Cell).map(|n| n.key.as_str()).collect();
    assert_eq!(cells.len(), 4, "2x2 table: {cells:?}");
    let f1 = g.nodes.iter().find(|n| n.props["text"] == "88.5").expect("the F1 value is its own node");
    assert_eq!(f1.props["column_header"], "F1", "so the number can be cited with its meaning");
    assert_eq!(edges_of(&g, EdgeKind::HasCell).len(), 4);

    // Chart labels are nodes, and the one matching body text is linked to it.
    let img: Vec<&str> = g.nodes.iter().filter(|n| n.kind == NodeKind::ImageText).map(|n| n.key.as_str()).collect();
    assert_eq!(img.len(), 2, "two fragments: {img:?}");
    let matched: Vec<&str> = edges_of(&g, EdgeKind::MatchesText).into_iter().map(|(_, to)| to).collect();
    assert!(matched.contains(&f.keys[1].as_str()), "'BERT' in the chart matches the paragraph: {matched:?}");
    assert!(
        matched.iter().any(|t| *t == f.ext.entities[0].entity_hk.as_ref().unwrap().as_str()),
        "and the BERT entity: {matched:?}"
    );
}

/// A chart whose `text` is empty still has to be embeddable; SPEC.md promised it and the
/// builder used to skip it.
#[test]
fn a_figure_with_no_text_still_gets_an_embed_target() {
    let f = fixture();
    let g = build(&f.doc, &f.ext, None);
    let chart = f.keys[3].as_str();
    assert_eq!(f.doc.blocks[3].block.text, "", "the fixture's chart has no text projection");
    assert!(
        g.embed_targets.iter().any(|t| t.node_key == chart && !t.text.trim().is_empty()),
        "targets: {:?}",
        g.embed_targets.iter().map(|t| &t.node_key).collect::<Vec<_>>()
    );
}

#[test]
fn citations_get_a_direct_edge() {
    let mut f = fixture();
    let reference = DocEntity {
        local_id: "r1".into(),
        name: "Devlin et al. 2019".into(),
        raw_type: "reference".into(),
        type_id: Some("reference".into()),
        aliases: vec![],
        mentions: vec![],
        entity_hk: Some(HashKey::of(&["t", "reference|devlin"])),
                context: String::new(),
    };
    let ref_hk = reference.entity_hk.clone().unwrap();
    f.ext.entities.push(reference);
    // Shaped exactly as ds-extract's bibliography pass emits it: n-ary, with the citing
    // block as a literal.
    f.ext.assertions.push(Assertion {
        local_id: "bib.c1".into(),
        predicate: "cites".into(),
        predicate_id: None,
        kind: AssertionKind::Nary,
        args: vec![
            Arg { role: "claim_block".into(), value: ArgValue::Literal { value: f.keys[1].to_string(), unit: None } },
            Arg { role: "reference".into(), value: ArgValue::Entity { local_id: "r1".into() } },
        ],
        qualifiers: Qualifiers::default(),
        evidence: vec![Evidence::Span { block_hk: f.keys[1].clone(), char_start: 0, char_end: 4 }],
        confidence: 0.9,
        verified: false,
        provenance_idx: 0,
    });

    let g = build(&f.doc, &f.ext, None);
    assert_eq!(
        edges_of(&g, EdgeKind::Cites),
        vec![(f.keys[1].as_str(), ref_hk.as_str())],
        "a citation should be one edge, not a scan of every assertion's arguments"
    );
    g.validate().expect("still valid");
}

#[test]
fn a_binary_assertion_gets_the_related_shortcut() {
    let f = fixture();
    let g = build(&f.doc, &f.ext, None);
    let related = edges_of(&g, EdgeKind::Related);
    assert_eq!(related.len(), 1);
    assert_eq!(related[0].0, f.ext.entities[0].entity_hk.as_ref().unwrap().as_str());
    assert_eq!(related[0].1, f.ext.entities[1].entity_hk.as_ref().unwrap().as_str());
}

// ─────────────── validate() ───────────────

#[test]
fn a_dangling_edge_is_rejected() {
    let f = fixture();
    let mut g = build(&f.doc, &f.ext, None);
    g.edges.push(ds_graph::GEdge {
        kind: EdgeKind::OnPage,
        from: "nope".into(),
        to: "also-nope".into(),
        props: serde_json::Value::Null,
    });
    assert!(matches!(g.validate(), Err(GraphError::DanglingEdge { .. })));
}

/// The gap that mattered most: keys are unique per *kind*, so an existence check alone
/// let an edge be satisfied by a node of an entirely different kind.
#[test]
fn an_edge_between_the_wrong_kinds_is_rejected() {
    let f = fixture();
    let mut g = build(&f.doc, &f.ext, None);
    let entity = f.ext.entities[0].entity_hk.clone().unwrap();
    g.edges.push(ds_graph::GEdge {
        kind: EdgeKind::OnPage,
        from: f.keys[1].to_string(),
        // Both keys exist, so the old check passed this.
        to: entity.to_string(),
        props: serde_json::Value::Null,
    });
    match g.validate() {
        Err(GraphError::WrongEndpointKind { kind, from, to }) => {
            assert_eq!((kind, from, to), (EdgeKind::OnPage, NodeKind::Block, NodeKind::Entity));
        }
        other => panic!("expected a wrong-endpoint error, got {other:?}"),
    }
}

#[test]
fn an_assertion_without_evidence_is_rejected() {
    let f = fixture();
    let mut g = build(&f.doc, &f.ext, None);
    g.edges.retain(|e| e.kind != EdgeKind::Evidence);
    assert!(matches!(g.validate(), Err(GraphError::AssertionWithoutEvidence(_))));
}

/// SPEC.md claimed this invariant from the start and nothing checked it, so a bare
/// predicate asserting nothing about anything passed validation.
#[test]
fn an_assertion_without_arguments_or_literals_is_rejected() {
    let f = fixture();
    let mut g = build(&f.doc, &f.ext, None);
    g.edges.retain(|e| e.kind != EdgeKind::Arg);
    match g.validate() {
        Err(GraphError::AssertionWithoutArguments(_)) => {}
        other => panic!("expected a missing-arguments error, got {other:?}"),
    }
}

#[test]
fn a_duplicate_node_of_the_same_kind_is_rejected() {
    let f = fixture();
    let mut g = build(&f.doc, &f.ext, None);
    let dup = g.nodes.iter().find(|n| n.kind == NodeKind::Block).unwrap().clone();
    g.nodes.push(dup);
    assert!(matches!(g.validate(), Err(GraphError::DuplicateNode(_))));
}

#[test]
fn an_empty_graph_validates() {
    assert!(DocGraph::default().validate().is_ok());
}
