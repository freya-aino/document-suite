//! `report.md` for promotion review (SPEC.md "Report").

use std::collections::{BTreeMap, HashMap, HashSet};

use vs_core::{PageProfile, Profile};
use vs_router::{DagList, Rule};

use crate::select::{aggregate, describe, Agg, Objective, PageEval, Trust};

const PROFILES: [Profile; 3] = [Profile::Quality, Profile::Balanced, Profile::Fast];

fn profile_name(p: Profile) -> &'static str {
    match p {
        Profile::Quality => "quality",
        Profile::Balanced => "balanced",
        Profile::Fast => "fast",
    }
}

/// Where the router would send a page: `Ok(rule index)` or the default/low-confidence DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    LowConfidence,
    Rule(usize),
    Default,
}

/// Reproduces `vs_router::Router::select` without needing validated DAGs.
pub fn route(list: &DagList, page: &PageProfile, profile: Profile) -> Target {
    let r = &list.routing;
    if page.confidence < r.min_classifier_confidence {
        return Target::LowConfidence;
    }
    r.rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule.profiles.is_empty() || rule.profiles.contains(&profile))
        .filter(|(_, rule)| rule.when.matches(page))
        .max_by(|(ia, a), (ib, b)| {
            a.when
                .specificity()
                .cmp(&b.when.specificity())
                .then(a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
                .then(ib.cmp(ia))
        })
        .map(|(i, _)| Target::Rule(i))
        .unwrap_or(Target::Default)
}

pub fn target_dag(list: &DagList, t: Target) -> &str {
    match t {
        Target::LowConfidence => &list.routing.low_confidence_dag,
        Target::Default => &list.routing.default_dag,
        Target::Rule(i) => &list.routing.rules[i].dag,
    }
}

/// Distinct pages (`page_id` → profile), in first-seen order.
fn pages(evals: &[PageEval]) -> Vec<(&str, &PageProfile)> {
    let mut seen = HashSet::new();
    evals.iter().filter(|e| seen.insert(e.page_id.as_str())).map(|e| (e.page_id.as_str(), &e.profile)).collect()
}

/// Trust summary of a set of evals.
pub fn trust_label(evals: &[&PageEval]) -> String {
    let labelled = evals.iter().filter(|e| e.trust == Trust::Labelled).count();
    match (labelled, evals.len()) {
        (_, 0) => "none".into(),
        (0, _) => "judge-only".into(),
        (l, n) if l == n => "labelled".into(),
        (l, n) => format!("mixed ({}% labelled)", (100 * l / n)),
    }
}

/// Evals of `dag` on the pages a rule matches.
fn rule_evals<'a>(evals: &'a [PageEval], rule: &Rule) -> Vec<&'a PageEval> {
    evals.iter().filter(|e| e.dag_id == rule.dag && rule.when.matches(&e.profile)).collect()
}

/// Mean metrics over evals of possibly different DAGs.
fn agg_of(evals: &[&PageEval]) -> Agg {
    let n = evals.len();
    if n == 0 {
        return Agg::default();
    }
    let sum = evals.iter().fold(Agg::default(), |a, e| Agg { n: a.n + 1, quality: a.quality + e.quality, latency_ms: a.latency_ms + e.latency_ms, gpu_seconds: a.gpu_seconds + e.gpu_seconds });
    Agg { n, quality: sum.quality / n as f32, latency_ms: sum.latency_ms / n as f32, gpu_seconds: sum.gpu_seconds / n as f32 }
}

fn f(v: f32) -> String {
    format!("{v:.3}")
}

fn pct(n: usize, d: usize) -> String {
    if d == 0 { "–".into() } else { format!("{:.1}%", 100.0 * n as f32 / d as f32) }
}

/// Mean quality/latency/gpu of a routing over the evaluated pages: for every page and
/// caller profile, the eval of the DAG the list routes to (pages without an eval of that
/// DAG are skipped).
pub fn routed_outcome(list: &DagList, evals: &[PageEval], profile: Profile) -> (Agg, usize) {
    let by_page: HashMap<(&str, &str), &PageEval> = evals.iter().map(|e| ((e.page_id.as_str(), e.dag_id.as_str()), e)).collect();
    let mut hits = vec![];
    let ps = pages(evals);
    for (pid, prof) in &ps {
        let dag = target_dag(list, route(list, prof, profile));
        if let Some(e) = by_page.get(&(*pid, dag)) {
            hits.push(*e);
        }
    }
    (agg_of(&hits), ps.len())
}

