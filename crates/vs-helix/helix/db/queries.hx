// HelixQL queries of visual-suite. These names are the contract with HelixStore
// (vs-helix) and the RAG service; changing a signature is a breaking change.
//
// Every query takes `tenant` and only touches that tenant's data.
//
// Helix 2.3.5 has no conditionals, and a lookup by key fails with "No value found" when
// the key is missing (it is compiled as exactly-one). So "create if missing" is
// Create* (the unique index rejects a duplicate with "Duplicate key on unique index",
// which the writer treats as "already there") followed by Update*.

// ───────────────────────────── Write path ─────────────────────────────
// Order per document run (SPEC.md "Write algorithm"):
//   GetDocument            previous current_run_id ("No value found" = new document)
//   DropRun(run)           clean up a partial earlier attempt of the same run
//   CreateDocument, UpdateDocument
//   CreateEntity (entities new to the corpus; with their vector), UpdateEntities
//   AddPages, AddSections, AddBlocks, AddAssertions, Link*, Add*Vectors
//   SetCurrentRun(run)
//   DropRun(previous run)

// `uk` = "document:<document_hk>" (unique constraint; see the schema).
QUERY CreateDocument(tenant: String, uk: String, document_hk: String, title: String, language: String, page_count: I64, doc_types: [String]) =>
    doc <- AddN<Document>({uk: uk, document_hk: document_hk, tenant: tenant, title: title, language: language, page_count: page_count, doc_types: doc_types})
    RETURN "ok"

QUERY UpdateDocument(tenant: String, document_hk: String, title: String, language: String, page_count: I64, doc_types: [String]) =>
    doc <- N<Document>({document_hk: document_hk})::WHERE(_::{tenant}::EQ(tenant))::UPDATE({title: title, language: language, page_count: page_count, doc_types: doc_types})
    RETURN "ok"

QUERY GetDocument(tenant: String, document_hk: String) =>
    doc <- N<Document>({document_hk: document_hk})::WHERE(_::{tenant}::EQ(tenant))
    RETURN doc

QUERY SetCurrentRun(tenant: String, document_hk: String, run_id: String) =>
    doc <- N<Document>({document_hk: document_hk})::WHERE(_::{tenant}::EQ(tenant))::UPDATE({current_run_id: run_id})
    RETURN "ok"

// Entity, its name vector and the edge in one transaction: a duplicate key aborts all
// three, so an entity never gets a second vector.
// `uk` = "entity:<entity_hk>".
QUERY CreateEntity(tenant: String, uk: String, entity_hk: String, name: String, type_id: String, aliases: [String], model_ref: String, vector: [F64]) =>
    entity <- AddN<Entity>({uk: uk, entity_hk: entity_hk, tenant: tenant, name: name, type_id: type_id, aliases: aliases})
    vec <- AddV<EntityName>(vector, {entity_hk: entity_hk, tenant: tenant, model_ref: model_ref})
    AddE<EntityEmbedding>::From(entity)::To(vec)
    RETURN "ok"

// For an entity without an embedding (should not happen; every entity is an embed target).
QUERY CreateEntityWithoutVector(tenant: String, uk: String, entity_hk: String, name: String, type_id: String, aliases: [String]) =>
    entity <- AddN<Entity>({uk: uk, entity_hk: entity_hk, tenant: tenant, name: name, type_id: type_id, aliases: aliases})
    RETURN "ok"

QUERY UpdateEntities(tenant: String, entities: [{entity_hk: String, name: String, type_id: String, aliases: [String]}]) =>
    FOR {entity_hk, name, type_id, aliases} IN entities {
        entity <- N<Entity>({entity_hk: entity_hk})::UPDATE({name: name, type_id: type_id, aliases: aliases})
    }
    RETURN "ok"

