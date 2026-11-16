//! `GraphStore` over Postgres (graph) and a vector store (embeddings).
//!
//! Decision D6/D9: the vault holds the graph next to its provenance, and vectors live in
//! a dedicated store. This type is the seam — it is the only place that knows both.
//!
//! What it writes here is the graph layer that the Data Vault did not already cover:
//! the section tree, section membership, the entity-to-entity `related` shortcut and
//! citations. Entities, assertions, mentions, evidence and stitch links are written by
//! [`crate::VaultWriter::write_stage2`] with full Data Vault history semantics, from the
//! same run.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;

use vs_core::HashKey;
use vs_core::DocTypeSource;
use vs_graph::{DocGraph, EdgeKind, GraphStore, NodeKind, RunInfo, StoreError, Vectors};
use vs_vectors::VectorStore;

use crate::{hash_diff, latest_differs, record_source};

pub struct PgGraphStore {
    pub pool: PgPool,
    pub vectors: Arc<dyn VectorStore>,
}

/// Stored as the serde name, so the column matches what a `DocTypeSource` deserialises.
fn source_name(s: DocTypeSource) -> &'static str {
    match s {
        DocTypeSource::Caller => "caller",
        DocTypeSource::Model => "model",
        DocTypeSource::PageVote => "page_vote",
    }
}

fn unavailable(e: sqlx::Error) -> StoreError {
    StoreError::Unavailable(format!("postgres: {e}"))
}

/// Columns for one batch insert, built as parallel arrays so each table is one statement
/// regardless of how many rows it has. Postgres `UNNEST` over text arrays is how the rest
/// of this crate writes, and it keeps a 500-block document to a handful of round trips.
#[derive(Default)]
struct Columns {
    section_hk: Vec<String>,
    section_heading: Vec<String>,
    section_title: Vec<String>,
    section_level: Vec<i16>,
    section_hash_diff: Vec<String>,

    tree_lnk_hk: Vec<String>,
    tree_parent: Vec<Option<String>>,
    tree_child: Vec<String>,

    member_lnk_hk: Vec<String>,
    member_section: Vec<String>,
    member_block: Vec<String>,

    related_lnk_hk: Vec<String>,
    related_from: Vec<String>,
    related_to: Vec<String>,
    related_assertion: Vec<String>,
    related_predicate: Vec<String>,
    related_confidence: Vec<f32>,
    related_hash_diff: Vec<String>,

    cites_lnk_hk: Vec<String>,
    cites_block: Vec<String>,
    cites_entity: Vec<String>,
}

/// The graph's section node keys are `sec:<heading_block_hk>`; the vault wants a real
/// hash key. Returns `None` for a key that is not a section reference, so a malformed
/// graph drops the row instead of inserting a bad one.
fn section_hk(node_key: &str) -> Option<HashKey> {
    let heading = node_key.strip_prefix("sec:")?;
    let parsed = crate::hash_key_from_hex(heading)?;
    Some(HashKey::section(&parsed))
}

impl Columns {
    fn of(graph: &DocGraph) -> Self {
        let mut c = Columns::default();

        for n in graph.nodes.iter().filter(|n| n.kind == NodeKind::Section) {
            let Some(hk) = section_hk(&n.key) else { continue };
            let heading = n.props.get("heading_block").and_then(|v| v.as_str()).unwrap_or_default();
            let title = n.props.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let level = n.props.get("level").and_then(|v| v.as_i64()).unwrap_or(1) as i16;
            c.section_hash_diff.push(hash_diff(&json!({ "title": title, "level": level })));
            c.section_hk.push(hk.to_string());
            c.section_heading.push(heading.to_string());
            c.section_title.push(title);
            c.section_level.push(level);
        }

        for e in &graph.edges {
            match e.kind {
                // document → section (top level) or section → section (nesting). The
                // document case is stored with a NULL parent rather than its own table.
                EdgeKind::HasSection => {
                    let Some(child) = section_hk(&e.to) else { continue };
                    let parent = section_hk(&e.from).map(|p| p.to_string());
                    c.tree_lnk_hk.push(
                        HashKey::of(&[parent.as_deref().unwrap_or("document"), child.as_str()]).to_string(),
                    );
                    c.tree_parent.push(parent);
                    c.tree_child.push(child.to_string());
                }
                EdgeKind::Contains => {
                    let Some(section) = section_hk(&e.from) else { continue };
                    c.member_lnk_hk.push(HashKey::of(&[section.as_str(), &e.to]).to_string());
                    c.member_section.push(section.to_string());
                    c.member_block.push(e.to.clone());
                }
                EdgeKind::Related => {
                    let predicate = e.props.get("predicate").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    let assertion = e.props.get("assertion").and_then(|v| v.as_str()).unwrap_or_default();
                    let confidence = e.props.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    // The assertion key in the graph is `asr:<document_hk>:<local_id>`,
                    // not a hash key; hash it so the column stays CHAR(64) and still
                    // points back at exactly one assertion.
                    let assertion_hk = HashKey::of(&[assertion]).to_string();
                    c.related_lnk_hk.push(HashKey::of(&[&e.from, &e.to, &assertion_hk]).to_string());
                    c.related_hash_diff.push(hash_diff(&json!({ "predicate_id": predicate, "confidence": confidence })));
                    c.related_from.push(e.from.clone());
                    c.related_to.push(e.to.clone());
                    c.related_assertion.push(assertion_hk);
                    c.related_predicate.push(predicate);
                    c.related_confidence.push(confidence);
                }
                EdgeKind::Cites => {
                    c.cites_lnk_hk.push(HashKey::of(&[&e.from, &e.to]).to_string());
                    c.cites_block.push(e.from.clone());
                    c.cites_entity.push(e.to.clone());
                }
                _ => {}
            }
        }
        c
    }
}