/// Renders the Markdown report.
pub fn render(list: &DagList, evals: &[PageEval], obj: &Objective, current: Option<&DagList>) -> String {
    let mut md = String::new();
    let ps = pages(evals);
    let n_pages = ps.len();
    let labelled_pages = {
        let mut s = HashSet::new();
        for e in evals.iter().filter(|e| e.trust == Trust::Labelled) {
            s.insert(e.page_id.as_str());
        }
        s.len()
    };
    let dag_ids: HashSet<&str> = evals.iter().map(|e| e.dag_id.as_str()).collect();

    md.push_str(&format!("# DAG list `{}`\n\n", list.version));
    if let Some(c) = &list.created_by {
        md.push_str(&format!("Created by: {c}\n\n"));
    }
    md.push_str("## Summary\n\n");
    md.push_str(&format!("- Pages evaluated: **{n_pages}** ({labelled_pages} labelled, {} judge-only)\n", n_pages - labelled_pages));
    md.push_str(&format!("- Candidate DAGs evaluated: {}; DAGs in the list: {}; routing rules: {}\n", dag_ids.len(), list.dags.len(), list.routing.rules.len()));
    md.push_str(&format!("- Default DAG: `{}`; low-confidence DAG: `{}` (confidence < {})\n", list.routing.default_dag, list.routing.low_confidence_dag, list.routing.min_classifier_confidence));
    md.push_str(&format!(
        "- Objective: quality floor {}, latency ref {} ms, GPU ref {} s, min pages per class {}\n\n",
        obj.quality_floor, obj.latency_ref_ms, obj.gpu_ref_seconds, obj.min_pages
    ));

    // Rules per profile
    md.push_str("## Rules\n\n");
    md.push_str("Coverage = share of evaluated pages the router sends to the rule for that caller profile. ");
    md.push_str("Quality, latency and GPU-seconds are means of the chosen DAG over the pages the rule matches. ");
    md.push_str("Rules marked **judge-only** have no labelled page behind them (assumption A15).\n\n");
    for profile in PROFILES {
        let coverage: HashMap<Target, usize> = ps.iter().fold(HashMap::new(), |mut m, (_, prof)| {
            *m.entry(route(list, prof, profile)).or_default() += 1;
            m
        });
        md.push_str(&format!("### Profile `{}`\n\n", profile_name(profile)));
        md.push_str("| Rule | Class | DAG | Utility | Quality | Latency ms | GPU s | Pages | Trust | Coverage |\n");
        md.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
        let mut any = false;
        for (i, rule) in list.routing.rules.iter().enumerate() {
            if !(rule.profiles.is_empty() || rule.profiles.contains(&profile)) {
                continue;
            }
            any = true;
            let es = rule_evals(evals, rule);
            let a = agg_of(&es);
            let trust = trust_label(&es);
            let flag = if trust == "judge-only" { " **judge-only**" } else { "" };
            md.push_str(&format!(
                "| `{}` | {} | `{}` | {} | {} | {:.0} | {} | {} | {}{} | {} |\n",
                rule.id,
                describe(&rule.when),
                rule.dag,
                f(rule.score),
                f(a.quality),
                a.latency_ms,
                f(a.gpu_seconds),
                a.n,
                trust,
                flag,
                pct(coverage.get(&Target::Rule(i)).copied().unwrap_or(0), n_pages)
            ));
        }
        for (t, name) in [(Target::Default, "default"), (Target::LowConfidence, "low confidence")] {
            let dag = target_dag(list, t);
            let es: Vec<&PageEval> = evals.iter().filter(|e| e.dag_id == dag).collect();
            let a = agg_of(&es);
            md.push_str(&format!(
                "| _{name}_ | any | `{dag}` | – | {} | {:.0} | {} | {} | {} | {} |\n",
                f(a.quality),
                a.latency_ms,
                f(a.gpu_seconds),
                a.n,
                trust_label(&es),
                pct(coverage.get(&t).copied().unwrap_or(0), n_pages)
            ));
        }
        if !any {
            md.push_str("\n_No class-specific rule for this profile; everything goes to the default._\n");
        }
        let (routed, _) = routed_outcome(list, evals, profile);
        md.push_str(&format!(
            "\nRouted outcome over evaluated pages: quality {}, latency {:.0} ms, GPU {} s ({} pages with an eval of the routed DAG).\n\n",
            f(routed.quality),
            routed.latency_ms,
            f(routed.gpu_seconds),
            routed.n
        ));
    }

    // Per-class detail: best DAG per fully specified class (level 0) for each profile
    md.push_str("## Classes\n\n");
    md.push_str("Fully discretised classes (back-off level 0) with at least one evaluated page; the DAG with the best balanced utility and the one the list routes to.\n\n");
    md.push_str("| Class | Pages | Trust | Best DAG (balanced) | Utility | Routed DAG (balanced) |\n|---|---|---|---|---|---|\n");
    let mut classes: BTreeMap<String, Vec<&PageEval>> = BTreeMap::new();
    for e in evals {
        classes.entry(describe(&crate::select::class_key(&e.profile, 0))).or_default().push(e);
    }
    let w = obj.weights(Profile::Balanced);
    for (name, es) in &classes {
        let n = es.iter().map(|e| e.page_id.as_str()).collect::<HashSet<_>>().len();
        let aggs = aggregate(es.iter().copied());
        let best = crate::select::best(&aggs, obj, w);
        let routed = target_dag(list, route(list, &es[0].profile, Profile::Balanced));
        md.push_str(&format!(
            "| {name} | {n} | {} | {} | {} | `{routed}` |\n",
            trust_label(es),
            best.as_ref().map(|b| format!("`{}`", b.0)).unwrap_or_else(|| "_none above floor_".into()),
            best.as_ref().map(|b| f(b.1)).unwrap_or_else(|| "–".into()),
        ));
    }
    md.push('\n');

    // Candidate overview
    md.push_str("## Candidates\n\n| DAG | Pages | Failed | Quality | Latency ms | GPU s | Trust | In list |\n|---|---|---|---|---|---|---|---|\n");
    let mut by_dag: BTreeMap<&str, Vec<&PageEval>> = BTreeMap::new();
    for e in evals {
        by_dag.entry(e.dag_id.as_str()).or_default().push(e);
    }
    let in_list: HashSet<&str> = list.dags.iter().map(|d| d.id.as_str()).collect();
    for (dag, es) in &by_dag {
        let a = agg_of(es);
        let failed = es.iter().filter(|e| e.failed).count();
        md.push_str(&format!(
            "| `{dag}` | {} | {failed} | {} | {:.0} | {} | {} | {} |\n",
            a.n,
            f(a.quality),
            a.latency_ms,
            f(a.gpu_seconds),
            trust_label(es),
            if in_list.contains(dag) { "yes" } else { "" }
        ));
    }
    md.push('\n');

    // Comparison
    if let Some(cur) = current {
        md.push_str(&format!("## Comparison with current list `{}`\n\n", cur.version));
        md.push_str(&format!(
            "- DAGs: {} → {}; rules: {} → {}; default: `{}` → `{}`\n\n",
            cur.dags.len(),
            list.dags.len(),
            cur.routing.rules.len(),
            list.routing.rules.len(),
            cur.routing.default_dag,
            list.routing.default_dag
        ));
        md.push_str("| Profile | Pages changing DAG | Current quality | New quality | Current latency ms | New latency ms | Current GPU s | New GPU s |\n|---|---|---|---|---|---|---|---|\n");
        for profile in PROFILES {
            let changed = ps.iter().filter(|(_, p)| target_dag(cur, route(cur, p, profile)) != target_dag(list, route(list, p, profile))).count();
            let (c, _) = routed_outcome(cur, evals, profile);
            let (n, _) = routed_outcome(list, evals, profile);
            md.push_str(&format!(
                "| {} | {} | {} ({} pages) | {} ({} pages) | {:.0} | {:.0} | {} | {} |\n",
                profile_name(profile),
                pct(changed, n_pages),
                f(c.quality),
                c.n,
                f(n.quality),
                n.n,
                c.latency_ms,
                n.latency_ms,
                f(c.gpu_seconds),
                f(n.gpu_seconds)
            ));
        }
        md.push_str("\n_Current-list numbers only cover pages where the DAG it routes to was evaluated in this run._\n\n");
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::select;
    use vs_core::{ClassifierTier, DocType, LanguageTag};

    fn profile(digital: bool) -> PageProfile {
        PageProfile {
            language: LanguageTag::parse("en").unwrap(),
            doc_type: DocType::Report,
            born_digital: digital,
            text_layer_coverage: if digital { 1.0 } else { 0.0 },
            has_handwriting: false,
            has_images: false,
            has_tables: false,
            has_formulas: false,
            has_charts: false,
            rotation_deg: 0,
            confidence: 0.9,
            tier: ClassifierTier::Rules,
        }
    }

    fn evals() -> Vec<PageEval> {
        let mut v = vec![];
        for i in 0..6 {
            let d = profile(true);
            let s = profile(false);
            v.push(PageEval::new(format!("d{i}"), d.clone(), "native", 0.95, 200.0, 0.0, Trust::Labelled));
            v.push(PageEval::new(format!("d{i}"), d, "vlm", 0.96, 3000.0, 2.0, Trust::Labelled));
            v.push(PageEval::new(format!("s{i}"), s.clone(), "native", 0.1, 200.0, 0.0, Trust::Judge));
            v.push(PageEval::new(format!("s{i}"), s, "vlm", 0.9, 3000.0, 2.0, Trust::Judge));
        }
        v
    }

    #[test]
    fn report_lists_rules_coverage_and_flags() {
        let obj = Objective { min_pages: 3, ..Default::default() };
        let evals = evals();
        let list = select(&evals, vec![], &obj, "t1", None).unwrap();
        let md = render(&list, &evals, &obj, None);
        assert!(md.contains("# DAG list `t1`"));
        assert!(md.contains("Pages evaluated: **12** (6 labelled, 6 judge-only)"));
        // the digital rule sits at the back-off level where it first differs from its parent
        assert!(md.contains("`balanced/lang=en/doc=report/digital=true`"), "{md}");
        assert!(md.contains("| `native` |"));
        assert!(md.contains("50.0%")); // digital rule covers half the pages
        assert!(md.contains("## Candidates"));
        assert!(md.contains("| `vlm` | 12 | 0 |"));
        // the default (vlm) carries the judge-only scan pages → mixed trust, not flagged
        assert!(md.contains("mixed (50% labelled)"));
    }

    #[test]
    fn routing_matches_router_semantics() {
        let obj = Objective { min_pages: 3, ..Default::default() };
        let evals = evals();
        let list = select(&evals, vec![], &obj, "t1", None).unwrap();
        assert_eq!(target_dag(&list, route(&list, &profile(true), Profile::Balanced)), "native");
        assert_eq!(target_dag(&list, route(&list, &profile(false), Profile::Balanced)), "vlm");
        let mut low = profile(true);
        low.confidence = 0.1;
        assert_eq!(route(&list, &low, Profile::Balanced), Target::LowConfidence);
        let (a, n) = routed_outcome(&list, &evals, Profile::Balanced);
        assert_eq!(n, 12);
        assert_eq!(a.n, 12);
        assert!((a.quality - (0.95 + 0.9) / 2.0).abs() < 1e-5);
    }

    #[test]
    fn comparison_section() {
        let obj = Objective { min_pages: 3, ..Default::default() };
        let evals = evals();
        let list = select(&evals, vec![], &obj, "new", None).unwrap();
        let mut cur = list.clone();
        cur.version = "old".into();
        cur.routing.rules.clear(); // everything to vlm
        let md = render(&list, &evals, &obj, Some(&cur));
        assert!(md.contains("## Comparison with current list `old`"));
        assert!(md.contains("| balanced | 50.0% |"));
    }

    #[test]
    fn trust_labels() {
        let p = profile(true);
        let l = PageEval::new("a", p.clone(), "d", 1.0, 1.0, 0.0, Trust::Labelled);
        let j = PageEval::new("b", p, "d", 1.0, 1.0, 0.0, Trust::Judge);
        assert_eq!(trust_label(&[]), "none");
        assert_eq!(trust_label(&[&l]), "labelled");
        assert_eq!(trust_label(&[&j]), "judge-only");
        assert_eq!(trust_label(&[&l, &j]), "mixed (50% labelled)");
    }
}