QUERY AddPages(tenant: String, document_hk: String, run_id: String, pages: [{page_key: String, page_hk: String, page_no: I64, doc_type: String, dag_ref: String}]) =>
    doc <- N<Document>({document_hk: document_hk})
    FOR {page_key, page_hk, page_no, doc_type, dag_ref} IN pages {
        page <- AddN<Page>({page_key: page_key, page_hk: page_hk, document_hk: document_hk, run_id: run_id, tenant: tenant, page_no: page_no, doc_type: doc_type, dag_ref: dag_ref})
        AddE<HasPage>::From(doc)::To(page)
    }
    RETURN "ok"

QUERY AddSections(tenant: String, document_hk: String, run_id: String, sections: [{section_key: String, title: String, level: I64}]) =>
    FOR {section_key, title, level} IN sections {
        section <- AddN<Section>({section_key: section_key, document_hk: document_hk, run_id: run_id, tenant: tenant, title: title, level: level})
    }
    RETURN "ok"

// Top-level sections hang off the document, the others off their parent section.
QUERY LinkDocumentSections(tenant: String, document_hk: String, section_keys: [{section_key: String}]) =>
    doc <- N<Document>({document_hk: document_hk})
    FOR {section_key} IN section_keys {
        section <- N<Section>({section_key: section_key})
        AddE<HasSection>::From(doc)::To(section)
    }
    RETURN "ok"

QUERY LinkSubsections(tenant: String, pairs: [{parent: String, child: String}]) =>
    FOR {parent, child} IN pairs {
        p <- N<Section>({section_key: parent})
        c <- N<Section>({section_key: child})
        AddE<HasSubsection>::From(p)::To(c)
    }
    RETURN "ok"

QUERY AddBlocks(tenant: String, document_hk: String, run_id: String, blocks: [{block_hk: String, page_key: String, kind: String, text: String, page_no: I64, bbox: [F64], global_order: I64, confidence: F64, content: String}]) =>
    FOR {block_hk, page_key, kind, text, page_no, bbox, global_order, confidence, content} IN blocks {
        block <- AddN<Block>({block_hk: block_hk, document_hk: document_hk, run_id: run_id, tenant: tenant, kind: kind, text: text, page_no: page_no, bbox: bbox, global_order: global_order, confidence: confidence, content: content})
        page <- N<Page>({page_key: page_key})
        AddE<OnPage>::From(block)::To(page)
    }
    RETURN "ok"

QUERY LinkContains(tenant: String, pairs: [{section_key: String, block_hk: String}]) =>
    FOR {section_key, block_hk} IN pairs {
        s <- N<Section>({section_key: section_key})
        b <- N<Block>({block_hk: block_hk})
        AddE<Contains>::From(s)::To(b)
    }
    RETURN "ok"

QUERY LinkNext(tenant: String, pairs: [{from: String, to: String}]) =>
    FOR {from, to} IN pairs {
        a <- N<Block>({block_hk: from})
        b <- N<Block>({block_hk: to})
        AddE<Next>::From(a)::To(b)
    }
    RETURN "ok"

QUERY LinkContinuesOn(tenant: String, pairs: [{from: String, to: String}]) =>
    FOR {from, to} IN pairs {
        a <- N<Block>({block_hk: from})
        b <- N<Block>({block_hk: to})
        AddE<ContinuesOn>::From(a)::To(b)
    }
    RETURN "ok"

QUERY LinkTableContinues(tenant: String, pairs: [{from: String, to: String}]) =>
    FOR {from, to} IN pairs {
        a <- N<Block>({block_hk: from})
        b <- N<Block>({block_hk: to})
        AddE<TableContinues>::From(a)::To(b)
    }
    RETURN "ok"

QUERY LinkCaptionOf(tenant: String, pairs: [{from: String, to: String}]) =>
    FOR {from, to} IN pairs {
        a <- N<Block>({block_hk: from})
        b <- N<Block>({block_hk: to})
        AddE<CaptionOf>::From(a)::To(b)
    }
    RETURN "ok"

QUERY LinkRefersTo(tenant: String, pairs: [{from: String, to: String}]) =>
    FOR {from, to} IN pairs {
        a <- N<Block>({block_hk: from})
        b <- N<Block>({block_hk: to})
        AddE<RefersTo>::From(a)::To(b)
    }
    RETURN "ok"