impl PgGraphStore {
    async fn write_graph(&self, run: &RunInfo, document_hk: &str, graph: &DocGraph) -> Result<(), StoreError> {
        let c = Columns::of(graph);
        let now = Utc::now();
        let src = record_source("doc", "graph", None);
        let run_uuid = run.run_id.0;
        let mut tx = self.pool.begin().await.map_err(unavailable)?;

        if !c.section_hk.is_empty() {
            sqlx::query(
                "INSERT INTO vault.hub_section (section_hk, document_hk, run_id, heading_block_hk, load_dts, record_source)
                 SELECT v.section_hk, $3, $4, v.heading, $5, $6
                 FROM UNNEST($1::text[], $2::text[]) AS v(section_hk, heading)
                 ON CONFLICT (section_hk) DO NOTHING",
            )
            .bind(&c.section_hk)
            .bind(&c.section_heading)
            .bind(document_hk)
            .bind(run_uuid)
            .bind(now)
            .bind(&src)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;

            let sat = format!(
                "INSERT INTO vault.sat_section (section_hk, load_dts, hash_diff, record_source, title, level)
                 SELECT v.section_hk, $1, v.hash_diff, $5, v.title, v.level
                 FROM UNNEST($2::text[], $3::text[], $4::text[], $6::smallint[]) AS v(section_hk, hash_diff, title, level)
                 WHERE {}",
                latest_differs("vault.sat_section", "section_hk")
            );
            sqlx::query(&sat)
                .bind(now)
                .bind(&c.section_hk)
                .bind(&c.section_hash_diff)
                .bind(&c.section_title)
                .bind(&src)
                .bind(&c.section_level)
                .execute(&mut *tx)
                .await
                .map_err(unavailable)?;
        }

        if !c.tree_lnk_hk.is_empty() {
            sqlx::query(
                "INSERT INTO vault.lnk_section_section (lnk_hk, parent_section_hk, child_section_hk, document_hk, run_id, load_dts, record_source)
                 SELECT v.lnk_hk, v.parent, v.child, $4, $5, $6, $7
                 FROM UNNEST($1::text[], $2::text[], $3::text[]) AS v(lnk_hk, parent, child)
                 ON CONFLICT (lnk_hk) DO NOTHING",
            )
            .bind(&c.tree_lnk_hk)
            .bind(&c.tree_parent as &Vec<Option<String>>)
            .bind(&c.tree_child)
            .bind(document_hk)
            .bind(run_uuid)
            .bind(now)
            .bind(&src)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        }

        if !c.member_lnk_hk.is_empty() {
            sqlx::query(
                "INSERT INTO vault.lnk_section_block (lnk_hk, section_hk, block_hk, load_dts, record_source)
                 SELECT v.lnk_hk, v.section_hk, v.block_hk, $4, $5
                 FROM UNNEST($1::text[], $2::text[], $3::text[]) AS v(lnk_hk, section_hk, block_hk)
                 ON CONFLICT (lnk_hk) DO NOTHING",
            )
            .bind(&c.member_lnk_hk)
            .bind(&c.member_section)
            .bind(&c.member_block)
            .bind(now)
            .bind(&src)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        }

        if !c.related_lnk_hk.is_empty() {
            sqlx::query(
                "INSERT INTO vault.lnk_entity_entity (lnk_hk, from_entity_hk, to_entity_hk, assertion_hk, run_id, load_dts, record_source)
                 SELECT v.lnk_hk, v.from_hk, v.to_hk, v.assertion_hk, $5, $6, $7
                 FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[]) AS v(lnk_hk, from_hk, to_hk, assertion_hk)
                 ON CONFLICT (lnk_hk) DO NOTHING",
            )
            .bind(&c.related_lnk_hk)
            .bind(&c.related_from)
            .bind(&c.related_to)
            .bind(&c.related_assertion)
            .bind(run_uuid)
            .bind(now)
            .bind(&src)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;

            let sat = format!(
                "INSERT INTO vault.sat_related (lnk_hk, load_dts, hash_diff, record_source, predicate_id, confidence)
                 SELECT v.lnk_hk, $1, v.hash_diff, $5, v.predicate_id, v.confidence
                 FROM UNNEST($2::text[], $3::text[], $4::text[], $6::real[]) AS v(lnk_hk, hash_diff, predicate_id, confidence)
                 WHERE {}",
                latest_differs("vault.sat_related", "lnk_hk")
            );
            sqlx::query(&sat)
                .bind(now)
                .bind(&c.related_lnk_hk)
                .bind(&c.related_hash_diff)
                .bind(&c.related_predicate)
                .bind(&src)
                .bind(&c.related_confidence)
                .execute(&mut *tx)
                .await
                .map_err(unavailable)?;
        }

