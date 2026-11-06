//! Objective, class back-off and DAG-list selection.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use vs_core::{PageProfile, Profile};
use vs_dag::Dag;
use vs_router::{DagList, Match, RoutingTable, Rule};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    Labelled,
    Judge,
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

fn aggregate<'a>(evals: impl Iterator<Item = &'a PageEval>) -> HashMap<String, Agg> {
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
fn best(aggs: &HashMap<String, Agg>, obj: &Objective, w: Weights) -> Option<(String, f32, usize)> {
    let mut v: Vec<(&String, f32, usize)> = aggs.iter().filter_map(|(id, a)| obj.utility(w, a).map(|u| (id, u, a.n))).collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(b.0)));
    v.first().map(|(id, u, n)| ((*id).clone(), *u, *n))
}

/// Number of back-off levels: level 0 = all 8 conditions, level 7 = language only.
pub const LEVELS: usize = 8;

/// Class key of a profile at a back-off level.
pub fn class_key(p: &PageProfile, level: usize) -> Match {
    let keep = |rank: usize| rank >= level; // rank 0 is dropped first
    Match {
        has_charts: keep(0).then_some(p.has_charts),
        has_images: keep(1).then_some(p.has_images),
        has_formulas: keep(2).then_some(p.has_formulas),
        has_tables: keep(3).then_some(p.has_tables),
        has_handwriting: keep(4).then_some(p.has_handwriting),
        born_digital: keep(5).then_some(p.born_digital),
        doc_types: keep(6).then(|| vec![p.doc_type]),
        languages: keep(7).then(|| vec![p.language.primary()]),
    }
}

fn describe(m: &Match) -> String {
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
    ] {
        if let Some(b) = v {
            parts.push(format!("{name}={b}"));
        }
    }
    if parts.is_empty() { "any".into() } else { parts.join("/") }
}

fn key_string(m: &Match) -> String {
    serde_json::to_string(m).unwrap_or_default()
}

/// Builds the DAG list from evaluations (SPEC.md "Objective and selection").
pub fn select(evals: &[PageEval], dags: Vec<Dag>, obj: &Objective, version: &str, created_by: Option<String>) -> Option<DagList> {
    let balanced = obj.weights(Profile::Balanced);
    let all = aggregate(evals.iter());
    let (default_dag, _, _) = best(&all, obj, balanced)?;
    let quality_only = Weights { quality: 1.0, latency: 0.0, cost: 0.0 };
    let low_confidence_dag = best(&all, obj, quality_only).map(|b| b.0).unwrap_or_else(|| default_dag.clone());

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
                if let Some((dag, u, n)) = best(&aggs, obj, w) {
                    if n >= obj.min_pages {
                        choice.insert((level, k), (m, dag, u, n));
                    }
                }
            }
        }
        // emit, pruning rules whose parent picks the same DAG
        let global_best = best(&all, obj, w).map(|b| b.0);
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
    use vs_core::{ClassifierTier, DocType, LanguageTag};

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
        PageEval { page_id: format!("p{i}"), profile: p.clone(), dag_id: dag.into(), quality: q, latency_ms: lat, gpu_seconds: gpu, trust: Trust::Labelled }
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
    fn class_key_backoff() {
        let p = profile("de-CH", true);
        assert_eq!(class_key(&p, 0).has_charts, Some(false));
        let k7 = class_key(&p, 7);
        assert_eq!(k7.languages, Some(vec!["de".to_string()]));
        assert!(k7.born_digital.is_none() && k7.doc_types.is_none());
    }
}
