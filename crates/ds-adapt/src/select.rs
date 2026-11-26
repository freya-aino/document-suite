//! Objective, class back-off and DAG-list selection.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use ds_core::{PageProfile, Profile};
use ds_dag::Dag;
use ds_router::{DagList, Match, RoutingTable, Rule};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    Labelled,
    Judge,
}

/// Individual metrics behind `PageEval::quality`; `None` = not applicable to the page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Metrics {
    pub text: Option<f32>,
    pub tables: Option<f32>,
    pub formulas: Option<f32>,
    pub reading_order: Option<f32>,
    pub layout: Option<f32>,
    /// Judge VLM rating (judge-trust evals only).
    pub judge: Option<f32>,
    /// Text agreement with the reference DAG (judge-trust evals only).
    ///
    /// A **diagnostic**, not a quality term: it measures consensus, so DAGs sharing a
    /// parse model agree with each other and disagree with a better one. It is excluded
    /// from `quality`, and it is `None` for the reference DAG itself, whose agreement
    /// with its own text is 1.0 by construction.
    pub agreement: Option<f32>,
    // ── detection and order, reported separately ──
    //
    // `layout` and `reading_order` are computed from the same matching, so they are not
    // independent: with correct ordering `reading_order` reduces to detection recall.
    // These four say what the composite cannot.
    /// Block-detection precision at `metrics::DEFAULT_MIN_IOU`.
    pub layout_precision: Option<f32>,
    /// Block-detection recall at `metrics::DEFAULT_MIN_IOU`.
    pub layout_recall: Option<f32>,
    /// Block-detection F1 at IoU 0.75. Reported beside `layout` (0.5): the gap is how
    /// much of the number is the label's box convention rather than the DAG.
    pub layout_f1_strict: Option<f32>,
    /// Kendall tau-b of reading order over the matched blocks only — order alone, with
    /// detection factored out.
    pub order_tau: Option<f32>,
}

/// Score of one DAG on one page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageEval {
    pub page_id: String,
    pub profile: PageProfile,
    pub dag_id: String,
    pub quality: f32,
    pub latency_ms: f32,
    pub gpu_seconds: f32,
    pub trust: Trust,
    // --- added by `score`; absent in older files ---
    #[serde(default)]
    pub dag_version: Option<String>,
    #[serde(default)]
    pub doc: Option<String>,
    #[serde(default)]
    pub page_no: Option<u32>,
    #[serde(default)]
    pub class_key: Option<String>,
    #[serde(default)]
    pub metrics: Metrics,
    #[serde(default)]
    pub tokens: u32,
    /// The DAG failed on this page (quality is 0).
    #[serde(default)]
    pub failed: bool,
    /// How each model node's output was parsed, from the node traces: `auto_path` for the
    /// `auto` family, the family name otherwise.
    ///
    /// Carried this far so that "is this newly registered model being read structurally,
    /// or split as Markdown because nothing understood its output?" is a question the
    /// report can answer. It was previously logged and lost.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parse_paths: Vec<String>,
    /// Critical path through the DAG, split: milliseconds in **our** code and
    /// milliseconds waiting on the model tier. Zero when the run predates the split or
    /// the page's result carries no node traces.
    ///
    /// Here and not only in the run record because this is what a report and an
    /// objective read. `host_ms` is the number a code change moves and the only one worth
    /// gating; `model_ms` moves when the hardware or the prompt does, and a latency
    /// regression that is really a slower GPU is the easiest wrong conclusion in the
    /// suite.
    #[serde(default)]
    pub host_ms: u64,
    #[serde(default)]
    pub model_ms: u64,
    /// `train` or `holdout`, from the run record.
    ///
    /// Carried so that a selection can be made on one split and *measured* on the other.
    /// Without it the final round of successive halving sweeps holdout pages into the
    /// selection records and there is no way to tell them apart afterwards — which makes
    /// the winner's curse unmeasurable rather than merely large.
    #[serde(default)]
    pub split: Option<String>,
}