QUERY AddBlockVectors(tenant: String, run_id: String, model_ref: String, items: [{block_hk: String, vector: [F64]}]) =>
    FOR {block_hk, vector} IN items {
        block <- N<Block>({block_hk: block_hk})
        vec <- AddV<BlockText>(vector, {block_hk: block_hk, tenant: tenant, run_id: run_id, model_ref: model_ref})
        AddE<BlockEmbedding>::From(block)::To(vec)
    }
    RETURN "ok"

QUERY AddSectionVectors(tenant: String, run_id: String, model_ref: String, items: [{section_key: String, vector: [F64]}]) =>
    FOR {section_key, vector} IN items {
        section <- N<Section>({section_key: section_key})
        vec <- AddV<SectionText>(vector, {section_key: section_key, tenant: tenant, run_id: run_id, model_ref: model_ref})
        AddE<SectionEmbedding>::From(section)::To(vec)
    }
    RETURN "ok"

QUERY LinkMentions(tenant: String, items: [{block_hk: String, entity_hk: String, char_start: I64, char_end: I64, surface: String, confidence: F64}]) =>
    FOR {block_hk, entity_hk, char_start, char_end, surface, confidence} IN items {
        b <- N<Block>({block_hk: block_hk})
        e <- N<Entity>({entity_hk: entity_hk})
        AddE<Mentions>({char_start: char_start, char_end: char_end, surface: surface, confidence: confidence})::From(b)::To(e)
    }
    RETURN "ok"

QUERY LinkCites(tenant: String, items: [{block_hk: String, entity_hk: String}]) =>
    FOR {block_hk, entity_hk} IN items {
        b <- N<Block>({block_hk: block_hk})
        e <- N<Entity>({entity_hk: entity_hk})
        AddE<Cites>::From(b)::To(e)
    }
    RETURN "ok"

QUERY AddAssertions(tenant: String, document_hk: String, run_id: String, assertions: [{assertion_key: String, predicate_id: String, predicate: String, kind: String, qualifiers: String, literals: String, confidence: F64, verified: Boolean}]) =>
    FOR {assertion_key, predicate_id, predicate, kind, qualifiers, literals, confidence, verified} IN assertions {
        a <- AddN<Assertion>({assertion_key: assertion_key, document_hk: document_hk, run_id: run_id, tenant: tenant, predicate_id: predicate_id, predicate: predicate, kind: kind, qualifiers: qualifiers, literals: literals, confidence: confidence, verified: verified})
    }
    RETURN "ok"

QUERY LinkArgs(tenant: String, items: [{assertion_key: String, entity_hk: String, role: String}]) =>
    FOR {assertion_key, entity_hk, role} IN items {
        a <- N<Assertion>({assertion_key: assertion_key})
        e <- N<Entity>({entity_hk: entity_hk})
        AddE<Arg>({role: role})::From(a)::To(e)
    }
    RETURN "ok"

QUERY LinkEvidence(tenant: String, items: [{assertion_key: String, block_hk: String, char_start: I64, char_end: I64}]) =>
    FOR {assertion_key, block_hk, char_start, char_end} IN items {
        a <- N<Assertion>({assertion_key: assertion_key})
        b <- N<Block>({block_hk: block_hk})
        AddE<Evidence>({char_start: char_start, char_end: char_end})::From(a)::To(b)
    }
    RETURN "ok"

QUERY LinkRelated(tenant: String, run_id: String, items: [{from: String, to: String, predicate_id: String, assertion_key: String, confidence: F64}]) =>
    FOR {from, to, predicate_id, assertion_key, confidence} IN items {
        a <- N<Entity>({entity_hk: from})
        b <- N<Entity>({entity_hk: to})
        AddE<Related>({predicate_id: predicate_id, assertion_key: assertion_key, run_id: run_id, confidence: confidence})::From(a)::To(b)
    }
    RETURN "ok"

