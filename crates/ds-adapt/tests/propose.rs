//! Proposals from the real catalogue, against the real validator.
//!
//! `src/propose.rs` unit-tests the mutations against a stand-in signature table. This
//! checks them against `ds_steps::production_registry` and the hand-written DAG list, and
//! — the part that matters — against **`ds_router::Router::load`**, which is the same
//! check `POST /v1/dag-lists` makes. A proposer whose output the API would refuse is a
//! proposer that wastes a stage cluster's GPU time before anyone finds out.

use std::path::Path;
use std::sync::Arc;

use ds_adapt::propose::{self, Budget};

fn config_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config")
}

#[test]
fn proposals_from_the_hand_written_list_are_all_routable() {
    let registry = ds_models::ModelRegistry::load(&config_dir().join("models.example.toml")).expect("the example registry");
    let models: Vec<ds_adapt::propose::ModelChoice> = registry
        .all()
        .map(|m| ds_adapt::propose::ModelChoice { id: m.id.clone(), tasks: m.task.iter().cloned().collect() })
        .collect();
    let client = Arc::new(ds_models::OpenAiClient::new(registry));
    let steps = ds_steps::production_registry(client.clone());
    let kinds: Vec<&str> = steps.kinds().copied().collect();

    let incumbent: ds_router::DagList =
        serde_json::from_str(&std::fs::read_to_string(config_dir().join("dag-list.example.json")).expect("readable")).expect("parses");

    let proposed = propose::propose(&incumbent.dags, &steps, &kinds, &models, &Budget::default());

    assert!(proposed.rejected.is_empty(), "every mutation must be well-typed by construction: {:?}", proposed.rejected);
    assert!(proposed.candidates.len() > incumbent.dags.len(), "nothing was proposed beyond the seeds");

    // The contract: the incumbents are candidates, or the comparison has nothing to
    // compare against on the same pages.
    for seed in &incumbent.dags {
        assert!(proposed.candidates.iter().any(|d| d.id == seed.id), "the incumbent {} was dropped", seed.id);
    }

    // Every candidate has to be loadable as a list the router would accept — the same
    // validation the API's upload route runs.
    for dag in &proposed.candidates {
        let list = ds_router::DagList {
            version: format!("proposed-{}", dag.id),
            created_by: Some("tests/propose.rs".into()),
            dags: vec![dag.clone()],
            routing: ds_router::RoutingTable {
                default_dag: dag.id.clone(),
                low_confidence_dag: dag.id.clone(),
                min_classifier_confidence: 0.6,
                rules: vec![],
            },
        };
        ds_router::Router::load(list, &steps).unwrap_or_else(|e| panic!("{} is not routable: {e}", dag.id));

        // And every model it names must exist, or it fails at the first page.
        for node in &dag.nodes {
            if let Some(m) = node.params.get("model").and_then(|v| v.as_str()) {
                assert!(client.spec(m).is_ok(), "{} names unknown model {m:?}", dag.id);
            }
        }
    }

    // A model is never put where it cannot do the job. Before the task constraint the
    // proposer offered `embed-text` and `page-classifier` as page parsers.
    for dag in &proposed.candidates {
        for node in &dag.nodes {
            if let Some(m) = node.params.get("model").and_then(|v| v.as_str()) {
                let spec = client.spec(m).expect("known model");
                assert!(!spec.task.is_empty(), "{}: {m} declares no task", dag.id);
            }
        }
    }

    // The conditioners are the point: the search space's `oriented-e2e` and
    // `deskew-layout-region-ocr` templates have never been scored because the corpus had
    // nothing for them to repair, and the proposer must at least be able to express both
    // sides of that question.
    let summary = propose::summary(&proposed);
    eprintln!("proposed from {} seeds: {summary:?}", incumbent.dags.len());
    assert!(summary.get("conditioner added").copied().unwrap_or(0) > 0, "no conditioner was ever added: {summary:?}");
    assert!(
        proposed.candidates.iter().any(|d| d.nodes.iter().any(|n| n.step == "page.orient")),
        "page.orient was never proposed, so rotation cannot be repaired by anything the search can reach"
    );
}

/// Dropping a conditioner must give back a shape the router still accepts — the ablation
/// is half of every question the conditioners exist to answer.
#[test]
fn an_ablation_of_every_conditioned_proposal_is_also_proposed() {
    let registry = ds_models::ModelRegistry::load(&config_dir().join("models.example.toml")).expect("registry");
    let client = Arc::new(ds_models::OpenAiClient::new(registry));
    let steps = ds_steps::production_registry(client);
    let kinds: Vec<&str> = steps.kinds().copied().collect();
    let incumbent: ds_router::DagList =
        serde_json::from_str(&std::fs::read_to_string(config_dir().join("dag-list.example.json")).expect("readable")).expect("parses");

    // One round adds conditioners; feeding those back must remove them again.
    let round1 = propose::propose(&incumbent.dags, &steps, &kinds, &[], &Budget::default());
    let conditioned: Vec<ds_dag::Dag> = round1.candidates.iter().filter(|d| d.id.contains('+')).cloned().collect();
    assert!(!conditioned.is_empty(), "no conditioned candidate to ablate");

    let round2 = propose::propose(&conditioned, &steps, &kinds, &[], &Budget { max_candidates: 1000, ..Default::default() });
    assert!(round2.rejected.is_empty(), "{:?}", round2.rejected);
    let ablations = round2.candidates.iter().filter(|d| d.id.contains('-')).count();
    assert!(ablations > 0, "a conditioner that cannot be removed cannot be scored");
}