impl PageEval {
    /// Minimal eval with the optional fields empty.
    pub fn new(page_id: impl Into<String>, profile: PageProfile, dag_id: impl Into<String>, quality: f32, latency_ms: f32, gpu_seconds: f32, trust: Trust) -> Self {
        Self {
            page_id: page_id.into(),
            profile,
            dag_id: dag_id.into(),
            quality,
            latency_ms,
            gpu_seconds,
            trust,
            dag_version: None,
            doc: None,
            page_no: None,
            class_key: None,
            metrics: Metrics::default(),
            tokens: 0,
            failed: false,
            parse_paths: Vec::new(),
            host_ms: 0,
            model_ms: 0,
            split: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Weights {
    pub quality: f32,
    pub latency: f32,
    pub cost: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Objective {
    pub quality_floor: f32,
    pub latency_ref_ms: f32,
    pub gpu_ref_seconds: f32,
    pub quality_profile: Weights,
    pub balanced_profile: Weights,
    pub fast_profile: Weights,
    pub min_pages: usize,
    pub min_classifier_confidence: f32,
}

impl Default for Objective {
    fn default() -> Self {
        Self {
            quality_floor: 0.7,
            latency_ref_ms: 2_000.0,
            gpu_ref_seconds: 1.0,
            quality_profile: Weights { quality: 1.0, latency: 0.02, cost: 0.02 },
            balanced_profile: Weights { quality: 1.0, latency: 0.1, cost: 0.1 },
            fast_profile: Weights { quality: 1.0, latency: 0.4, cost: 0.3 },
            min_pages: 30,
            min_classifier_confidence: 0.6,
        }
    }
}

impl Objective {
    pub fn weights(&self, p: Profile) -> Weights {
        match p {
            Profile::Quality => self.quality_profile,
            Profile::Balanced => self.balanced_profile,
            Profile::Fast => self.fast_profile,
        }
    }

    /// `None` when the DAG is below the quality floor.
    pub fn utility(&self, w: Weights, a: &Agg) -> Option<f32> {
        if a.quality < self.quality_floor {
            return None;
        }
        Some(w.quality * a.quality - w.latency * a.latency_ms / self.latency_ref_ms - w.cost * a.gpu_seconds / self.gpu_ref_seconds)
    }
}

/// Mean metrics of one DAG over a set of pages.
#[derive(Debug, Clone, Copy, Default)]
pub struct Agg {
    pub n: usize,
    pub quality: f32,
    pub latency_ms: f32,
    pub gpu_seconds: f32,
}

pub fn aggregate<'a>(evals: impl Iterator<Item = &'a PageEval>) -> HashMap<String, Agg> {
    let mut sums: HashMap<String, Agg> = HashMap::new();
    for e in evals {
        let a = sums.entry(e.dag_id.clone()).or_default();
        a.n += 1;
        a.quality += e.quality;
        a.latency_ms += e.latency_ms;
        a.gpu_seconds += e.gpu_seconds;
    }
    for a in sums.values_mut() {
        let n = a.n as f32;
        a.quality /= n;
        a.latency_ms /= n;
        a.gpu_seconds /= n;
    }
    sums
}

/// Best DAG by utility (ties → lower id for determinism). Requires every DAG to have the same page set.
/// How complicated each candidate is: `(model nodes, nodes)` per DAG id.
///
/// Model nodes first because that is the cost that matters — a model call is orders of
/// magnitude dearer than a CPU step, and it is the one a deployment pays for per page.
pub type Complexity = HashMap<String, (usize, usize)>;

/// Builds the complexity map from the candidates.
///
/// A node counts as a model node when its params name a `model`, which is exactly the
/// `ds-models`/`ds-ingest` split — the same rule `propose`'s budget uses, and for the
/// same reason: `blocks.merge`, `blocks.verify` and `blocks.merge_native` all output
/// `Blocks` and all run on the CPU, so counting by output kind charges a DAG for calls it
/// never makes.
pub fn complexity(dags: &[Dag]) -> Complexity {
    dags.iter()
        .map(|d| {
            let model_nodes = d.nodes.iter().filter(|n| n.params.get("model").and_then(|v| v.as_str()).is_some()).count();
            (d.id.clone(), (model_nodes, d.nodes.len()))
        })
        .collect()
}

/// The best DAG by utility, breaking ties on **simplicity**.
///
/// The tie-break used to be the lower DAG id, which publishes alphabetical luck as an
/// engineering recommendation — and ties are not rare here: two candidates differing only
/// in a parameter that did not matter on this corpus score identically, and on a small
/// corpus two genuinely different DAGs often do too. Fewest model calls wins, then fewest
/// nodes, and the id only decides when a pair is indistinguishable on both.
///
/// A DAG missing from `cx` is treated as maximally complex rather than as simplest, so an
/// incomplete map cannot promote a candidate by omission.
pub fn best(aggs: &HashMap<String, Agg>, obj: &Objective, w: Weights, cx: &Complexity) -> Option<(String, f32, usize)> {
    let mut v: Vec<(&String, f32, usize)> = aggs.iter().filter_map(|(id, a)| obj.utility(w, a).map(|u| (id, u, a.n))).collect();
    let rank = |id: &String| cx.get(id).copied().unwrap_or((usize::MAX, usize::MAX));
    v.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| rank(a.0).cmp(&rank(b.0)))
            .then_with(|| a.0.cmp(b.0))
    });
    v.first().map(|(id, u, n)| ((*id).clone(), *u, *n))
}

