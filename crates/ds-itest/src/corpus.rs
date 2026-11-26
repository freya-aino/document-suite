//! The generated corpus, and getting it through the API.
//!
//! The corpus is `ds_adapt::dataset`'s: small PDFs written from known geometry, with the
//! ground truth emitted from the same geometry, so the labels are mechanically exact
//! rather than annotated. What that buys and what it does not is in
//! `crates/ds-adapt/SPEC.md`; the short version is that these pages are born-digital,
//! one typeface, upright and clean, so they measure the machinery and not document
//! parsing.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One line of `manifest.jsonl`.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestEntry {
    pub uri: String,
    pub language: String,
    pub doc_type_hint: Option<String>,
    pub ground_truth_uri: Option<String>,
    #[serde(default = "train")]
    pub split: String,
}

fn train() -> String {
    "train".into()
}

impl ManifestEntry {
    /// `prose-01`, from the document's path.
    pub fn id(&self) -> String {
        Path::new(&self.uri).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    }
}

/// A written corpus on disk.
#[derive(Debug, Clone)]
pub struct Corpus {
    pub dir: PathBuf,
    pub entries: Vec<ManifestEntry>,
}

/// A nonce unique to this test run, for `freshened`.
///
/// Every test needs one of its own. The corpus generator is deterministic by design, so
/// two tests that each "generate their own corpus" in fact produce **the same bytes** —
/// and therefore the same content hash, the same `document_hk` and the same document on
/// the server. They then race over one document's current run, which is how
/// `GET /v1/documents/{hk}` came to report `processing` to one test while the other was
/// still working. Deterministic input is the right property for a corpus and the wrong
/// one for test isolation; this is the seam between them.
pub fn nonce(test: &str) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{test}-{}-{now}", std::process::id())
}

/// The same document, with bytes nobody has uploaded before.
///
/// Needed for any timing. Ingest skips rendering a page whose `signals.json` already
/// exists in the blob store, and the key is derived from the content hash — so a second
/// upload of identical bytes is a *different*, much faster operation, and a latency
/// measurement over a re-run would silently be measuring the cache. The generator is
/// deterministic by design, so the nonce has to come from here.
///
/// A comment appended after `%%EOF` is the least invasive way to change the hash: a PDF
/// reader locates the cross-reference table through `startxref` near the end of the file
/// and tolerates trailing bytes, so the document itself is untouched — the same page, a
/// different object.
pub fn freshened(bytes: &[u8], nonce: &str) -> Vec<u8> {
    let mut out = bytes.to_vec();
    out.extend_from_slice(format!("\n% ds-itest {nonce}\n").as_bytes());
    out
}

impl Corpus {
    /// Writes a corpus under `dir`.
    pub fn generate(dir: &Path, docs_per_shape: usize, pages_per_doc: u32) -> anyhow::Result<Self> {
        let spec = ds_adapt::dataset::Spec {
            docs_per_shape,
            pages_per_doc,
            // Everything in one split: this suite scores what it uploaded, and the
            // train/holdout discipline belongs to a selection run, not to a smoke test.
            // Pretending otherwise would put a holdout label on pages that were also
            // used to choose something.
            holdout_share: 0.0,
            language: "en".into(),
            // Clean, upright pages. The perturbed variants are a separate measurement:
            // see `ds_adapt::dataset::Perturb`.
            perturb: ds_adapt::dataset::Perturb::none(),
        };
        let written = ds_adapt::dataset::write(dir, &spec)?;
        let text = std::fs::read_to_string(&written.manifest)?;
        let entries = text
            .lines()
            .map(|l| serde_json::from_str::<ManifestEntry>(l).map_err(anyhow::Error::from))
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(!entries.is_empty(), "the generator wrote no documents");
        Ok(Self { dir: dir.to_path_buf(), entries })
    }

    /// The ground-truth blocks for one page, through the same reader `ds-adapt score`
    /// uses.
    pub fn ground_truth(&self, e: &ManifestEntry, page_no: u32) -> anyhow::Result<Vec<ds_adapt::metrics::EvalBlock>> {
        let uri = e.ground_truth_uri.as_deref().ok_or_else(|| anyhow::anyhow!("{} is unlabelled", e.id()))?;
        let page = ds_adapt::score::load_ground_truth(uri, page_no)?
            .ok_or_else(|| anyhow::anyhow!("{}: no ground truth for page {page_no}", e.id()))?;
        Ok(ds_adapt::score::gt_blocks(&page))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshening_changes_the_bytes_but_not_the_document() {
        let dir = std::env::temp_dir().join(format!("ds-itest-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let corpus = Corpus::generate(&dir, 1, 1).expect("generate");
        let original = std::fs::read(&corpus.entries[0].uri).expect("read");

        let a = freshened(&original, "run-1");
        let b = freshened(&original, "run-2");
        assert_ne!(a, b, "two runs must not upload identical bytes, or the second measures the render cache");
        assert_ne!(a, original);
        // Still the same document: the header and the trailer a reader looks for are
        // intact, and the nonce sits after `%%EOF` rather than inside the body.
        assert!(a.starts_with(b"%PDF-1.7"));
        let eof = a.windows(5).position(|w| w == b"%%EOF").expect("the trailer survives");
        assert!(eof > 0 && eof < original.len(), "the nonce must follow the trailer, not replace it");
        assert_eq!(&a[..original.len()], &original[..], "the document itself is byte-identical");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
