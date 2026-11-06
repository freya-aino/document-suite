//! `config/dag-list.example.json` must stay loadable by the same path production uses:
//! `vs_router::DagList` (JSON) → `Router::load` against `production_registry`, with the
//! model ids of `config/models.example.toml`.

use std::path::Path;
use std::sync::Arc;

use vs_dag::{ArtifactKind, StepSignatures};

fn config_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config")
}

#[test]
fn example_dag_list_validates_against_production_registry() {
    let registry = vs_models::ModelRegistry::load(&config_dir().join("models.example.toml")).expect("models.example.toml loads");
    let client = Arc::new(vs_models::OpenAiClient::new(registry));
    let steps = vs_steps::production_registry(client.clone());

    let text = std::fs::read_to_string(config_dir().join("dag-list.example.json")).expect("dag-list.example.json readable");
    let list: vs_router::DagList = serde_json::from_str(&text).expect("dag-list.example.json is a vs_router::DagList");
    vs_router::Router::load(list.clone(), &steps).expect("example DAG list is valid for the production registry");

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
    let registry = vs_models::ModelRegistry::load(&config_dir().join("models.example.toml")).unwrap();
    let steps = vs_steps::production_registry(Arc::new(vs_models::OpenAiClient::new(registry)));
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
}
