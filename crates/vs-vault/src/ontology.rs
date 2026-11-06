//! `vs_extract::OntologyStore` over `registry.entity_types` / `registry.relation_types`
//! (vs-vault SPEC "Ontology"). Rows merged into another type are left out: the
//! normaliser must only ever return a live type id.

use async_trait::async_trait;

use vs_extract::{ExtractError, OntologyStore, OntologyType};

use crate::ops::{EntityTypeRow, Ops, RelationTypeRow};

/// Ontology access for the `doc` worker.
pub struct PgOntologyStore {
    pub ops: Ops,
}

impl PgOntologyStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { ops: Ops { pool } }
    }
}

pub fn entity_type(r: EntityTypeRow) -> Option<OntologyType> {
    if r.merged_into.is_some() {
        return None;
    }
    Some(OntologyType { id: r.type_id, label: r.label, synonyms: r.synonyms, review_status: r.review_status })
}

pub fn relation_type(r: RelationTypeRow) -> Option<OntologyType> {
    if r.merged_into.is_some() {
        return None;
    }
    Some(OntologyType { id: r.predicate_id, label: r.label, synonyms: r.synonyms, review_status: r.review_status })
}

fn store_err(e: crate::VaultError) -> ExtractError {
    ExtractError::Store(e.to_string())
}

#[async_trait]
impl OntologyStore for PgOntologyStore {
    async fn entity_types(&self) -> Result<Vec<OntologyType>, ExtractError> {
        Ok(self.ops.entity_types().await.map_err(store_err)?.into_iter().filter_map(entity_type).collect())
    }

    async fn relation_types(&self) -> Result<Vec<OntologyType>, ExtractError> {
        Ok(self.ops.relation_types().await.map_err(store_err)?.into_iter().filter_map(relation_type).collect())
    }

    async fn insert_pending_entity_type(&self, id: &str, label: &str) -> Result<(), ExtractError> {
        self.ops.insert_pending_entity_type(id, label).await.map_err(store_err)
    }

    async fn insert_pending_relation_type(&self, id: &str, label: &str, roles: &[String]) -> Result<(), ExtractError> {
        self.ops.insert_pending_relation_type(id, label, roles).await.map_err(store_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_types_are_hidden() {
        let live = EntityTypeRow {
            type_id: "method".into(),
            label: "Method".into(),
            description: None,
            synonyms: vec!["approach".into()],
            parent_type_id: None,
            review_status: "approved".into(),
            merged_into: None,
        };
        let merged = EntityTypeRow { type_id: "approach".into(), merged_into: Some("method".into()), ..live.clone() };
        let t = entity_type(live).unwrap();
        assert_eq!((t.id.as_str(), t.synonyms.len()), ("method", 1));
        assert!(entity_type(merged).is_none());
    }
}
