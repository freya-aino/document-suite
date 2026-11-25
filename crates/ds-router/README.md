# ds-router

Chooses the Stage 1 DAG for each page. This is where most of the throughput gain comes
from: the routing table encodes which pages can skip expensive steps. System spec:
[`docs/SPEC.md`](../../docs/SPEC.md) §5.2 — this crate is the "skipping work" lever.

## What it does

Loads a `DagList`, validates it against a step catalogue, and picks one DAG per page by
specificity then score, with a low-confidence fallback and per-call version pinning.
This is where most of the throughput gain comes from, because the routing table is what
decides which pages skip which steps.

## Known limitations

- Nothing ships a routing table that uses the rotation conditions. The router can match
  on them and `ds-adapt` has rotation in its class ladder, so the question is answerable —
  but whether a sideways page class wants a DAG with `page.orient` is a measurement nobody
  has made.

## DAG list (the deployable bundle)

A **DAG list** is the unit that `ds-adapt` produces and that gets promoted to production.

```jsonc
{
  "version": "2026-11-06.1",
  "created_by": "adapt-run/0193…",          // traceability to the scoring run
  "dags": [ /* ds_dag::Dag objects */ ],
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
        "score": 0.91                            // utility from ds-adapt, for tie-breaks and reports
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
| `rotated` | bool | the page is turned away from upright at all (`rotation_deg != 0`) |
| `rotated_deg` | list of degrees | the page's exact rotation is one of them: `[90, 270]` is the sideways case, `[180]` the inverted one |

Each set field counts 1 towards specificity. An empty `when` matches everything
(specificity 0) and acts as a profile-specific default.

The two rotation conditions are the newest and the reason is worth stating: the signal was
measured for PDFs and then for rasters, and until these existed no rule could *ask* about
it — so a sideways page was routed exactly like an upright one and was only straightened
if the DAG it happened to land on included `page.orient`. `rotated` is the question a
routing table usually has ("does this need straightening before anything reads it?"), and
`rotated_deg` is for the rule that treats a quarter turn, which is unreadable, differently
from an inverted page, which is merely upside down.

Note what `rotation_deg` means: how far the *source* page is turned, not a correction to
apply. See `PageSignals::rotation_deg`.

## Load-time validation (`DagList::load`)

- every DAG validates against the worker's `StepRegistry` (`ds-dag` rules);
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

All three are emitted from `select` itself, not from its caller, so every path through
the table is counted — including the two that bypass the rules entirely, which is where a
routing table goes wrong unnoticed. Those two carry the pseudo-rule names
`low_confidence` and `default` in the `rule` label, so `router_selections_total` always
accounts for every page the table saw.
