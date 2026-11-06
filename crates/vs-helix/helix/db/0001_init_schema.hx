// Migration 0001: initial data model (schema::1). See ../../SPEC.md "Data model".
//
// Every later change adds a new file with `schema::N+1 { ... }` and a
// `MIGRATION schema::N => schema::N+1 { ... }` block; schemas are never edited in place.
//
// Conventions:
// - Keys are the Data Vault hash keys from vs-core (hex SHA-256), so every node can be
//   traced back to the vault. Helix 2.3.5 quirks shape the indexes:
//   * secondary indexes are global per field name (shared by all node types);
//   * lookups through a UNIQUE INDEX do not find stored nodes, plain INDEX lookups do.
//   So key fields are plain INDEX (used for lookups), and the shared nodes (Document,
//   Entity) carry a separate write-only `uk` ("document:<hk>" / "entity:<hk>") under
//   UNIQUE INDEX, which makes a second create fail with "Duplicate key on unique index".
//   Run nodes need no constraint: a run is dropped before it is written again.
// - `tenant` is on every node and vector; every query filters by it.
// - Document and Entity are shared across runs (created once, then updated). Everything else (pages,
//   sections, blocks, assertions, their vectors and edges) belongs to one document run
//   and carries `run_id`: a run is written by dropping that run's nodes and adding them
//   again (retry-safe), then Document.current_run_id is switched and the previous run
//   dropped.
// - JSON-shaped values (qualifiers, literals, table HTML / LaTeX content) are strings.

schema::1 {
    N::Document {
        UNIQUE INDEX uk: String,
        INDEX document_hk: String,
        INDEX tenant: String,
        title: String,
        language: String,
        page_count: I64,
        current_run_id: String DEFAULT "",
        doc_types: [String],
    }

    N::Page {
        // page_hk + run id (pages are per run like the blocks on them)
        INDEX page_key: String,
        INDEX page_hk: String,
        INDEX document_hk: String,
        INDEX run_id: String,
        tenant: String,
        page_no: I64,
        doc_type: String,
        dag_ref: String,
    }

    N::Section {
        INDEX section_key: String,
        INDEX document_hk: String,
        INDEX run_id: String,
        tenant: String,
        title: String,
        level: I64,
    }

    N::Block {
        INDEX block_hk: String,
        INDEX document_hk: String,
        INDEX run_id: String,
        tenant: String,
        kind: String,
        text: String,
        page_no: I64,
        // normalised [x0, y0, x1, y1] on the rendered page
        bbox: [F64],
        global_order: I64,
        confidence: F64,
        // table HTML, formula LaTeX or figure description; empty for text blocks
        content: String,
    }

    N::Entity {
        UNIQUE INDEX uk: String,
        INDEX entity_hk: String,
        INDEX tenant: String,
        name: String,
        type_id: String,
        aliases: [String],
    }

    N::Assertion {
        INDEX assertion_key: String,
        INDEX document_hk: String,
        INDEX run_id: String,
        tenant: String,
        predicate_id: String,
        predicate: String,
        kind: String,
        qualifiers: String,
        literals: String,
        confidence: F64,
        verified: Boolean,
    }

    // ── Structure layer ──
    E::HasPage { From: Document, To: Page, }
    E::HasSection { From: Document, To: Section, }
    E::HasSubsection { From: Section, To: Section, }
    E::Contains { From: Section, To: Block, }
    E::OnPage { From: Block, To: Page, }
    E::Next { From: Block, To: Block, }
    E::ContinuesOn { From: Block, To: Block, }
    E::TableContinues { From: Block, To: Block, }
    E::CaptionOf { From: Block, To: Block, }
    E::RefersTo { From: Block, To: Block, }

    // ── Link and semantic layers ──
    E::Mentions {
        From: Block,
        To: Entity,
        Properties: {
            char_start: I64,
            char_end: I64,
            surface: String,
            confidence: F64,
        }
    }
    E::Arg {
        From: Assertion,
        To: Entity,
        Properties: {
            role: String,
        }
    }
    E::Evidence {
        From: Assertion,
        To: Block,
        Properties: {
            char_start: I64,
            char_end: I64,
        }
    }
    E::Related {
        From: Entity,
        To: Entity,
        Properties: {
            predicate_id: String,
            assertion_key: String,
            run_id: String,
            confidence: F64,
        }
    }
    E::Cites { From: Block, To: Entity, }

    // ── Vectors ── (one index per embedding source; `model_ref` records the embedder)
    V::BlockText {
        INDEX block_hk: String,
        tenant: String,
        run_id: String,
        model_ref: String,
    }
    V::SectionText {
        INDEX section_key: String,
        tenant: String,
        run_id: String,
        model_ref: String,
    }
    V::EntityName {
        INDEX entity_hk: String,
        tenant: String,
        model_ref: String,
    }
    E::BlockEmbedding { From: Block, To: BlockText, }
    E::SectionEmbedding { From: Section, To: SectionText, }
    E::EntityEmbedding { From: Entity, To: EntityName, }
}
