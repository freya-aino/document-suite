//! DAG list bundle and per-page DAG selection. See `README.md`.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use vs_core::{DocType, PageProfile, Profile};
use vs_dag::{Dag, DagError, StepSignatures};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Match {
    pub languages: Option<Vec<String>>,
    pub doc_types: Option<Vec<DocType>>,
    pub born_digital: Option<bool>,
    pub has_handwriting: Option<bool>,
    pub has_images: Option<bool>,
    pub has_tables: Option<bool>,
    pub has_formulas: Option<bool>,
    pub has_charts: Option<bool>,
    /// Whether the source page is turned away from upright at all
    /// (`PageProfile::rotation_deg != 0`).
    ///
    /// The signal was measured for PDFs and then for rasters too, and no rule could ask
    /// about it — so a sideways page was routed exactly like an upright one and only got
    /// turned if the DAG it happened to land on included `page.orient`. A boolean rather
    /// than the angle because that is the question a routing table has: *does this page
    /// need straightening before anything reads it?* A rule that wants only quarter turns
    /// can pair this with `rotated_deg`.
    pub rotated: Option<bool>,
    /// Exact rotations to match, for a rule that treats a quarter turn differently from
    /// an upside-down page: `[90, 270]` is the sideways case, `[180]` the inverted one.
    pub rotated_deg: Option<Vec<u16>>,
}

impl Match {
    pub fn matches(&self, p: &PageProfile) -> bool {
        let flag = |cond: Option<bool>, v: bool| cond.is_none_or(|c| c == v);
        self.languages.as_ref().is_none_or(|ls| ls.iter().any(|l| l.eq_ignore_ascii_case(&p.language.primary())))
            && self.doc_types.as_ref().is_none_or(|ds| ds.contains(&p.doc_type))
            && flag(self.born_digital, p.born_digital)
            && flag(self.has_handwriting, p.has_handwriting)
            && flag(self.has_images, p.has_images)
            && flag(self.has_tables, p.has_tables)
            && flag(self.has_formulas, p.has_formulas)
            && flag(self.has_charts, p.has_charts)
            && flag(self.rotated, !p.rotation_deg.is_multiple_of(360))
            && self.rotated_deg.as_ref().is_none_or(|ds| ds.contains(&(p.rotation_deg % 360)))
    }