/// Number of back-off levels: level 0 = all 9 conditions, level 8 = language only.
pub const LEVELS: usize = 9;

/// Class key of a profile at a back-off level.
///
/// `rotated` is dropped first, before the content flags, because it is the condition most
/// likely to have no examples: most corpora are upright. It is in the ladder at all
/// because whether a sideways page wants a different workflow — one that straightens it
/// first — is exactly the kind of question this exists to answer, and until the router
/// could match on rotation there was no way to express the answer.
pub fn class_key(p: &PageProfile, level: usize) -> Match {
    let keep = |rank: usize| rank >= level; // rank 0 is dropped first
    Match {
        rotated: keep(0).then_some(!p.rotation_deg.is_multiple_of(360)),
        has_charts: keep(1).then_some(p.has_charts),
        has_images: keep(2).then_some(p.has_images),
        has_formulas: keep(3).then_some(p.has_formulas),
        has_tables: keep(4).then_some(p.has_tables),
        has_handwriting: keep(5).then_some(p.has_handwriting),
        born_digital: keep(6).then_some(p.born_digital),
        doc_types: keep(7).then(|| vec![p.doc_type]),
        languages: keep(8).then(|| vec![p.language.primary()]),
        // The exact angle is never a class: three near-identical classes for 90, 180 and
        // 270 would split what little rotated data a corpus has three ways. A routing
        // table can still be written by hand with `rotated_deg` when a deployment knows
        // it needs to.
        rotated_deg: None,
    }
}

pub fn describe(m: &Match) -> String {
    let mut parts = vec![];
    if let Some(l) = &m.languages {
        parts.push(format!("lang={}", l.join("|")));
    }
    if let Some(d) = &m.doc_types {
        parts.push(format!("doc={}", serde_json::to_value(d[0]).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()));
    }
    for (name, v) in [
        ("digital", m.born_digital),
        ("hw", m.has_handwriting),
        ("tables", m.has_tables),
        ("formulas", m.has_formulas),
        ("images", m.has_images),
        ("charts", m.has_charts),
        ("rotated", m.rotated),
    ] {
        if let Some(b) = v {
            parts.push(format!("{name}={b}"));
        }
    }
    if parts.is_empty() { "any".into() } else { parts.join("/") }
}

