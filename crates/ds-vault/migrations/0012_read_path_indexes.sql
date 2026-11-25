-- Indexes for the read path, which never got any.
--
-- Every table in migration 0002 is indexed on its own hash key and on nothing else, so
-- every traversal away from a key — the pages of a document, the blocks of a page, the
-- mentions in a block, the arguments of an assertion — was a sequential scan. The graph
-- layer added in 0005 indexes both directions of every link it defines, which is the
-- standard this brings 0002 up to; the raw vault simply predates it.
--
-- Measured on a synthetic corpus of 500 documents / 10k pages / 200k blocks, before:
--
--   docs/STORES.md "finding the current run"   Seq Scan on lnk_run_document
--   docs/STORES.md "blocks of a page"          Parallel Seq Scan, 199 980 rows discarded
--   docs/STORES.md "BlockContext"              Parallel Seq Scan on hub_block
--   ds_vault::READING_ORDER_QUERY              Seq Scan on sat_block_placement (200 000
--                                              rows) + a 19 MB external merge sort, per
--                                              document read
--
-- The last one is a query shape rather than a missing index and is fixed in
-- `READING_ORDER_QUERY` itself, which now looks the latest placement up per block
-- instead of ranking every placement row in the database first. These indexes are what
-- make that lookup, and the three above, index scans.
--
-- Plain `CREATE INDEX`, not `CONCURRENTLY`: this runs in the migration's transaction like
-- every other, and takes a SHARE lock that blocks writes to each table while it builds.
-- On a populated deployment, run it in a window where nothing is ingesting.

-- ───────────── Links: the side that is not the lnk_hk ─────────────
-- A link table is read from both of its parents, never by its own key.
CREATE INDEX lnk_document_page_document_idx   ON vault.lnk_document_page (document_hk);
CREATE INDEX lnk_page_block_page_idx          ON vault.lnk_page_block (page_hk);
-- `(document_hk, load_dts DESC)` rather than `(document_hk)`: `vault.v_current_run` takes
-- the newest row per document, and with the sort key in the index that is a one-row
-- lookup instead of a scan and a sort.
CREATE INDEX lnk_run_document_document_idx    ON vault.lnk_run_document (document_hk, load_dts DESC);
CREATE INDEX lnk_run_document_run_idx         ON vault.lnk_run_document (run_hk);
CREATE INDEX lnk_block_block_from_idx         ON vault.lnk_block_block (from_block_hk);
CREATE INDEX lnk_block_block_to_idx           ON vault.lnk_block_block (to_block_hk);
-- The entity side of these two is indexed in 0002; the other side was not, so "the
-- mentions in this block" and "the arguments and evidence of this assertion" — which is
-- how an assertion is read back at all — both scanned.
CREATE INDEX lnk_mention_block_idx            ON vault.lnk_mention (block_hk);
CREATE INDEX lnk_assertion_arg_assertion_idx  ON vault.lnk_assertion_arg (assertion_hk);
CREATE INDEX lnk_assertion_evidence_assertion_idx ON vault.lnk_assertion_evidence (assertion_hk);
CREATE INDEX lnk_assertion_evidence_block_idx ON vault.lnk_assertion_evidence (block_hk);
-- Provenance of a block: which DAG and model produced it.
CREATE INDEX lnk_block_dag_model_block_idx    ON vault.lnk_block_dag_model (block_hk);

-- ───────────── Hubs: the parent key and the run ─────────────
CREATE INDEX hub_page_document_idx            ON vault.hub_page (document_hk);
CREATE INDEX hub_block_page_idx              ON vault.hub_block (page_hk);
-- `run_id` is how a reader asks for "the blocks of this run", which is the filter in
-- `READING_ORDER_QUERY` and in docs/STORES.md's `BlockContext`.
CREATE INDEX hub_block_run_idx                ON vault.hub_block (run_id);
CREATE INDEX hub_assertion_run_idx            ON vault.hub_assertion (run_id);