    pub fn specificity(&self) -> u32 {
        [
            self.languages.is_some(),
            self.doc_types.is_some(),
            self.born_digital.is_some(),
            self.has_handwriting.is_some(),
            self.has_images.is_some(),
            self.has_tables.is_some(),
            self.has_formulas.is_some(),
            self.has_charts.is_some(),
            self.rotated.is_some(),
            self.rotated_deg.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count() as u32
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    #[serde(default)]
    pub when: Match,
    /// Empty = applies to all profiles.
    #[serde(default)]
    pub profiles: Vec<Profile>,
    pub dag: String,
    #[serde(default)]
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingTable {
    pub default_dag: String,
    pub low_confidence_dag: String,
    pub min_classifier_confidence: f32,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DagList {
    pub version: String,
    pub created_by: Option<String>,
    pub dags: Vec<Dag>,
    pub routing: RoutingTable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    Rule { specificity: u32 },
    LowConfidence,
    Default,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Selection<'a> {
    pub dag: &'a Dag,
    pub rule_id: Option<&'a str>,
    pub reason: Reason,
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("DAG {dag:?} invalid: {source}")]
    InvalidDag { dag: String, source: DagError },
    #[error("duplicate DAG id {0:?}")]
    DuplicateDag(String),
    #[error("duplicate rule id {0:?}")]
    DuplicateRule(String),
    #[error("{0} references unknown DAG {1:?}")]
    UnknownDag(String, String),
    #[error("min_classifier_confidence must be within [0, 1]")]
    BadThreshold,
}

/// A validated DAG list, ready for selection.
#[derive(Debug, Clone)]
pub struct Router {
    list: DagList,
    by_id: HashMap<String, usize>,
}

/// `router_selections_total{rule,dag,profile}`.
///
/// Emitted from `select` rather than from its caller so that every path through the table
/// is counted — the low-confidence short circuit and the default fall-through included,
/// which is exactly where a routing table goes wrong unnoticed. `rule` carries the
/// pseudo-rules `low_confidence` and `default` for those two, so the three counters here
/// always sum to `router_selections_total`.
fn record_selection(dag: &str, rule: &str, profile: Profile) {
    let profile = match profile {
        Profile::Quality => "quality",
        Profile::Balanced => "balanced",
        Profile::Fast => "fast",
    };
    metrics::counter!("router_selections_total", "rule" => rule.to_string(), "dag" => dag.to_string(), "profile" => profile).increment(1);
}

impl Router {
    pub fn load(list: DagList, steps: &dyn StepSignatures) -> Result<Self, RouterError> {
        let mut by_id = HashMap::new();
        for (i, d) in list.dags.iter().enumerate() {
            if by_id.insert(d.id.clone(), i).is_some() {
                return Err(RouterError::DuplicateDag(d.id.clone()));
            }
            d.validate(steps).map_err(|source| RouterError::InvalidDag { dag: d.id.clone(), source })?;
        }
        let r = &list.routing;
        if !(0.0..=1.0).contains(&r.min_classifier_confidence) {
            return Err(RouterError::BadThreshold);
        }
        for (what, dag) in [("default_dag", &r.default_dag), ("low_confidence_dag", &r.low_confidence_dag)] {
            if !by_id.contains_key(dag) {
                return Err(RouterError::UnknownDag(what.into(), dag.clone()));
            }
        }
        let mut seen = HashSet::new();
        for rule in &r.rules {
            if !seen.insert(rule.id.as_str()) {
                return Err(RouterError::DuplicateRule(rule.id.clone()));
            }
            if !by_id.contains_key(&rule.dag) {
                return Err(RouterError::UnknownDag(format!("rule {}", rule.id), rule.dag.clone()));
            }
        }
        Ok(Router { list, by_id })
    }

    pub fn version(&self) -> &str {
        &self.list.version
    }

    pub fn dag(&self, id: &str) -> Option<&Dag> {
        self.by_id.get(id).map(|&i| &self.list.dags[i])
    }

    pub fn select(&self, page: &PageProfile, profile: Profile) -> Selection<'_> {
        let r = &self.list.routing;
        if page.confidence < r.min_classifier_confidence {
            metrics::counter!("router_low_confidence_total").increment(1);
            record_selection(&r.low_confidence_dag, "low_confidence", profile);
            return Selection { dag: self.dag(&r.low_confidence_dag).expect("validated"), rule_id: None, reason: Reason::LowConfidence };
        }
        let best = r
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.profiles.is_empty() || rule.profiles.contains(&profile))
            .filter(|(_, rule)| rule.when.matches(page))
            // max by (specificity, score, earlier position)
            .max_by(|(ia, a), (ib, b)| {
                a.when
                    .specificity()
                    .cmp(&b.when.specificity())
                    .then(a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
                    .then(ib.cmp(ia))
            });
        match best {
            Some((_, rule)) => {
                record_selection(&rule.dag, &rule.id, profile);
                Selection {
                    dag: self.dag(&rule.dag).expect("validated"),
                    rule_id: Some(&rule.id),
                    reason: Reason::Rule { specificity: rule.when.specificity() },
                }
            }
            None => {
                metrics::counter!("router_default_total").increment(1);
                record_selection(&r.default_dag, "default", profile);
                Selection { dag: self.dag(&r.default_dag).expect("validated"), rule_id: None, reason: Reason::Default }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_core::{ClassifierTier, LanguageTag};
    use vs_dag::{ArtifactKind, Node, Signature};

    struct OneStep;
    impl StepSignatures for OneStep {
        fn signature(&self, _: &str) -> Option<Signature> {
            Some(Signature { inputs: vec![ArtifactKind::PageImage], output: ArtifactKind::Blocks })
        }
    }

    fn dag(id: &str) -> Dag {
        Dag {
            id: id.into(),
            version: "1".into(),
            description: None,
            output: "p".into(),
            nodes: vec![Node {
                id: "p".into(),
                step: "page.parse_e2e".into(),
                inputs: vec!["$page_image".into()],
                params: serde_json::Value::Null,
                timeout_ms: None,
                optional: false,
            }],
        }
    }

    fn page(lang: &str, born_digital: bool, conf: f32) -> PageProfile {
        PageProfile {
            language: LanguageTag::parse(lang).unwrap(),
            doc_type: DocType::ScientificPaper,
            born_digital,
            text_layer_coverage: if born_digital { 0.95 } else { 0.0 },
            has_handwriting: false,
            has_images: true,
            has_tables: true,
            has_formulas: true,
            has_charts: false,
            rotation_deg: 0,
            confidence: conf,
            tier: ClassifierTier::Vision,
        }
    }

    fn router() -> Router {
        let rule = |id: &str, when: Match, profiles: Vec<Profile>, dag: &str, score: f32| Rule {
            id: id.into(),
            when,
            profiles,
            dag: dag.into(),
            score,
        };
        let list = DagList {
            version: "v1".into(),
            created_by: None,
            dags: vec![dag("robust"), dag("digital"), dag("digital-sci-en"), dag("fast-any")],
            routing: RoutingTable {
                default_dag: "robust".into(),
                low_confidence_dag: "robust".into(),
                min_classifier_confidence: 0.6,
                rules: vec![
                    rule("digital", Match { born_digital: Some(true), ..Default::default() }, vec![], "digital", 0.8),
                    rule(
                        "digital-sci-en",
                        Match {
                            languages: Some(vec!["en".into()]),
                            doc_types: Some(vec![DocType::ScientificPaper]),
                            born_digital: Some(true),
                            ..Default::default()
                        },
                        vec![],
                        "digital-sci-en",
                        0.9,
                    ),
                    rule("fast-any", Match::default(), vec![Profile::Fast], "fast-any", 0.5),
                ],
            },
        };
        Router::load(list, &OneStep).unwrap()
    }

    #[test]
    fn most_specific_rule_wins() {
        let r = router();
        let s = r.select(&page("en-GB", true, 0.9), Profile::Balanced);
        assert_eq!(s.dag.id, "digital-sci-en");
        assert_eq!(s.reason, Reason::Rule { specificity: 3 });
    }

    #[test]
    fn language_falls_back_to_less_specific() {
        let r = router();
        assert_eq!(r.select(&page("de", true, 0.9), Profile::Balanced).dag.id, "digital");
    }

    #[test]
    fn default_and_profile_rules() {
        let r = router();
        let s = r.select(&page("de", false, 0.9), Profile::Balanced);
        assert_eq!((s.dag.id.as_str(), s.reason), ("robust", Reason::Default));
        assert_eq!(r.select(&page("de", false, 0.9), Profile::Fast).dag.id, "fast-any");
    }

    #[test]
    fn low_confidence_goes_to_robust() {
        let r = router();
        let s = r.select(&page("en", true, 0.3), Profile::Balanced);
        assert_eq!(s.reason, Reason::LowConfidence);
        assert_eq!(s.dag.id, "robust");
    }

    #[test]
    fn every_path_through_the_table_is_counted() {
        // A thread-local recorder: each `#[test]` gets its own thread, so what this one
        // records is only what it did.
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let r = router();
        r.select(&page("en-GB", true, 0.9), Profile::Balanced); // a rule matched
        r.select(&page("de", false, 0.9), Profile::Balanced); // nothing matched
        r.select(&page("en", true, 0.3), Profile::Quality); // short-circuited on confidence

        let body = handle.render();
        assert!(body.contains("router_default_total 1"), "{body}");
        assert!(body.contains("router_low_confidence_total 1"), "{body}");
        // The two bypass paths carry pseudo-rule names, so `router_selections_total`
        // accounts for every page the table saw rather than only the ones a rule claimed
        // — which is where a routing table goes wrong unnoticed.
        for expect in [
            r#"router_selections_total{rule="digital-sci-en",dag="digital-sci-en",profile="balanced"} 1"#,
            r#"router_selections_total{rule="default",dag="robust",profile="balanced"} 1"#,
            r#"router_selections_total{rule="low_confidence",dag="robust",profile="quality"} 1"#,
        ] {
            assert!(body.contains(expect), "missing {expect}\n{body}");
        }
    }

    /// A page that arrived sideways must be able to select a DAG that straightens it.
    /// The signal was measured and no rule could ask about it, so it changed nothing.
    #[test]
    fn a_sideways_page_can_be_routed_differently_from_an_upright_one() {
        let upright = page("en", true, 0.9);
        let mut sideways = upright.clone();
        sideways.rotation_deg = 270;
        let mut inverted = upright.clone();
        inverted.rotation_deg = 180;

        let any_rotation = Match { rotated: Some(true), ..Default::default() };
        assert!(!any_rotation.matches(&upright));
        assert!(any_rotation.matches(&sideways));
        assert!(any_rotation.matches(&inverted), "upside down is rotated too");

        let upright_only = Match { rotated: Some(false), ..Default::default() };
        assert!(upright_only.matches(&upright));
        assert!(!upright_only.matches(&sideways));

        // The quarter-turn case, which is the one a reader cannot read at all, told apart
        // from the inverted one.
        let quarter = Match { rotated_deg: Some(vec![90, 270]), ..Default::default() };
        assert!(quarter.matches(&sideways));
        assert!(!quarter.matches(&inverted));
        assert!(!quarter.matches(&upright));

        // And it counts towards specificity, so a rotation rule beats a general one.
        assert!(quarter.specificity() > Match::default().specificity());
    }

    #[test]
    fn load_rejects_unknown_dag() {
        let mut list = router().list.clone();
        list.routing.default_dag = "missing".into();
        assert!(matches!(Router::load(list, &OneStep), Err(RouterError::UnknownDag(..))));
    }
}
