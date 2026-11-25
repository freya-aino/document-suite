//! The graph layer of the vault: the tables the Data Vault proper does not cover.
//!
//! Decision D6/D9: the vault holds the graph next to its provenance, and vectors live in
//! a dedicated store.
//!
//! What is written here is the section tree, section membership, table cells, in-image
//! text, the entity-to-entity `related` shortcut, citations and the document type.
//! Documents, pages, blocks, entities, assertions, mentions, evidence and stitch links
//! are written by [`crate::VaultWriter::write_stage2`] with full Data Vault history
//! semantics — and this runs **inside that same transaction**, from the same run.
//!
//! It used to be a separate `GraphStore` implementation with its own transaction, called
//! by the worker after `write_stage2` had already committed. Two writers for one graph
//! meant two projections that could drift (a defect fixed in one had no counterpart in
//! the other) and, worse, two commits: a crash between them left a run recorded as
//! finished with no sections, cells or in-image text under it.

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::AssertSqlSafe;
use sqlx::{Postgres, Transaction};

use ds_core::{DocTypeSource, DocumentType, HashKey, RunId};
use ds_graph::{DocGraph, EdgeKind, NodeKind};

use crate::{hash_diff, latest_differs};

/// Stored as the serde name, so the column matches what a `DocTypeSource` deserialises.
fn source_name(s: DocTypeSource) -> &'static str {
    match s {
        DocTypeSource::Caller => "caller",
        DocTypeSource::Model => "model",
        DocTypeSource::PageVote => "page_vote",
    }
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

    cell_hk: Vec<String>,
    cell_block: Vec<String>,
    cell_row: Vec<i32>,
    cell_col: Vec<i32>,
    cell_text: Vec<String>,
    cell_header: Vec<bool>,
    cell_column_header: Vec<Option<String>>,
    cell_hash_diff: Vec<String>,

    img_hk: Vec<String>,
    img_block: Vec<String>,
    img_seq: Vec<i32>,
    img_text: Vec<String>,
    img_hash_diff: Vec<String>,

    match_lnk_hk: Vec<String>,
    match_source: Vec<String>,
    match_block: Vec<Option<String>>,
    match_entity: Vec<Option<String>>,
    match_via: Vec<String>,
}

