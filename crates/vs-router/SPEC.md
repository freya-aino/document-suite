# vs-router — Specification

Chooses the Stage 1 DAG for each page. This is where most of the throughput gain comes
from: the routing table encodes which pages can skip expensive steps.

## DAG list (the deployable bundle)

A **DAG list** is the unit that `vs-adapt` produces and that gets promoted to production.

```jsonc
{
  "version": "2026-11-06.1",
  "created_by": "adapt-run/0193…",          // traceability to the scoring run
  "dags": [ /* vs_dag::Dag objects */ ],
  "routing": {
    "default_dag": "vlm-e2e-robust",          // used when no rule matches
    "low_confidence_dag": "vlm-e2e-robust",   // used when classifier confidence is low
    "min_classifier_confidence": 0.6,
    "rules": [
      {
        "id": "en-sci-digital",
        "when": { "languages": ["en"], "doc_types": ["scientific_paper"], "born_digital": true },
        "profiles": [],                         // empty = all profiles
        "dag": "born-digital-sci",
        "score": 0.91                            // utility from vs-adapt, for tie-breaks and reports
      }
    ]
  }
}
```

Stored in Postgres `registry.dag_lists` (JSONB) with status `candidate | active | retired`.
Exactly one list is `active` per tenant (or globally). Workers cache lists by version.

## Selection algorithm

Input: `PageProfile`, caller `Profile` (`quality | balanced | fast`), the DAG list.

1. If `profile.confidence < min_classifier_confidence` → `low_confidence_dag`.
2. Candidate rules = rules whose `when` matches the page **and** whose `profiles` is empty
   or contains the caller profile.
3. Pick the candidate with the highest **specificity** (number of conditions set in
   `when`); ties broken by higher `score`, then by rule order in the file.
4. No candidate → `default_dag`.

Returns `Selection { dag, rule_id, reason }`; `reason` is logged and stored with the page
result for later analysis (routing hit rates per rule are a key metric).

### Match conditions

| Field | Type | Matches when |
|-------|------|--------------|
| `languages` | list of primary subtags | `profile.language.primary()` is in the list |
| `doc_types` | list of `DocType` | page doc type is in the list |
| `born_digital`, `has_handwriting`, `has_images`, `has_tables`, `has_formulas`, `has_charts` | bool | equal to the profile flag |

Each set field counts 1 towards specificity. An empty `when` matches everything
(specificity 0) and acts as a profile-specific default.

## Load-time validation (`DagList::load`)

- every DAG validates against the worker's `StepRegistry` (`vs-dag` rules);
- DAG ids are unique;
- `default_dag`, `low_confidence_dag` and every rule `dag` exist;
- rule ids are unique;
- `min_classifier_confidence` in `[0, 1]`.

A list that fails validation is never activated; workers keep the previous list.

## Per-call pinning

`JobSpec.dag_list_version` pins a list. If the pinned list is not available the job fails
fast at ingest (no silent fallback), so reproducibility runs never mix lists.

## Metrics

`router_selections_total{rule,dag,profile}`, `router_low_confidence_total`,
`router_default_total`.