pub fn key_string(m: &Match) -> String {
    serde_json::to_string(m).unwrap_or_default()
}

/// Builds the DAG list from evaluations (SPEC.md "Objective and selection").
pub fn select(evals: &[PageEval], dags: Vec<Dag>, obj: &Objective, version: &str, created_by: Option<String>) -> Option<DagList> {
    let balanced = obj.weights(Profile::Balanced);
    let cx = complexity(&dags);
    let all = aggregate(evals.iter());
    let (default_dag, _, _) = best(&all, obj, balanced, &cx)?;
    let quality_only = Weights { quality: 1.0, latency: 0.0, cost: 0.0 };
    let low_confidence_dag = best(&all, obj, quality_only, &cx).map(|b| b.0).unwrap_or_else(|| default_dag.clone());

    let mut rules: Vec<Rule> = vec![];
    for profile in [Profile::Quality, Profile::Balanced, Profile::Fast] {
        let w = obj.weights(profile);
        // choice per (level, class key)
        let mut choice: HashMap<(usize, String), (Match, String, f32, usize)> = HashMap::new();
        for level in 0..LEVELS {
            let mut groups: HashMap<String, (Match, Vec<&PageEval>)> = HashMap::new();
            for e in evals {
                let m = class_key(&e.profile, level);
                groups.entry(key_string(&m)).or_insert_with(|| (m, vec![])).1.push(e);
            }
            for (k, (m, es)) in groups {
                let aggs = aggregate(es.into_iter());
                if let Some((dag, u, n)) = best(&aggs, obj, w, &cx) {
                    if n >= obj.min_pages {
                        choice.insert((level, k), (m, dag, u, n));
                    }
                }
            }
        }
        // emit, pruning rules whose parent picks the same DAG
        let global_best = best(&all, obj, w, &cx).map(|b| b.0);
        let mut keys: Vec<&(usize, String)> = choice.keys().collect();
        keys.sort();
        for key in keys {
            let (level, _) = key;
            let (m, dag, u, _) = &choice[key];
            let parent_dag = (level + 1..LEVELS)
                .find_map(|pl| {
                    let sample = evals.iter().find(|e| m.matches(&e.profile))?;
                    choice.get(&(pl, key_string(&class_key(&sample.profile, pl)))).map(|c| c.1.clone())
                })
                .or_else(|| global_best.clone());
            if parent_dag.as_deref() == Some(dag.as_str()) {
                continue;
            }
            rules.push(Rule {
                id: format!("{}/{}", serde_json::to_value(profile).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(), describe(m)),
                when: m.clone(),
                profiles: vec![profile],
                dag: dag.clone(),
                score: *u,
            });
        }
        // profile-level default when it differs from the global default
        if let Some(gb) = &global_best {
            if gb != &default_dag {
                rules.push(Rule {
                    id: format!("{}/any", serde_json::to_value(profile).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()),
                    when: Match::default(),
                    profiles: vec![profile],
                    dag: gb.clone(),
                    score: 0.0,
                });
            }
        }
    }

    // keep only DAGs that are referenced
    let mut used: Vec<&str> = rules.iter().map(|r| r.dag.as_str()).collect();
    used.push(&default_dag);
    used.push(&low_confidence_dag);
    let dags = dags.into_iter().filter(|d| used.contains(&d.id.as_str())).collect();

    Some(DagList {
        version: version.to_string(),
        created_by,
        dags,
        routing: RoutingTable { default_dag, low_confidence_dag, min_classifier_confidence: obj.min_classifier_confidence, rules },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::{ClassifierTier, DocType, LanguageTag};

    fn profile(lang: &str, digital: bool) -> PageProfile {
        PageProfile {
            language: LanguageTag::parse(lang).unwrap(),
            doc_type: DocType::ScientificPaper,
            born_digital: digital,
            text_layer_coverage: if digital { 1.0 } else { 0.0 },
            has_handwriting: false,
            has_images: false,
            has_tables: true,
            has_formulas: false,
            has_charts: false,
            rotation_deg: 0,
            confidence: 0.9,
            tier: ClassifierTier::Vision,
        }
    }

    fn eval(i: usize, p: &PageProfile, dag: &str, q: f32, lat: f32, gpu: f32) -> PageEval {
        PageEval::new(format!("p{i}"), p.clone(), dag, q, lat, gpu, Trust::Labelled)
    }

    #[test]
    fn utility_respects_floor() {
        let o = Objective::default();
        let a = Agg { n: 1, quality: 0.5, latency_ms: 10.0, gpu_seconds: 0.0 };
        assert_eq!(o.utility(o.balanced_profile, &a), None);
    }

    #[test]
    fn digital_pages_get_cheap_dag_scans_get_robust() {
        let obj = Objective { min_pages: 5, ..Default::default() };
        let digital = profile("en", true);
        let scan = profile("en", false);
        let mut evals = vec![];
        for i in 0..10 {
            // digital: native DAG is as good and much cheaper
            evals.push(eval(i, &digital, "native", 0.95, 200.0, 0.0));
            evals.push(eval(i, &digital, "vlm", 0.96, 3000.0, 2.0));
            // scans: native is useless, vlm is required
            evals.push(eval(100 + i, &scan, "native", 0.1, 200.0, 0.0));
            evals.push(eval(100 + i, &scan, "vlm", 0.9, 3000.0, 2.0));
        }
        let list = select(&evals, vec![], &obj, "t", None).unwrap();
        assert_eq!(list.routing.default_dag, "vlm"); // only DAG above the floor on all pages
        let balanced_digital = list
            .routing
            .rules
            .iter()
            .find(|r| r.profiles == vec![Profile::Balanced] && r.when.born_digital == Some(true))
            .expect("rule for digital pages");
        assert_eq!(balanced_digital.dag, "native");
        // scans fall back to the default; no redundant rule emitted for them
        assert!(!list.routing.rules.iter().any(|r| r.when.born_digital == Some(false) && r.dag == "vlm"));
    }

    #[test]
    fn old_eval_json_still_parses() {
        let j = r#"{"page_id":"p","profile":{"language":"en","doc_type":"other","born_digital":true,"text_layer_coverage":1.0,
            "has_handwriting":false,"has_images":false,"has_tables":false,"has_formulas":false,"has_charts":false,
            "rotation_deg":0,"confidence":0.9,"tier":"rules"},"dag_id":"d","quality":0.5,"latency_ms":1.0,"gpu_seconds":0.0,"trust":"judge"}"#;
        let e: PageEval = serde_json::from_str(j).unwrap();
        assert_eq!(e.trust, Trust::Judge);
        assert!(e.dag_version.is_none() && e.metrics.text.is_none() && !e.failed);
    }

    #[test]
    fn class_key_backoff() {
        let p = profile("de-CH", true);
        let full = class_key(&p, 0);
        assert_eq!(full.has_charts, Some(false));
        // Rotation is in the class, so a corpus with sideways pages can be given its own
        // workflow; it is dropped first because most corpora have no examples of it.
        assert_eq!(full.rotated, Some(false));
        assert_eq!(class_key(&p, 1).rotated, None, "rotation is the first condition to go");

        let last = class_key(&p, LEVELS - 1);
        assert_eq!(last.languages, Some(vec!["de".to_string()]));
        assert!(last.born_digital.is_none() && last.doc_types.is_none(), "the last level is language alone");
        // The exact angle is never part of a class: it would split what little rotated
        // data there is three ways.
        assert!((0..LEVELS).all(|l| class_key(&p, l).rotated_deg.is_none()));
    }
}

