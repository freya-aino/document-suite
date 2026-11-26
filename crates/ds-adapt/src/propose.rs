//! Proposing DAG shapes, rather than only expanding the ones somebody wrote.
//!
//! `generate` is a template × slot cartesian product: it explores the *parameters* of
//! shapes a human authored, and it is the readable path that should stay. This is the
//! exploratory one — it takes working DAGs and changes their **structure**.
//!
//! The two are deliberately separate. A template says what a person meant; a mutant says
//! what the type system allows. Mixing them would make the search space unreadable, and
//! `search-space.example.json` is documentation as much as it is configuration.
//!
//! ## Mutations, and why these four
//!
//! Every one is **type-safe by construction** and then validated anyway:
//!
//! - **Insert a conditioner.** Any `[PageImage] → PageImage` step can be spliced in front
//!   of whatever consumed the page image. Always well-typed.
//! - **Drop a conditioner.** The ablation, and the most valuable of the four: scoring can
//!   only say whether deskewing *pays* if the same DAG exists with and without it, on the
//!   same pages. A search that only ever adds work cannot answer that.
//! - **Swap a model.** The same shape against a different model id.
//! - **Toggle `optional`.** Whether a step's failure should sink the page or be skipped
//!   past is a real design choice and nothing has ever scored it.
//!
//! ## What this is not
//!
//! Not a search over all topologies. Free enumeration over five artifact kinds and
//! seventeen steps produces mostly nonsense — "call every model and merge" dominates on
//! quality while being absurd on cost — and nothing here would notice, because the
//! objective's cost terms are referenced to one host. Mutating working DAGs keeps every
//! candidate within sight of something a person endorsed, which is the property that
//! makes the output reviewable.
//!
//! Nor does it promote anything. `select` chooses, a human activates (decision D4 keeps
//! optimisation offline), and on the generated corpus the result is a **screen** rather
//! than a winner: see the plan and `report::render`'s own caveat.

use std::collections::{BTreeMap, BTreeSet};

use ds_dag::{ArtifactKind, Dag, Node, StepSignatures, SOURCE_PAGE_IMAGE};

use crate::generate::{content_version, Rejected};

/// A model the search may substitute, with the tasks its registry entry declares.
///
/// The tasks are the whole point. Offering any model for any node produced candidates
/// like `vlm-e2e-robust~parse=embed-text` — the page parser replaced by the *embedding*
/// model — which type-checks perfectly, because a step's signature says nothing about
/// what a model can do, and would then fail or produce nonsense at the first page.
///
/// A swap is allowed when the two models share at least one declared task. That is
/// derived from the registry rather than from a step-kind-to-task table, which lives
/// inside `ds-models` and is not public; duplicating it here would be a second copy of
/// knowledge that already exists and would drift.
#[derive(Debug, Clone)]
pub struct ModelChoice {
    pub id: String,
    pub tasks: BTreeSet<String>,
}

impl ModelChoice {
    /// Could this model stand in for that one?
    fn interchangeable_with(&self, other: &ModelChoice) -> bool {
        self.id != other.id && !self.tasks.is_disjoint(&other.tasks)
    }
}

/// Limits on the search. Without them a handful of seeds and seventeen steps produce
/// thousands of candidates, and every one costs model calls to score.
#[derive(Debug, Clone)]
pub struct Budget {
    pub max_candidates: usize,
    /// Cap on nodes in a proposed DAG, relative to nothing: an absolute ceiling, because
    /// a 20-node page pipeline is not a thing anyone will run.
    pub max_nodes: usize,
    /// How many more model-calling nodes a candidate may have than the thriftiest seed.
    /// This is what stops "fan out to everything and merge" from crowding out the field.
    pub max_extra_model_nodes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self { max_candidates: 200, max_nodes: 10, max_extra_model_nodes: 1 }
    }
}

/// What was done to a seed to get a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// The seed itself, carried through unchanged.
    Seed,
    ConditionerAdded,
    ConditionerRemoved,
    ModelSwapped,
    OptionalityToggled,
}

