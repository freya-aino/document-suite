//! The executor, and the series it records while running.
//!
//! `vs-dag`'s own tests covered validation only, so the thing that actually runs a page —
//! level-parallel execution, per-node timeouts, the optional-node fallback — was reached
//! only through the step crates' tests of individual steps. These exercise it directly,
//! with a fake registry, and in the same pass assert that the metrics the catalogue
//! documents are really emitted and that their label sets stay bounded.

use std::sync::Arc;

use async_trait::async_trait;
use metrics_exporter_prometheus::PrometheusBuilder;
use vs_core::{BBox, BlockContent, BlockKind, ClassifierTier, DocType, HashKey, LanguageTag, PageProfile, RunId};
use vs_dag::{
    Artifact, ArtifactKind, BlockDraft, Dag, ExecError, Executor, Node, NodeOutcome, PageImage, Signature, Step, StepContext, StepError, StepOutput,
    StepRegistry, TextLayer,
};

/// A step that does whatever the test told it to, so one registry covers every case.
enum Behaviour {
    Blocks(&'static str),
    Fail(StepError),
    Hang,
}

struct Fake {
    kind: &'static str,
    inputs: Vec<ArtifactKind>,
    behaviour: Behaviour,
}

#[async_trait]
impl Step for Fake {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn signature(&self) -> Signature {
        Signature { inputs: self.inputs.clone(), output: ArtifactKind::Blocks }
    }
    fn default_timeout_ms(&self) -> u64 {
        50
    }
    async fn run(&self, ctx: &StepContext, _inputs: &[&Artifact], _params: &serde_json::Value) -> Result<StepOutput, StepError> {
        match &self.behaviour {
            Behaviour::Blocks(text) => Ok(StepOutput {
                artifact: Artifact::Blocks(vec![BlockDraft {
                    kind: BlockKind::Paragraph,
                    bbox: BBox::new(0.0, 0.0, 1.0, 0.5).unwrap(),
                    reading_order: 0,
                    content: BlockContent::Text { text: (*text).into() },
                    text: (*text).into(),
                    level: None,
                    confidence: 0.9,
                    node_id: ctx.node_id.clone(),
                }]),
                model_ref: Some("fake@1".into()),
                prompt_ref: None,
                // One named fact about how this ran, so the executor's handling of
                // per-node metadata is exercised rather than asserted.
                metrics: vs_dag::Metrics::from([("fake_path".to_string(), serde_json::json!("blocks"))]),
            }),
            Behaviour::Fail(e) => Err(clone_err(e)),
            Behaviour::Hang => {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                unreachable!("the executor's timeout fires first")
            }
        }
    }
}

/// `StepError` is not `Clone`, and the fake has to hand out the same error repeatedly.
fn clone_err(e: &StepError) -> StepError {
    match e {
        StepError::InvalidParams(s) => StepError::InvalidParams(s.clone()),
        StepError::Model(s) => StepError::Model(s.clone()),
        StepError::Parse(s) => StepError::Parse(s.clone()),
        StepError::Io(s) => StepError::Io(s.clone()),
        StepError::NotImplemented(s) => StepError::NotImplemented(s),
    }
}

fn registry() -> StepRegistry {
    let mut reg = StepRegistry::new();
    for (kind, inputs, behaviour) in [
        ("fake.parse", vec![ArtifactKind::PageImage], Behaviour::Blocks("parsed")),
        ("fake.second", vec![ArtifactKind::PageImage], Behaviour::Blocks("second")),
        ("fake.join", vec![ArtifactKind::Blocks, ArtifactKind::Blocks], Behaviour::Blocks("joined")),
        // The message is deliberately the sort of thing a real parser produces: long, and
        // carrying the offending content.
        (
            "fake.badparse",
            vec![ArtifactKind::PageImage],
            Behaviour::Fail(StepError::Parse("expected `blocks` at line 3 column 11, got \"<!DOCTYPE html>\"".into())),
        ),
        ("fake.hang", vec![ArtifactKind::PageImage], Behaviour::Hang),
    ] {
        reg.register(Arc::new(Fake { kind, inputs, behaviour }));
    }
    reg
}

fn node(id: &str, step: &str, inputs: &[&str], optional: bool) -> Node {
    Node {
        id: id.into(),
        step: step.into(),
        inputs: inputs.iter().map(|s| s.to_string()).collect(),
        params: serde_json::json!({"n": 1}),
        timeout_ms: None,
        optional,
    }
}

fn dag(output: &str, nodes: Vec<Node>) -> Dag {
    Dag { id: "t".into(), version: "1".into(), description: None, output: output.into(), nodes }
}

fn ctx() -> StepContext {
    StepContext {
        run_id: RunId::new(),
        document_hk: HashKey::of(&["d"]),
        page_hk: HashKey::of(&["p"]),
        page_no: 1,
        profile: PageProfile {
            language: LanguageTag::parse("en").unwrap(),
            doc_type: DocType::Other,
            born_digital: true,
            text_layer_coverage: 1.0,
            has_handwriting: false,
            has_images: false,
            has_tables: false,
            has_formulas: false,
            has_charts: false,
            rotation_deg: 0,
            confidence: 1.0,
            tier: ClassifierTier::Rules,
        },
        dag_ref: String::new(),
        node_id: String::new(),
        interactive: false,
        pages: None,
    }
}

fn page() -> (Artifact, Artifact) {
    (
        Artifact::PageImage(PageImage { uri: "mem".into(), bytes: Arc::from(Vec::new().into_boxed_slice()), width_px: 10, height_px: 10, dpi: 72 }),
        Artifact::TextLayer(TextLayer::default()),
    )
}

#[tokio::test]
async fn a_level_runs_in_parallel_and_provenance_follows_every_node() {
    let reg = registry();
    let d = dag(
        "join",
        vec![
            node("a", "fake.parse", &["$page_image"], false),
            node("b", "fake.second", &["$page_image"], false),
            node("join", "fake.join", &["a", "b"], false),
        ],
    );
    let (img, text) = page();
    let out = Executor::new(&reg).run(&d, &ctx(), img, text).await.expect("run");

    assert_eq!(out.blocks.len(), 1);
    assert_eq!(out.blocks[0].text, "joined");
    assert_eq!(out.traces.len(), 3, "one trace per node");
    assert!(out.traces.iter().all(|t| t.outcome == NodeOutcome::Ok));
    let a = out.traces.iter().find(|t| t.node_id == "a").expect("node a");
    assert_eq!(a.provenance.dag_ref, "t@1");
    assert_eq!(a.provenance.step_kind, "fake.parse");
    assert_eq!(a.provenance.model_ref.as_deref(), Some("fake@1"));
    // The params hash is what makes two runs of the same DAG comparable, so it has to be
    // over the params and not over the node.
    let b = out.traces.iter().find(|t| t.node_id == "b").expect("node b");
    assert_eq!(a.provenance.params_hash, b.provenance.params_hash);
}

#[tokio::test]
async fn a_node_not_on_the_path_to_the_output_never_runs() {
    let reg = registry();
    let d = dag(
        "a",
        vec![
            node("a", "fake.parse", &["$page_image"], false),
            // Would fail the page if it ran. Pruning is what keeps a DAG list's unused
            // branches from costing anything.
            node("dead", "fake.badparse", &["$page_image"], false),
        ],
    );
    let (img, text) = page();
    let out = Executor::new(&reg).run(&d, &ctx(), img, text).await.expect("the pruned node cannot fail the page");
    assert_eq!(out.traces.len(), 1);
    assert_eq!(out.traces[0].node_id, "a");
}

#[tokio::test]
async fn an_optional_node_fails_to_an_empty_artefact_and_a_required_one_does_not() {
    let reg = registry();
    let (img, text) = page();
    let optional = dag(
        "join",
        vec![
            node("a", "fake.parse", &["$page_image"], false),
            node("bad", "fake.badparse", &["$page_image"], true),
            node("join", "fake.join", &["a", "bad"], false),
        ],
    );
    let out = Executor::new(&reg).run(&optional, &ctx(), img.clone(), text.clone()).await.expect("optional failure is survivable");
    assert_eq!(out.blocks[0].text, "joined", "the dependant ran against an empty block set");
    let bad = out.traces.iter().find(|t| t.node_id == "bad").expect("trace for the failed node");
    assert!(matches!(&bad.outcome, NodeOutcome::SkippedAfterError(e) if e.contains("DOCTYPE")), "{:?}", bad.outcome);

    let required = dag("bad", vec![node("bad", "fake.badparse", &["$page_image"], false)]);
    let err = Executor::new(&reg).run(&required, &ctx(), img.clone(), text.clone()).await.expect_err("required failure stops the page");
    assert!(matches!(err, ExecError::Node { .. }), "{err:?}");

    // A node that never returns is the executor's problem, not the step's.
    let hung = dag("h", vec![node("h", "fake.hang", &["$page_image"], false)]);
    let err = Executor::new(&reg).run(&hung, &ctx(), img, text).await.expect_err("hang");
    assert!(matches!(err, ExecError::Timeout { timeout_ms: 50, .. }), "{err:?}");
}

#[tokio::test]
async fn the_recorded_series_name_the_step_and_keep_the_reason_bounded() {
    // `#[tokio::test]` runs on one thread, which is what makes a thread-local recorder a
    // sound way to capture what this test alone records.
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let reg = registry();
    let (img, text) = page();
    let d = dag(
        "join",
        vec![
            node("a", "fake.parse", &["$page_image"], false),
            node("bad", "fake.badparse", &["$page_image"], true),
            node("join", "fake.join", &["a", "bad"], false),
        ],
    );
    Executor::new(&reg).run(&d, &ctx(), img.clone(), text.clone()).await.expect("run");
    let hung = dag("h", vec![node("h", "fake.hang", &["$page_image"], false)]);
    let _ = Executor::new(&reg).run(&hung, &ctx(), img, text).await;

    let body = handle.render();
    assert!(body.contains(r#"dag_pages_total{dag="t@1"} 2"#), "{body}");
    assert!(body.contains(r#"dag_node_duration_seconds_count{step="fake.parse",dag="t@1"} 1"#), "{body}");
    // The timed-out node is timed too: a step slow enough to be killed is the one whose
    // duration anyone wants a quantile for.
    assert!(body.contains(r#"dag_node_duration_seconds_count{step="fake.hang",dag="t@1"} 1"#), "{body}");
    assert!(body.contains(r#"dag_node_failures_total{step="fake.badparse",dag="t@1",reason="parse"} 1"#), "{body}");
    assert!(body.contains(r#"dag_node_failures_total{step="fake.hang",dag="t@1",reason="timeout"} 1"#), "{body}");
    // The whole point of mapping the error onto a fixed word: the message that reached
    // the `NodeTrace` must not reach a label, or every malformed reply is its own series.
    assert!(!body.contains("DOCTYPE"), "the error message leaked into a label:\n{body}");
}
