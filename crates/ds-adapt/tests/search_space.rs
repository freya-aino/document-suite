//! `config/search-space.example.json` must generate candidates the production registry
//! accepts. A template naming a step that does not exist would otherwise only fail on a
//! stage cluster, after the GPU spend.

use std::path::Path;
use std::sync::Arc;

fn config_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config")
}

#[test]
fn example_search_space_generates_valid_candidates() {
    let registry = ds_models::ModelRegistry::load(&config_dir().join("models.example.toml")).unwrap();
    let client = Arc::new(ds_models::OpenAiClient::new(registry));
    let steps = ds_steps::production_registry(client.clone());

    let text = std::fs::read_to_string(config_dir().join("search-space.example.json")).expect("search-space.example.json readable");
    let space: ds_adapt::SearchSpace = serde_json::from_str(&text).expect("parses as a SearchSpace");
    let generated = ds_adapt::generate::generate(&space, &steps).expect("every template validates");
    assert!(!generated.candidates.is_empty(), "the example search space must produce candidates");
    // A rejected candidate means a template is wrong, not that generation is working.
    assert!(
        generated.rejected.is_empty(),
        "every template must validate against the production registry, rejected: {:?}",
        generated.rejected
    );

    // The conditioning templates are the point of the exercise: scoring can only say
    // whether deskewing or binarising helps if candidates exist both with and without.
    let ids: Vec<&str> = generated.candidates.iter().map(|d| d.id.as_str()).collect();
    for expected in ["deskew-layout-region-ocr", "conditioned-e2e", "binarized-region-ocr"] {
        assert!(ids.iter().any(|id| id.starts_with(expected)), "no candidate from template {expected}: {ids:?}");
    }
    assert!(
        ids.iter().any(|id| id.starts_with("layout-region-ocr")),
        "an unconditioned baseline is required to compare against: {ids:?}"
    );

    for dag in &generated.candidates {
        for node in &dag.nodes {
            if let Some(model) = node.params.get("model").and_then(|v| v.as_str()) {
                assert!(client.spec(model).is_ok(), "candidate {} node {}: unknown model {model:?}", dag.id, node.id);
            }
        }
    }
}