        if !c.cites_lnk_hk.is_empty() {
            sqlx::query(
                "INSERT INTO vault.lnk_block_entity_cites (lnk_hk, block_hk, entity_hk, load_dts, record_source)
                 SELECT v.lnk_hk, v.block_hk, v.entity_hk, $4, $5
                 FROM UNNEST($1::text[], $2::text[], $3::text[]) AS v(lnk_hk, block_hk, entity_hk)
                 ON CONFLICT (lnk_hk) DO NOTHING",
            )
            .bind(&c.cites_lnk_hk)
            .bind(&c.cites_block)
            .bind(&c.cites_entity)
            .bind(now)
            .bind(&src)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        }

        if let Some(dt) = &run.doc_type {
            let sat = concat!(
                "INSERT INTO vault.sat_document_type (document_hk, load_dts, hash_diff, record_source, run_id, doc_type, confidence, source)
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8
                 WHERE NOT EXISTS (
                     SELECT 1 FROM (SELECT s.hash_diff FROM vault.sat_document_type s
                                    WHERE s.document_hk = $1 ORDER BY s.load_dts DESC LIMIT 1) l
                     WHERE l.hash_diff = $3
                 )"
            );
            let type_name = crate::enum_name(&dt.doc_type);
            let source_name = source_name(dt.source);
            sqlx::query(&sat)
                .bind(document_hk)
                .bind(now)
                .bind(hash_diff(&json!({ "doc_type": type_name, "confidence": dt.confidence, "source": source_name })))
                .bind(&src)
                .bind(run_uuid)
                .bind(&type_name)
                .bind(dt.confidence)
                .bind(source_name)
                .execute(&mut *tx)
                .await
                .map_err(unavailable)?;
        }

        tx.commit().await.map_err(unavailable)?;
        tracing::debug!(
            sections = c.section_hk.len(),
            section_links = c.tree_lnk_hk.len(),
            members = c.member_lnk_hk.len(),
            related = c.related_lnk_hk.len(),
            cites = c.cites_lnk_hk.len(),
            doc_type = ?run.doc_type.map(|d| d.doc_type),
            "graph layer written"
        );
        Ok(())
    }
}

#[async_trait]
impl GraphStore for PgGraphStore {
    async fn write_run(&self, run: &RunInfo, graph: &DocGraph, vectors: &Vectors) -> Result<(), StoreError> {
        let document_hk = graph
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Document)
            .map(|n| n.key.as_str())
            .ok_or_else(|| StoreError::Rejected("graph has no document node".into()))?;

        // Postgres first: it is the system of record. A vector-store failure retries the
        // whole task, and both halves are idempotent, so the retry converges.
        self.write_graph(run, document_hk, graph).await?;
        self.vectors.write_run(run, graph, vectors).await?;
        Ok(())
    }
}