/// A component's key is a readable string (`cell:<block>:<row>:<col>`), not a hash, so
/// hashing it is what makes the column a `CHAR(64)` like every other key here. The input
/// is already unique, so this is a narrowing, not a derivation.
fn component_hk(node_key: &str) -> String {
    HashKey::of(&[node_key]).to_string()
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

        // Cells and in-image text are nodes, so they are collected from the node list;
        // their edges only say which block they belong to, which the node already has.
        for n in &graph.nodes {
            match n.kind {
                NodeKind::Cell => {
                    let Some(block) = n.key.strip_prefix("cell:").and_then(|r| r.split(':').next()) else { continue };
                    let text = n.props.get("text").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    let header = n.props.get("header").and_then(|v| v.as_bool()).unwrap_or(false);
                    let column_header =
                        n.props.get("column_header").and_then(|v| v.as_str()).map(str::to_string);
                    c.cell_hash_diff.push(hash_diff(&json!({ "text": text, "header": header, "column_header": column_header })));
                    c.cell_hk.push(component_hk(&n.key));
                    c.cell_block.push(block.to_string());
                    c.cell_row.push(n.props.get("row").and_then(|v| v.as_i64()).unwrap_or(0) as i32);
                    c.cell_col.push(n.props.get("col").and_then(|v| v.as_i64()).unwrap_or(0) as i32);
                    c.cell_text.push(text);
                    c.cell_header.push(header);
                    c.cell_column_header.push(column_header);
                }
                NodeKind::ImageText => {
                    let Some(block) = n.key.strip_prefix("imgtext:").and_then(|r| r.split(':').next()) else { continue };
                    let text = n.props.get("text").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    c.img_hash_diff.push(hash_diff(&json!({ "text": text })));
                    c.img_hk.push(component_hk(&n.key));
                    c.img_block.push(block.to_string());
                    c.img_seq.push(n.props.get("index").and_then(|v| v.as_i64()).unwrap_or(0) as i32);
                    c.img_text.push(text);
                }
                _ => {}
            }
        }

        for e in &graph.edges {
            match e.kind {
                EdgeKind::MatchesText => {
                    let source = component_hk(&e.from);
                    // A match points at a block or an entity, never both; the target's
                    // shape is how the two are told apart, since `via` says which.
                    let via = e.props.get("via").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                    let (block, entity) = match via.as_str() {
                        "entity_name" => (None, Some(e.to.clone())),
                        _ => (Some(e.to.clone()), None),
                    };
                    c.match_lnk_hk.push(HashKey::of(&[&source, &e.to, &via]).to_string());
                    c.match_source.push(source);
                    c.match_block.push(block);
                    c.match_entity.push(entity);
                    c.match_via.push(via);
                }
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

/// Writes the graph layer into `tx`, which the caller commits.
///
/// Taking the transaction rather than a pool is the whole point: these rows and the Data
/// Vault rows of the same run become visible together or not at all.
pub(crate) async fn write_graph_layer(
    tx: &mut Transaction<'_, Postgres>,
    run_id: RunId,
    document_hk: &str,
    graph: &DocGraph,
    doc_type: Option<&DocumentType>,
    now: DateTime<Utc>,
    src: &str,
) -> sqlx::Result<()> {
    let c = Columns::of(graph);
    let run_uuid = run_id.0;
    // Every statement below runs on the caller's transaction.
    let tx: &mut sqlx::PgConnection = tx;

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
        .bind(src)
        .execute(&mut *tx)
        .await?;

        let sat = format!(
            "INSERT INTO vault.sat_section (section_hk, load_dts, hash_diff, record_source, title, level)
             SELECT v.section_hk, $1, v.hash_diff, $5, v.title, v.level
             FROM UNNEST($2::text[], $3::text[], $4::text[], $6::smallint[]) AS v(section_hk, hash_diff, title, level)
             WHERE {}",
            latest_differs("vault.sat_section", "section_hk")
        );
        sqlx::query(AssertSqlSafe(sat))
            .bind(now)
            .bind(&c.section_hk)
            .bind(&c.section_hash_diff)
            .bind(&c.section_title)
            .bind(src)
            .bind(&c.section_level)
            .execute(&mut *tx)
            .await?;
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
        .bind(src)
        .execute(&mut *tx)
        .await?;
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
        .bind(src)
        .execute(&mut *tx)
        .await?;
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
        .bind(src)
        .execute(&mut *tx)
        .await?;

        let sat = format!(
            "INSERT INTO vault.sat_related (lnk_hk, load_dts, hash_diff, record_source, predicate_id, confidence)
             SELECT v.lnk_hk, $1, v.hash_diff, $5, v.predicate_id, v.confidence
             FROM UNNEST($2::text[], $3::text[], $4::text[], $6::real[]) AS v(lnk_hk, hash_diff, predicate_id, confidence)
             WHERE {}",
            latest_differs("vault.sat_related", "lnk_hk")
        );
        sqlx::query(AssertSqlSafe(sat))
            .bind(now)
            .bind(&c.related_lnk_hk)
            .bind(&c.related_hash_diff)
            .bind(&c.related_predicate)
            .bind(src)
            .bind(&c.related_confidence)
            .execute(&mut *tx)
            .await?;
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
        .bind(src)
        .execute(&mut *tx)
        .await?;
    }

    if !c.cell_hk.is_empty() {
        sqlx::query(
            "INSERT INTO vault.hub_cell (cell_hk, block_hk, run_id, row_idx, col_idx, load_dts, record_source)
             SELECT v.cell_hk, v.block_hk, $5, v.row_idx, v.col_idx, $6, $7
             FROM UNNEST($1::text[], $2::text[], $3::int[], $4::int[]) AS v(cell_hk, block_hk, row_idx, col_idx)
             ON CONFLICT (cell_hk) DO NOTHING",
        )
        .bind(&c.cell_hk)
        .bind(&c.cell_block)
        .bind(&c.cell_row)
        .bind(&c.cell_col)
        .bind(run_uuid)
        .bind(now)
        .bind(src)
        .execute(&mut *tx)
        .await?;

        let sat = format!(
            "INSERT INTO vault.sat_cell (cell_hk, load_dts, hash_diff, record_source, text, header, column_header)
             SELECT v.cell_hk, $1, v.hash_diff, $5, v.text, v.header, v.column_header
             FROM UNNEST($2::text[], $3::text[], $4::text[], $6::bool[], $7::text[])
                  AS v(cell_hk, hash_diff, text, header, column_header)
             WHERE {}",
            latest_differs("vault.sat_cell", "cell_hk")
        );
        sqlx::query(AssertSqlSafe(sat))
            .bind(now)
            .bind(&c.cell_hk)
            .bind(&c.cell_hash_diff)
            .bind(&c.cell_text)
            .bind(src)
            .bind(&c.cell_header)
            .bind(&c.cell_column_header as &Vec<Option<String>>)
            .execute(&mut *tx)
            .await?;
    }

    if !c.img_hk.is_empty() {
        sqlx::query(
            "INSERT INTO vault.hub_image_text (image_text_hk, block_hk, run_id, seq, load_dts, record_source)
             SELECT v.hk, v.block_hk, $4, v.seq, $5, $6
             FROM UNNEST($1::text[], $2::text[], $3::int[]) AS v(hk, block_hk, seq)
             ON CONFLICT (image_text_hk) DO NOTHING",
        )
        .bind(&c.img_hk)
        .bind(&c.img_block)
        .bind(&c.img_seq)
        .bind(run_uuid)
        .bind(now)
        .bind(src)
        .execute(&mut *tx)
        .await?;

        let sat = format!(
            // The alias column must be named `image_text_hk`: `latest_differs`
            // generates `s.<parent> = v.<parent>`, so a shorter alias makes the
            // generated predicate reference a column that does not exist.
            "INSERT INTO vault.sat_image_text (image_text_hk, load_dts, hash_diff, record_source, text)
             SELECT v.image_text_hk, $1, v.hash_diff, $4, v.text
             FROM UNNEST($2::text[], $3::text[], $5::text[]) AS v(image_text_hk, hash_diff, text)
             WHERE {}",
            latest_differs("vault.sat_image_text", "image_text_hk")
        );
        sqlx::query(AssertSqlSafe(sat))
            .bind(now)
            .bind(&c.img_hk)
            .bind(&c.img_hash_diff)
            .bind(src)
            .bind(&c.img_text)
            .execute(&mut *tx)
            .await?;
    }

    if !c.match_lnk_hk.is_empty() {
        sqlx::query(
            "INSERT INTO vault.lnk_image_text_match (lnk_hk, image_text_hk, block_hk, entity_hk, via, load_dts, record_source)
             SELECT v.lnk_hk, v.source, v.block_hk, v.entity_hk, v.via, $6, $7
             FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[])
                  AS v(lnk_hk, source, block_hk, entity_hk, via)
             ON CONFLICT (lnk_hk) DO NOTHING",
        )
        .bind(&c.match_lnk_hk)
        .bind(&c.match_source)
        .bind(&c.match_block as &Vec<Option<String>>)
        .bind(&c.match_entity as &Vec<Option<String>>)
        .bind(&c.match_via)
        .bind(now)
        .bind(src)
        .execute(&mut *tx)
        .await?;
    }

    if let Some(dt) = doc_type {
        let sat = "INSERT INTO vault.sat_document_type (document_hk, load_dts, hash_diff, record_source, run_id, doc_type, confidence, source)\n                 SELECT $1, $2, $3, $4, $5, $6, $7, $8\n                 WHERE NOT EXISTS (\n                     SELECT 1 FROM (SELECT s.hash_diff FROM vault.sat_document_type s\n                                    WHERE s.document_hk = $1 ORDER BY s.load_dts DESC LIMIT 1) l\n                     WHERE l.hash_diff = $3\n                 )";
        let type_name = crate::enum_name(&dt.doc_type);
        let source_name = source_name(dt.source);
        sqlx::query(sat)
            .bind(document_hk)
            .bind(now)
            .bind(hash_diff(&json!({ "doc_type": type_name, "confidence": dt.confidence, "source": source_name })))
            .bind(src)
            .bind(run_uuid)
            .bind(&type_name)
            .bind(dt.confidence)
            .bind(source_name)
            .execute(&mut *tx)
            .await?;
    }

    tracing::debug!(
        sections = c.section_hk.len(),
        section_links = c.tree_lnk_hk.len(),
        members = c.member_lnk_hk.len(),
        related = c.related_lnk_hk.len(),
        cites = c.cites_lnk_hk.len(),
        cells = c.cell_hk.len(),
        image_text = c.img_hk.len(),
        text_matches = c.match_lnk_hk.len(),
        doc_type = ?doc_type.map(|d| d.doc_type),
        "graph layer written"
    );
    Ok(())
}

/// The one piece of the graph answered by a query rather than a table: reading order.
///
/// It reads `sat_block_placement`, which is where Stage 2 writes the document-level view
/// of a block. It used to read `sat_block_content.global_order` — written only by Stage 1,
/// which sees one page at a time and therefore writes NULL there — so it ordered every
/// block by NULL and nothing noticed, because nothing called it. Migration 0011 has the
/// whole story.
///
/// The placement is looked up **per block**, through a lateral join, rather than by
/// ranking the satellite first. Written the obvious way — a `DISTINCT ON (block_hk)`
/// subquery joined to the blocks — the subquery has no predicate to narrow it, so
/// Postgres ranked every placement row in the database before the joins reduced it to one
/// document: on 200k blocks that was a sequential scan and a 19 MB external merge sort
/// for every document read, and it grew with the corpus rather than with the document.
/// This form walks the document's own pages and blocks and takes one indexed lookup per
/// block (migration 0012). Same rows, same order: the latest placement of each block,
/// body blocks only.
pub const READING_ORDER_QUERY: &str = "SELECT b.block_hk \
     FROM vault.hub_page g \
     JOIN vault.hub_block b ON b.page_hk = g.page_hk \
     CROSS JOIN LATERAL (SELECT global_order, furniture FROM vault.sat_block_placement s \
                         WHERE s.block_hk = b.block_hk ORDER BY s.load_dts DESC LIMIT 1) p \
     WHERE g.document_hk = $1 AND b.run_id = $2 AND NOT p.furniture \
     ORDER BY p.global_order";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use ds_graph::{GEdge, GNode};

    fn node(kind: NodeKind, key: &str, props: Value) -> GNode {
        GNode { kind, key: key.into(), props }
    }

    fn edge(kind: EdgeKind, from: &str, to: &str, props: Value) -> GEdge {
        GEdge { kind, from: from.into(), to: to.into(), props }
    }

    fn hex(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
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

    #[test]
    fn cells_and_image_text_become_rows() {
        let (tbl, fig, ent) = (hex('t'), hex('f'), hex('e'));
        let cell_key = format!("cell:{tbl}:1:2");
        let img_key = format!("imgtext:{fig}:0");
        let graph = DocGraph {
            nodes: vec![
                node(NodeKind::Document, &hex('d'), json!({})),
                node(
                    NodeKind::Cell,
                    &cell_key,
                    json!({ "row": 1, "col": 2, "header": false, "text": "88.5", "column_header": "F1" }),
                ),
                node(NodeKind::ImageText, &img_key, json!({ "text": "BERT", "index": 0 })),
            ],
            edges: vec![
                edge(EdgeKind::MatchesText, &img_key, &hex('b'), json!({ "via": "block_text" })),
                edge(EdgeKind::MatchesText, &img_key, &ent, json!({ "via": "entity_name" })),
            ],
            embed_targets: vec![],
        };
        let c = Columns::of(&graph);

        assert_eq!(c.cell_block, [tbl], "the cell's table comes out of its key");
        assert_eq!((c.cell_row[0], c.cell_col[0]), (1, 2));
        assert_eq!(c.cell_text, ["88.5"]);
        assert_eq!(c.cell_column_header, [Some("F1".to_string())]);
        assert_eq!(c.cell_hk[0].len(), 64, "a readable key must narrow to CHAR(64)");

        assert_eq!(c.img_block, [fig]);
        assert_eq!((c.img_seq[0], c.img_text[0].as_str()), (0, "BERT"));

        // Exactly one of block_hk / entity_hk per match, which the table's CHECK enforces.
        assert_eq!(c.match_lnk_hk.len(), 2);
        let block_match = c.match_via.iter().position(|v| v == "block_text").unwrap();
        let entity_match = c.match_via.iter().position(|v| v == "entity_name").unwrap();
        assert!(c.match_block[block_match].is_some() && c.match_entity[block_match].is_none());
        assert!(c.match_entity[entity_match].as_deref() == Some(ent.as_str()) && c.match_block[entity_match].is_none());
    }

    /// A malformed component key is dropped rather than inserted with a bad parent.
    #[test]
    fn components_with_unparseable_keys_are_skipped() {
        let graph = DocGraph {
            nodes: vec![
                node(NodeKind::Document, &hex('d'), json!({})),
                node(NodeKind::Cell, "not-a-cell-key", json!({ "text": "x" })),
                node(NodeKind::ImageText, "also-wrong", json!({ "text": "y" })),
            ],
            edges: vec![],
            embed_targets: vec![],
        };
        let c = Columns::of(&graph);
        assert!(c.cell_hk.is_empty() && c.img_hk.is_empty());
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
