//! `config/dag-list.example.json` must stay loadable by the same path production uses:
//! `ds_router::DagList` (JSON) → `Router::load` against `production_registry`, with the
//! model ids of `config/models.example.toml`.

use std::path::Path;
use std::sync::Arc;

use ds_dag::{ArtifactKind, StepSignatures};

fn config_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config")
}

#[test]
fn example_dag_list_validates_against_production_registry() {
    let registry = ds_models::ModelRegistry::load(&config_dir().join("models.example.toml")).expect("models.example.toml loads");
    let client = Arc::new(ds_models::OpenAiClient::new(registry));
    let steps = ds_steps::production_registry(client.clone());

    let text = std::fs::read_to_string(config_dir().join("dag-list.example.json")).expect("dag-list.example.json readable");
    let list: ds_router::DagList = serde_json::from_str(&text).expect("dag-list.example.json is a ds_router::DagList");
    ds_router::Router::load(list.clone(), &steps).expect("example DAG list is valid for the production registry");

    // Every `model` param names a model in the example registry.
    for dag in &list.dags {
        for node in &dag.nodes {
            if let Some(model) = node.params.get("model").and_then(|v| v.as_str()) {
                assert!(client.spec(model).is_ok(), "DAG {} node {}: model {model:?} is not in models.example.toml", dag.id, node.id);
            }
        }
    }
}

#[test]
fn production_registry_step_signatures() {
    use ArtifactKind::*;
    let registry = ds_models::ModelRegistry::load(&config_dir().join("models.example.toml")).unwrap();
    let steps = ds_steps::production_registry(Arc::new(ds_models::OpenAiClient::new(registry)));
    let expect = |kind: &str, inputs: &[ArtifactKind], output: ArtifactKind| {
        let sig = steps.signature(kind).unwrap_or_else(|| panic!("step {kind:?} missing from the production registry"));
        assert_eq!((sig.inputs.as_slice(), sig.output), (inputs, output), "signature of {kind}");
    };
    expect("page.parse_e2e", &[PageImage], Blocks);
    expect("layout.detect", &[PageImage], Layout);
    expect("region.ocr", &[PageImage, Layout], Blocks);
    expect("table.recognize", &[PageImage, Layout], Blocks);
    expect("formula.recognize", &[PageImage, Layout], Blocks);
    expect("figure.crop", &[PageImage, Layout], FigureCrops);
    expect("figure.describe", &[FigureCrops], Blocks);
    expect("chart.to_table", &[FigureCrops], Blocks);
    expect("blocks.merge", &[Blocks, Blocks], Blocks);
    expect("blocks.merge_native", &[TextLayer, Layout, Blocks], Blocks);
    expect("blocks.verify", &[Blocks, Blocks], Blocks);
    // Pre-processing is page_image -> page_image, which is what lets it compose and lets
    // a DAG apply none, one or several operations.
    for kind in ["page.orient", "page.deskew", "page.denoise", "page.normalize", "page.binarize", "page.crop_margins"] {
        expect(kind, &[PageImage], PageImage);
    }

    // Exhaustive, not just present: `blocks.verify` sat in the documented catalogue
    // unimplemented across four revisions because this test only ever asked whether the
    // steps it listed existed, never whether the registry held anything else — or, as it
    // turned out, whether the list had kept up (`page.orient` was missing from it too).
    // Adding a step now means editing this list, which means editing the catalogue in
    // `ds-dag/README.md` that it mirrors.
    let mut kinds: Vec<&str> = steps.kinds().copied().collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        vec![
            "blocks.merge",
            "blocks.merge_native",
            "blocks.verify",
            "chart.to_table",
            "figure.crop",
            "figure.describe",
            "formula.recognize",
            "layout.detect",
            "page.binarize",
            "page.crop_margins",
            "page.denoise",
            "page.deskew",
            "page.normalize",
            "page.orient",
            "page.parse_e2e",
            "region.ocr",
            "table.recognize",
        ],
        "the production registry and the step catalogue have diverged"
    );
}

