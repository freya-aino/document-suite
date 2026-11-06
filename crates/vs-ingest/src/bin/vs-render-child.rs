//! Standalone render child (the worker uses `vs-worker render-child`; this binary is
//! for tests and local use).

fn main() -> std::io::Result<()> {
    vs_ingest::child::serve(std::io::stdin().lock(), std::io::stdout().lock())
}