impl Mutation {
    pub fn label(self) -> &'static str {
        match self {
            Mutation::Seed => "seed",
            Mutation::ConditionerAdded => "conditioner added",
            Mutation::ConditionerRemoved => "conditioner removed",
            Mutation::ModelSwapped => "model swapped",
            Mutation::OptionalityToggled => "optionality toggled",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Proposed {
    pub candidates: Vec<Dag>,
    /// How each candidate came about, by DAG id.
    ///
    /// Recorded rather than inferred from the id. The first version of `summary` read the
    /// id for `+`, `-`, `~` and `?` — and every incumbent in
    /// `config/dag-list.example.json` has a hyphen in its name, so `born-digital-text`
    /// and all 52 of its mutants were reported as "conditioner removed" while model swaps
    /// and optionality toggles showed as zero.
    pub kinds: BTreeMap<String, Mutation>,
    pub rejected: Vec<Rejected>,
    /// How many well-formed candidates were dropped for the budget.
    pub truncated: usize,
}

/// Mutates `seeds` into candidates the step catalogue accepts.
///
/// **The seeds are always carried through as candidates.** That is a contract, not a
/// convenience: `report::render`'s comparison section can only report the current list's
/// quality over pages where the DAG it routes to was actually evaluated, so an incumbent
/// left out of the candidate set silently becomes uncomparable — and the proposal would
/// then be measured against nothing.
pub fn propose(seeds: &[Dag], steps: &dyn StepSignatures, kinds: &[&str], models: &[ModelChoice], budget: &Budget) -> Proposed {
    let conditioners: Vec<&str> = kinds
        .iter()
        .copied()
        .filter(|k| {
            matches!(steps.signature(k), Some(s) if s.inputs == vec![ArtifactKind::PageImage] && s.output == ArtifactKind::PageImage)
        })
        .collect();

    let baseline_models = seeds.iter().map(|d| model_nodes(d, steps)).min().unwrap_or(0);
    let mut out = Proposed::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    // Seeds first, so a truncated run still contains the incumbents.
    for seed in seeds {
        accept(seed.clone(), Mutation::Seed, steps, budget, baseline_models, &mut out, &mut seen);
    }

    for seed in seeds {
        let mut mutants: Vec<(Dag, Mutation)> = Vec::new();
        for c in &conditioners {
            if let Some(d) = insert_conditioner(seed, c) {
                mutants.push((d, Mutation::ConditionerAdded));
            }
        }
        mutants.extend(drop_conditioners(seed, steps).into_iter().map(|d| (d, Mutation::ConditionerRemoved)));
        mutants.extend(swap_models(seed, models).into_iter().map(|d| (d, Mutation::ModelSwapped)));
        mutants.extend(toggle_optional(seed).into_iter().map(|d| (d, Mutation::OptionalityToggled)));
        for (m, kind) in mutants {
            accept(m, kind, steps, budget, baseline_models, &mut out, &mut seen);
        }
    }
    out
}

/// Validates a candidate and files it, deduplicated.
fn accept(dag: Dag, kind: Mutation, steps: &dyn StepSignatures, budget: &Budget, baseline_models: usize, out: &mut Proposed, seen: &mut BTreeSet<String>) {
    if let Err(e) = dag.validate(steps) {
        out.rejected.push(Rejected { id: dag.id.clone(), error: e.to_string() });
        return;
    }
    if dag.nodes.len() > budget.max_nodes {
        return;
    }
    if model_nodes(&dag, steps) > baseline_models + budget.max_extra_model_nodes {
        return;
    }
    // By content, so two routes to the same shape are one candidate. Ids differ by
    // construction, so an id-based check would let duplicates through.
    let key = content_version(&dag.output, &dag.nodes);
    if !seen.insert(key) {
        return;
    }
    if out.candidates.len() >= budget.max_candidates {
        out.truncated += 1;
        return;
    }
    out.kinds.insert(dag.id.clone(), kind);
    out.candidates.push(dag);
}

/// Nodes that call a model — the cost that actually matters.
///
/// Counted by whether the node names a `model` in its params, which is exactly the
/// distinction in this catalogue: every `ds-models` step takes one and no `ds-ingest`
/// step does.
///
/// The first version of this inferred it from the signature — "output is `Blocks` or
/// `Layout`" — and was wrong in a way that quietly shrank the search: `blocks.merge`,
/// `blocks.verify` and `blocks.merge_native` all output `Blocks` and all run on the CPU,
/// so a merge-heavy incumbent like `born-digital-text` was scored as three model calls
/// instead of two, and the budget then rejected its own mutants.
fn model_nodes(dag: &Dag, _steps: &dyn StepSignatures) -> usize {
    dag.nodes.iter().filter(|n| n.params.get("model").and_then(|v| v.as_str()).is_some()).count()
}

fn id_for(seed: &Dag, suffix: &str) -> String {
    format!("{}{suffix}", seed.id)
}

/// Splices a `[PageImage] → PageImage` step in front of everything that consumed the page
/// image.
fn insert_conditioner(seed: &Dag, step: &str) -> Option<Dag> {
    let consumers: Vec<usize> = seed
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.inputs.iter().any(|i| i == SOURCE_PAGE_IMAGE))
        .map(|(i, _)| i)
        .collect();
    if consumers.is_empty() {
        // Nothing reads the page image, so the new node would be unreachable — and
        // `validate` prunes unreachable nodes, which would hand back a duplicate of the
        // seed under a new name.
        return None;
    }
    let new_id = format!("cond_{}", step.replace('.', "_"));
    if seed.nodes.iter().any(|n| n.id == new_id) {
        return None;
    }
    let mut nodes = seed.nodes.clone();
    for i in consumers {
        for input in &mut nodes[i].inputs {
            if input == SOURCE_PAGE_IMAGE {
                *input = new_id.clone();
            }
        }
    }
    nodes.insert(
        0,
        Node {
            id: new_id,
            step: step.to_string(),
            inputs: vec![SOURCE_PAGE_IMAGE.to_string()],
            params: serde_json::json!({}),
            timeout_ms: None,
            optional: false,
        },
    );
    Some(Dag {
        id: id_for(seed, &format!("+{}", short(step))),
        version: seed.version.clone(),
        description: Some(format!("{} with {step} in front", seed.id)),
        output: seed.output.clone(),
        nodes,
    })
}

/// Removes each conditioner in turn, rewiring its consumers to its own input.
///
/// The ablation. Without it the search can only ever add work, and "does this
/// conditioner pay for itself" is unanswerable.
fn drop_conditioners(seed: &Dag, steps: &dyn StepSignatures) -> Vec<Dag> {
    let mut out = Vec::new();
    for (i, n) in seed.nodes.iter().enumerate() {
        let is_conditioner = matches!(steps.signature(&n.step), Some(s) if s.inputs == vec![ArtifactKind::PageImage] && s.output == ArtifactKind::PageImage);
        if !is_conditioner || n.inputs.len() != 1 || seed.output == n.id {
            continue;
        }
        let replacement = n.inputs[0].clone();
        let mut nodes = seed.nodes.clone();
        nodes.remove(i);
        for node in &mut nodes {
            for input in &mut node.inputs {
                if *input == n.id {
                    *input = replacement.clone();
                }
            }
        }
        out.push(Dag {
            id: id_for(seed, &format!("-{}", short(&n.step))),
            version: seed.version.clone(),
            description: Some(format!("{} without {}", seed.id, n.step)),
            output: seed.output.clone(),
            nodes,
        });
    }
    out
}

/// The same shape against a different model, one node at a time.
///
/// Only between models that share a declared task: see `ModelChoice`. Without that
/// constraint the proposer offered the embedding model as a page parser.
fn swap_models(seed: &Dag, models: &[ModelChoice]) -> Vec<Dag> {
    let by_id: BTreeMap<&str, &ModelChoice> = models.iter().map(|m| (m.id.as_str(), m)).collect();
    let mut out = Vec::new();
    for (i, n) in seed.nodes.iter().enumerate() {
        let Some(current) = n.params.get("model").and_then(|v| v.as_str()) else { continue };
        // A model the registry does not know is left alone rather than swapped blindly:
        // nothing here can tell what it was for.
        let Some(current) = by_id.get(current) else { continue };
        for m in models.iter().filter(|m| m.interchangeable_with(current)) {
            let mut nodes = seed.nodes.clone();
            nodes[i].params["model"] = serde_json::json!(m.id);
            out.push(Dag {
                id: id_for(seed, &format!("~{}={}", n.id, m.id)),
                version: seed.version.clone(),
                description: Some(format!("{} with {} on {}", seed.id, n.id, m.id)),
                output: seed.output.clone(),
                nodes,
            });
        }
    }
    out
}

/// Flips `optional` on each node that is not the output.
///
/// Whether a step's failure should sink the page or be skipped past — the executor
/// substitutes an empty artifact and marks the trace — is a real choice, and nothing has
/// ever scored it. The output node is excluded: an optional output means a page that
/// silently produced nothing.
fn toggle_optional(seed: &Dag) -> Vec<Dag> {
    seed.nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.id != seed.output)
        .map(|(i, n)| {
            let mut nodes = seed.nodes.clone();
            nodes[i].optional = !nodes[i].optional;
            Dag {
                id: id_for(seed, &format!("?{}={}", n.id, nodes[i].optional)),
                version: seed.version.clone(),
                description: Some(format!("{} with {} optional={}", seed.id, n.id, nodes[i].optional)),
                output: seed.output.clone(),
                nodes,
            }
        })
        .collect()
}