/// Unused today, kept because it documents the one piece of the graph that is answered by
/// a query rather than a table: reading order.
pub const READING_ORDER_QUERY: &str = "SELECT block_hk FROM vault.sat_block_content \
     WHERE NOT furniture ORDER BY global_order";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use vs_graph::{GEdge, GNode};

    fn node(kind: NodeKind, key: &str, props: Value) -> GNode {
        GNode { kind, key: key.into(), props }
    }

    fn edge(kind: EdgeKind, from: &str, to: &str, props: Value) -> GEdge {
        GEdge { kind, from: from.into(), to: to.into(), props }
    }

    fn hex(c: char) -> String {
        std::iter::repeat(c).take(64).collect()
    }

    #[test]
    fn section_key_derivation() {
        let heading = hex('a');
        let hk = section_hk(&format!("sec:{heading}")).unwrap();
        let expected = HashKey::section(&crate::hash_key_from_hex(&heading).unwrap());
        assert_eq!(hk.as_str(), expected.as_str());
        // Anything that is not a section reference is refused rather than coerced.
        assert!(section_hk(&heading).is_none());
        assert!(section_hk("sec:not-hex").is_none());
    }

    #[test]
    fn maps_sections_tree_and_membership() {
        let (h1, h2, b1) = (hex('1'), hex('2'), hex('b'));
        let graph = DocGraph {
            nodes: vec![
                node(NodeKind::Document, &hex('d'), json!({})),
                node(NodeKind::Section, &format!("sec:{h1}"), json!({ "title": "Intro", "level": 1, "heading_block": h1 })),
                node(NodeKind::Section, &format!("sec:{h2}"), json!({ "title": "Method", "level": 2, "heading_block": h2 })),
            ],
            edges: vec![
                edge(EdgeKind::HasSection, &hex('d'), &format!("sec:{h1}"), Value::Null),
                edge(EdgeKind::HasSection, &format!("sec:{h1}"), &format!("sec:{h2}"), Value::Null),
                edge(EdgeKind::Contains, &format!("sec:{h1}"), &b1, Value::Null),
            ],
            embed_targets: vec![],
        };
        let c = Columns::of(&graph);
        assert_eq!(c.section_title, ["Intro", "Method"]);
        assert_eq!(c.section_level, [1, 2]);
        assert_eq!(c.tree_parent.len(), 2);
        // A top-level section is the NULL-parent row; nesting carries the parent key.
        assert_eq!(c.tree_parent[0], None, "document -> section must store a NULL parent");
        assert!(c.tree_parent[1].is_some(), "section -> section must store its parent");
        assert_eq!(c.member_block, [b1]);
    }

    #[test]
    fn related_edges_carry_predicate_and_assertion() {
        let (e1, e2) = (hex('e'), hex('f'));
        let graph = DocGraph {
            nodes: vec![node(NodeKind::Document, &hex('d'), json!({}))],
            edges: vec![edge(
                EdgeKind::Related,
                &e1,
                &e2,
                json!({ "predicate": "evaluated_on", "assertion": "asr:doc:1", "confidence": 0.75 }),
            )],
            embed_targets: vec![],
        };
        let c = Columns::of(&graph);
        assert_eq!(c.related_predicate, ["evaluated_on"]);
        assert_eq!(c.related_confidence, [0.75]);
        assert_eq!(c.related_assertion[0].len(), 64, "assertion ref must be hashed to CHAR(64)");
        assert_eq!(c.related_from, [e1]);
        assert_eq!(c.related_to, [e2]);
    }

    /// Only the graph's own kinds are written here; the rest belong to `write_stage2`.
    #[test]
    fn ignores_edges_written_elsewhere() {
        let graph = DocGraph {
            nodes: vec![node(NodeKind::Document, &hex('d'), json!({}))],
            edges: vec![
                edge(EdgeKind::OnPage, &hex('b'), &hex('p'), Value::Null),
                edge(EdgeKind::Mentions, &hex('b'), &hex('e'), Value::Null),
                edge(EdgeKind::Next, &hex('b'), &hex('c'), Value::Null),
            ],
            embed_targets: vec![],
        };
        let c = Columns::of(&graph);
        assert!(c.member_lnk_hk.is_empty() && c.related_lnk_hk.is_empty() && c.cites_lnk_hk.is_empty());
    }
}
