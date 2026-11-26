//! The shipped search spaces must generate candidates the production registry accepts. A
//! template naming a step that does not exist would otherwise only fail on a stage
//! cluster, after the GPU spend.

use std::path::Path;
use std::sync::Arc;

fn config_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config")
}

/// Both shipped spaces, generated and validated the same way.
fn generate(space_file: &str) -> (Arc<ds_models::OpenAiClient>, ds_adapt::generate::Generated) {
    let registry = ds_models::ModelRegistry::load(&config_dir().join("models.example.toml")).unwrap();
    let client = Arc::new(ds_models::OpenAiClient::new(registry));
    let steps = ds_steps::production_registry(client.clone());
    let text = std::fs::read_to_string(config_dir().join(space_file)).unwrap_or_else(|e| panic!("{space_file}: {e}"));
    let space: ds_adapt::SearchSpace = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{space_file}: {e}"));
    let generated = ds_adapt::generate::generate(&space, &steps).unwrap_or_else(|e| panic!("{space_file}: {e:?}"));
    assert!(!generated.candidates.is_empty(), "{space_file} must produce candidates");
    // A rejected candidate means a template is wrong, not that generation is working.
    assert!(generated.rejected.is_empty(), "{space_file}: rejected {:?}", generated.rejected);
    for dag in &generated.candidates {
        for node in &dag.nodes {
            if let Some(model) = node.params.get("model").and_then(|v| v.as_str()) {
                assert!(client.spec(model).is_ok(), "{space_file}: candidate {} node {}: unknown model {model:?}", dag.id, node.id);
            }
        }
    }
    (client, generated)
}

/// The wider space the integration suite runs, and the only place the three steps added
/// for it appear in a DAG at all.
///
/// A step in the registry that no template uses is a step nothing measures: it validates,
/// it never runs, and the search cannot tell you whether it helps. So this asserts the
/// *use*, not the registration.
#[test]
fn the_integration_search_space_exercises_every_new_step() {
    let (_client, generated) = generate("search-space.itest.json");
    let steps_used: std::collections::BTreeSet<&str> = generated.candidates.iter().flat_map(|d| d.nodes.iter().map(|n| n.step.as_str())).collect();
    for step in ["blocks.vote", "blocks.reorder", "layout.merge"] {
        assert!(steps_used.contains(step), "no candidate uses {step}: {steps_used:?}");
    }

    let ids: Vec<&str> = generated.candidates.iter().map(|d| d.id.as_str()).collect();
    // Each of these exists to be compared against something: an ablation partner that
    // differs by one node. Without the partner the measurement says nothing.
    for (template, partner) in [
        ("reordered-e2e", "e2e"),
        ("verify-two-model", "e2e-cross-checked"),
        ("native-first-vlm-fallback", "native-layout-tables"),
        ("orient-deskew-layout-region", "layout-region-ocr"),
    ] {
        assert!(ids.iter().any(|id| id.starts_with(template)), "no candidate from {template}: {ids:?}");
        assert!(ids.iter().any(|id| id.starts_with(partner)), "{template} has no ablation partner {partner}: {ids:?}");
    }
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

/// Every tier's model registry must name the **same model ids**.
///
/// That is the property the whole integration stack rests on: one search space, one DAG
/// list and one set of `params["model"]` values run on the fake tier, on a small CPU
/// model and on the GPU pools, with only the endpoints differing. If a tier renamed a
/// model, the search space would still validate against `config/models.example.toml` and
/// then fail at run time on that tier alone — after the stack was up, and looking like a
/// routing bug.
#[test]
fn every_tier_registry_serves_the_same_model_ids() {
    let reference = ds_models::ModelRegistry::load(&config_dir().join("models.example.toml")).unwrap();
    let want = ids_of(&reference);
    assert!(want.contains(&"embed-text".to_string()), "sanity: the reference registry is the one we think it is");

    let compose = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/compose/models");
    let gpu_host = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/gpu/models.compose.toml");
    let mut checked = 0;
    for path in [compose.join("fake.toml"), compose.join("cpu.toml"), compose.join("gpu.toml"), gpu_host] {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let registry = ds_models::ModelRegistry::load(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(ids_of(&registry), want, "{name} does not serve the same model ids as models.example.toml");

        // And every candidate the search space generates must resolve on that tier, which
        // is the same check the API's upload route makes against the active list.
        let client = std::sync::Arc::new(ds_models::OpenAiClient::new(registry));
        let steps = ds_steps::production_registry(client.clone());
        let text = std::fs::read_to_string(config_dir().join("search-space.example.json")).expect("readable");
        let space: ds_adapt::SearchSpace = serde_json::from_str(&text).expect("parses");
        let generated = ds_adapt::generate::generate(&space, &steps).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(generated.rejected.is_empty(), "{name}: rejected {:?}", generated.rejected);
        for dag in &generated.candidates {
            for node in &dag.nodes {
                if let Some(model) = node.params.get("model").and_then(|v| v.as_str()) {
                    assert!(client.spec(model).is_ok(), "{name}: candidate {} names unknown model {model:?}", dag.id);
                }
            }
        }
        checked += 1;
    }
    assert_eq!(checked, 4, "every tier registry must be checked");
}

fn ids_of(registry: &ds_models::ModelRegistry) -> Vec<String> {
    let mut ids: Vec<String> = registry.all().map(|m| m.id.clone()).collect();
    ids.sort();
    ids
}