#[cfg(test)]
mod tiebreak_tests {
    use super::*;

    fn dag(id: &str, nodes: &[(&str, Option<&str>)]) -> Dag {
        Dag {
            id: id.into(),
            description: None,
            version: "1".into(),
            output: nodes.last().expect("a node").0.into(),
            nodes: nodes
                .iter()
                .map(|(nid, model)| ds_dag::Node {
                    id: (*nid).into(),
                    step: "page.parse_e2e".into(),
                    inputs: vec![],
                    params: match model {
                        Some(m) => serde_json::json!({ "model": m }),
                        None => serde_json::json!({}),
                    },
                    timeout_ms: None,
                    optional: false,
                })
                .collect(),
        }
    }

    fn agg(quality: f32) -> Agg {
        Agg { n: 10, quality, latency_ms: 100.0, gpu_seconds: 0.1 }
    }

    #[test]
    fn a_tie_goes_to_the_simpler_dag_and_not_the_alphabetically_lower() {
        // The defect this pins: the tie-break was `a.0.cmp(b.0)`, so an identical score
        // was resolved by the DAG id — publishing alphabetical luck as an engineering
        // recommendation. `a-expensive` sorts first and calls three models.
        let dags = vec![
            dag("a-expensive", &[("p1", Some("m")), ("p2", Some("m")), ("p3", Some("m"))]),
            dag("z-cheap", &[("p1", Some("m"))]),
        ];
        let cx = complexity(&dags);
        let mut aggs = HashMap::new();
        aggs.insert("a-expensive".to_string(), agg(0.9));
        aggs.insert("z-cheap".to_string(), agg(0.9));
        let obj = Objective { quality_floor: 0.0, ..Objective::default() };
        let w = Weights { quality: 1.0, latency: 0.0, cost: 0.0 };
        assert_eq!(best(&aggs, &obj, w, &cx).expect("a winner").0, "z-cheap");
        // And it is a tie-break, not a preference: a real quality difference still wins.
        aggs.insert("a-expensive".to_string(), agg(0.95));
        assert_eq!(best(&aggs, &obj, w, &cx).expect("a winner").0, "a-expensive");
    }