// Removes one run's subgraph of a document: Related edges it produced, vectors, and
// its assertions, blocks, sections and pages (their edges go with them). Shared
// Document and Entity nodes stay.
// (HelixQL 2.x: DROP takes an inline traversal, not a variable.)
QUERY DropRun(tenant: String, document_hk: String, run_id: String) =>
    DROP N<Assertion>({document_hk: document_hk})::WHERE(_::{run_id}::EQ(run_id))::Out<Arg>::OutE<Related>::WHERE(_::{run_id}::EQ(run_id))
    DROP N<Assertion>({document_hk: document_hk})::WHERE(_::{run_id}::EQ(run_id))
    DROP N<Block>({document_hk: document_hk})::WHERE(_::{run_id}::EQ(run_id))::Out<BlockEmbedding>
    DROP N<Block>({document_hk: document_hk})::WHERE(_::{run_id}::EQ(run_id))
    DROP N<Section>({document_hk: document_hk})::WHERE(_::{run_id}::EQ(run_id))::Out<SectionEmbedding>
    DROP N<Section>({document_hk: document_hk})::WHERE(_::{run_id}::EQ(run_id))
    DROP N<Page>({document_hk: document_hk})::WHERE(_::{run_id}::EQ(run_id))
    RETURN "ok"

// ─────────────────────── Corpus entity resolution ───────────────────────
// Vector search is post-filtered by tenant, so callers ask for more than they need.

QUERY SearchEntityNames(tenant: String, vector: [F64], k: I64) =>
    vecs <- SearchV<EntityName>(vector, k)::WHERE(_::{tenant}::EQ(tenant))
    entities <- vecs::In<EntityEmbedding>
    RETURN entities

QUERY GetEntity(tenant: String, entity_hk: String) =>
    entity <- N<Entity>({entity_hk: entity_hk})::WHERE(_::{tenant}::EQ(tenant))
    RETURN entity

// ───────────────────────────── Retrieval ─────────────────────────────
// Reference patterns for the RAG service (SPEC.md "Retrieval patterns"); provisional
// until the RAG service's needs are confirmed.

QUERY SearchBlocks(tenant: String, vector: [F64], k: I64) =>
    vecs <- SearchV<BlockText>(vector, k)::WHERE(_::{tenant}::EQ(tenant))
    blocks <- vecs::In<BlockEmbedding>
    RETURN blocks

QUERY SearchBlocksText(tenant: String, text: String, k: I64) =>
    hits <- SearchBM25<Block>(text, k)
    blocks <- hits::WHERE(_::{tenant}::EQ(tenant))
    RETURN blocks

QUERY BlockContext(tenant: String, block_hk: String) =>
    block <- N<Block>({block_hk: block_hk})::WHERE(_::{tenant}::EQ(tenant))
    next <- block::Out<Next>
    previous <- block::In<Next>
    // (not `continues`: Helix 2.3.5 codegen singularises it to the keyword `continue`)
    continuations <- block::Out<ContinuesOn>
    captions <- block::In<CaptionOf>
    referenced <- block::Out<RefersTo>
    page <- block::Out<OnPage>
    RETURN block, next, previous, continuations, captions, referenced, page

QUERY EntityAssertions(tenant: String, entity_hk: String) =>
    entity <- N<Entity>({entity_hk: entity_hk})::WHERE(_::{tenant}::EQ(tenant))
    assertions <- entity::In<Arg>
    arguments <- assertions::Out<Arg>
    evidence <- assertions::Out<Evidence>
    RETURN entity, assertions, arguments, evidence

QUERY RelatedEntities(tenant: String, entity_hk: String) =>
    entity <- N<Entity>({entity_hk: entity_hk})::WHERE(_::{tenant}::EQ(tenant))
    related <- entity::Out<Related>
    edges <- entity::OutE<Related>
    RETURN related, edges

QUERY DocumentOutline(tenant: String, document_hk: String) =>
    doc <- N<Document>({document_hk: document_hk})::WHERE(_::{tenant}::EQ(tenant))
    sections <- doc::Out<HasSection>
    subsections <- sections::Out<HasSubsection>
    pages <- doc::Out<HasPage>
    RETURN doc, sections, subsections, pages