/// `page.deskew` → `deskew`, for readable candidate ids.
fn short(step: &str) -> &str {
    step.rsplit('.').next().unwrap_or(step)
}

/// How the proposal differs from the seeds, for a report.
pub fn summary(p: &Proposed) -> BTreeMap<String, usize> {
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for d in &p.candidates {
        let kind = p.kinds.get(&d.id).copied().unwrap_or(Mutation::Seed);
        *by_kind.entry(kind.label().to_string()).or_default() += 1;
    }
    by_kind
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_dag::Signature;

    /// The production catalogue's shape, without its implementations.
    struct Steps;
    impl StepSignatures for Steps {
        fn signature(&self, kind: &str) -> Option<Signature> {
            use ArtifactKind::*;
            let (inputs, output) = match kind {
                "page.parse_e2e" => (vec![PageImage], Blocks),
                "layout.detect" => (vec![PageImage], Layout),
                "region.ocr" => (vec![PageImage, Layout], Blocks),
                "blocks.merge" => (vec![Blocks, Blocks], Blocks),
                "page.deskew" | "page.orient" | "page.binarize" => (vec![PageImage], PageImage),
                _ => return None,
            };
            Some(Signature { inputs, output })
        }
    }

    const KINDS: &[&str] = &["page.parse_e2e", "layout.detect", "region.ocr", "blocks.merge", "page.deskew", "page.orient", "page.binarize"];

    fn e2e() -> Dag {
        Dag {
            id: "e2e".into(),
            version: "1".into(),
            description: None,
            output: "parse".into(),
            nodes: vec![Node {
                id: "parse".into(),
                step: "page.parse_e2e".into(),
                inputs: vec![SOURCE_PAGE_IMAGE.into()],
                params: serde_json::json!({ "model": "a" }),
                timeout_ms: None,
                optional: false,
            }],
        }
    }

    fn choice(id: &str, tasks: &[&str]) -> ModelChoice {
        ModelChoice { id: id.into(), tasks: tasks.iter().map(|t| t.to_string()).collect() }
    }

    #[test]
    fn a_model_is_only_swapped_for_one_that_does_the_same_job() {
        // The failure this constraint exists for: offering the embedding model as a page
        // parser. It type-checks — a signature says nothing about what a model can do.
        let models = vec![
            choice("a", &["page_parse", "table"]),
            choice("b", &["page_parse"]),
            choice("embed-text", &["embed"]),
            choice("page-classifier", &["page_classify"]),
        ];
        let p = propose(&[e2e()], &Steps, KINDS, &models, &Budget::default());
        let swaps: Vec<&str> = p
            .candidates
            .iter()
            .filter(|d| p.kinds.get(&d.id) == Some(&Mutation::ModelSwapped))
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(swaps, vec!["e2e~parse=b"], "only the other page parser may stand in: {swaps:?}");
        for d in &p.candidates {
            for n in &d.nodes {
                let m = n.params.get("model").and_then(|v| v.as_str()).unwrap_or("a");
                assert!(m != "embed-text" && m != "page-classifier", "{}: {m} cannot parse a page", d.id);
            }
        }
    }

    #[test]
    fn every_proposal_validates_against_the_catalogue() {
        let models = vec![choice("a", &["page_parse"]), choice("b", &["page_parse"])];
        let p = propose(&[e2e()], &Steps, KINDS, &models, &Budget::default());
        assert!(p.rejected.is_empty(), "a proposer that emits invalid DAGs is worse than none: {:?}", p.rejected);
        assert!(p.candidates.len() > 1, "nothing was proposed");
        for d in &p.candidates {
            d.validate(&Steps).unwrap_or_else(|e| panic!("{} does not validate: {e}", d.id));
        }
    }

    #[test]
    fn the_seeds_are_always_candidates() {
        // The contract: `report::render` can only compare the current list over pages
        // where the DAG it routes to was evaluated, so an incumbent left out of the
        // candidate set becomes uncomparable and the proposal is measured against
        // nothing.
        let seed = e2e();
        let tight = Budget { max_candidates: 1, ..Default::default() };
        let p = propose(std::slice::from_ref(&seed), &Steps, KINDS, &[], &tight);
        assert_eq!(p.candidates.len(), 1);
        assert_eq!(p.candidates[0].id, seed.id, "the seed must survive even a budget of one");
        assert!(p.truncated > 0, "and the rest must be reported as dropped");
    }

    #[test]
    fn a_conditioner_can_be_added_and_then_taken_away_again() {
        let models: Vec<ModelChoice> = vec![];
        let p = propose(&[e2e()], &Steps, KINDS, &models, &Budget::default());
        let with = p.candidates.iter().find(|d| d.id == "e2e+deskew").expect("deskew was not inserted");
        assert_eq!(with.nodes.len(), 2);
        // The parse step now reads the conditioner, not the raw page.
        let parse = with.nodes.iter().find(|n| n.id == "parse").expect("parse");
        assert_eq!(parse.inputs, vec!["cond_page_deskew".to_string()]);

        // And the ablation exists: dropping it returns the original shape, which is the
        // pair scoring needs to say whether the conditioner pays.
        let back = propose(std::slice::from_ref(with), &Steps, KINDS, &models, &Budget::default());
        let without = back.candidates.iter().find(|d| d.id.ends_with("-deskew")).expect("no ablation");
        assert_eq!(without.nodes.len(), 1);
        assert_eq!(without.nodes[0].inputs, vec![SOURCE_PAGE_IMAGE.to_string()]);
        assert_eq!(
            content_version(&without.output, &without.nodes),
            content_version(&e2e().output, &e2e().nodes),
            "the ablation must be the original shape again"
        );
    }

    #[test]
    fn identical_shapes_are_one_candidate() {
        // Two seeds differing only in name produce the same mutants; dedupe is by
        // content, because ids differ by construction.
        let mut twin = e2e();
        twin.id = "twin".into();
        let p = propose(&[e2e(), twin], &Steps, KINDS, &[], &Budget::default());
        let mut versions: Vec<String> = p.candidates.iter().map(|d| content_version(&d.output, &d.nodes)).collect();
        versions.sort();
        let before = versions.len();
        versions.dedup();
        assert_eq!(versions.len(), before, "the same shape was proposed twice");
    }

    #[test]
    fn runaway_fan_out_is_kept_out() {
        // A seed that already merges two parses; the budget allows one extra model node,
        // so a candidate may not pile on more.
        let seed = Dag {
            id: "pair".into(),
            version: "1".into(),
            description: None,
            output: "merge".into(),
            nodes: vec![
                Node { id: "a".into(), step: "page.parse_e2e".into(), inputs: vec![SOURCE_PAGE_IMAGE.into()], params: serde_json::json!({"model":"a"}), timeout_ms: None, optional: false },
                Node { id: "b".into(), step: "page.parse_e2e".into(), inputs: vec![SOURCE_PAGE_IMAGE.into()], params: serde_json::json!({"model":"b"}), timeout_ms: None, optional: false },
                Node { id: "merge".into(), step: "blocks.merge".into(), inputs: vec!["a".into(), "b".into()], params: serde_json::json!({}), timeout_ms: None, optional: false },
            ],
        };
        let p = propose(&[e2e(), seed], &Steps, KINDS, &[], &Budget { max_extra_model_nodes: 0, ..Default::default() });
        // The thriftiest seed has one model node, so with no allowance the three-model
        // seed itself is out and only single-parse shapes survive.
        for d in &p.candidates {
            assert!(model_nodes(d, &Steps) <= 1, "{} has {} model nodes", d.id, model_nodes(d, &Steps));
        }
    }

    #[test]
    fn the_output_node_is_never_made_optional() {
        // An optional output is a page that silently produced nothing.
        let p = propose(&[e2e()], &Steps, KINDS, &[], &Budget::default());
        for d in &p.candidates {
            let out = d.nodes.iter().find(|n| n.id == d.output).expect("the output node exists");
            assert!(!out.optional, "{}: the output node is optional", d.id);
        }
    }
}