    #[test]
    fn model_nodes_outrank_node_count() {
        // Two model calls and three nodes beats one model call and six, because the model
        // call is the cost that matters — a CPU step is free next to it.
        let dags = vec![
            dag("many-cheap-nodes", &[("a", None), ("b", None), ("c", None), ("d", None), ("e", None), ("p", Some("m"))]),
            dag("two-model-calls", &[("p1", Some("m")), ("p2", Some("m")), ("merge", None)]),
        ];
        let cx = complexity(&dags);
        assert_eq!(cx["many-cheap-nodes"], (1, 6));
        assert_eq!(cx["two-model-calls"], (2, 3));
        let mut aggs = HashMap::new();
        for id in ["many-cheap-nodes", "two-model-calls"] {
            aggs.insert(id.to_string(), agg(0.8));
        }
        let obj = Objective { quality_floor: 0.0, ..Objective::default() };
        let w = Weights { quality: 1.0, latency: 0.0, cost: 0.0 };
        assert_eq!(best(&aggs, &obj, w, &cx).expect("a winner").0, "many-cheap-nodes");
    }

    #[test]
    fn a_candidate_missing_from_the_map_is_not_promoted_by_omission() {
        let dags = vec![dag("known", &[("p1", Some("m")), ("p2", Some("m"))])];
        let cx = complexity(&dags);
        let mut aggs = HashMap::new();
        aggs.insert("known".to_string(), agg(0.8));
        aggs.insert("unknown".to_string(), agg(0.8));
        let obj = Objective { quality_floor: 0.0, ..Objective::default() };
        let w = Weights { quality: 1.0, latency: 0.0, cost: 0.0 };
        assert_eq!(best(&aggs, &obj, w, &cx).expect("a winner").0, "known", "an unmeasurable DAG must not win a tie");
    }
}
