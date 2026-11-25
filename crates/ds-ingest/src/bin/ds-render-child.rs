//! Standalone render child (the worker uses `ds-worker render-child`; this binary is
//! for tests and local use).

fn main() -> std::io::Result<()> {
    ds_ingest::child::serve(std::io::stdin().lock(), std::io::stdout().lock())
}
