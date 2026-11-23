use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub blob: BlobConfig,
    pub postgres: PostgresConfig,
    pub qdrant: QdrantConfig,
    pub models: ModelsConfig,
    #[serde(default)]
    pub worker: WorkerConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// `vs-api` only.
    #[serde(default)]
    pub api: ApiConfig,
    /// `vs-api` only, and **required** by it: the binary refuses to start without an
    /// `[auth]` section, so there is no configuration in which the API serves requests
    /// without verifying tokens. Optional here because the worker roles never read it.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Passed through to `vs-classify` (`ClassifyConfig`).
    #[serde(default = "empty_table")]
    pub classify: toml::Value,
    /// Stage 2 (`doc` role) and page-classifier models; `page` and `doc` workers only.
    #[serde(default)]
    pub stage2: Stage2Config,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BlobConfig {
    pub bucket: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// Server-side encryption for everything this suite writes: `none` (the default),
    /// `aes256` (SSE-S3), `aws:kms` or `aws:kms:dsse` (both need `sse_kms_key_id`), or
    /// `sse-c` (needs `sse_customer_key_env`).
    ///
    /// The system spec asserts encryption at rest for originals (§5.6) and for a long
    /// time nothing here configured it, which meant it was true only where the bucket had
    /// a default encryption rule — in other words, not a property of this software.
    /// `none` is still the default because a bucket policy is the better place for it when
    /// one exists, and silently sending an encryption header a non-AWS S3 implementation
    /// rejects would break Garage and MinIO deployments that are already encrypting at the
    /// volume. Setting it here is for the deployment that cannot rely on either.
    #[serde(default = "sse_none")]
    pub sse: String,
    /// KMS key for `aws:kms` and `aws:kms:dsse`.
    #[serde(default)]
    pub sse_kms_key_id: Option<String>,
    /// Name of the environment variable holding the base64 256-bit customer key for
    /// `sse-c`. The key itself is never in the config file — it is the whole secret, and a
    /// config file is checked in.
    #[serde(default)]
    pub sse_customer_key_env: Option<String>,
    /// Days to keep each class of derived object. 0 keeps it forever.
    ///
    /// Only *derived* objects have defaults worth having: a page image and a page result
    /// can be rebuilt by re-running the document, so keeping them indefinitely is
    /// storage spent on a cache. Originals default to forever because deleting the input
    /// is a decision about the tenant's own data, and nothing in this software is
    /// entitled to make it.
    #[serde(default)]
    pub retention: RetentionConfig,
}

fn sse_none() -> String {
    "none".into()
}

/// How long each class of object is kept. Days; 0 means forever.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    /// Rendered page images and thumbnails (`pages/`). Rebuildable from the original.
    pub page_images_days: u64,
    /// Stage 1 and Stage 2 result JSON (`results/`). The vault holds the same facts with
    /// their provenance; these are the raw model outputs behind them.
    pub results_days: u64,
    /// Extracted text layers (`textlayer/`). Rebuildable.
    pub text_layers_days: u64,
    /// Block crops handed to models (`crops/`). Rebuildable.
    pub crops_days: u64,
    /// Scratch images uploaded for `image_transport = "url"` (`model-images/`). These are
    /// consumed within one model call, so a day is already generous — and they are the
    /// one class that is **not** rebuildable from anything, because nothing records that
    /// they existed.
    pub model_images_days: u64,
    /// Uploaded originals (`originals/`). 0 — forever — unless a deployment decides
    /// otherwise.
    pub originals_days: u64,
    /// Hours between sweeps. 0 disables the sweep entirely, whatever the retentions say.
    pub sweep_interval_h: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            page_images_days: 30,
            results_days: 90,
            text_layers_days: 30,
            crops_days: 7,
            model_images_days: 1,
            originals_days: 0,
            sweep_interval_h: 6,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostgresConfig {
    /// Without password; password comes from `VS__POSTGRES__PASSWORD`.
    pub url: String,
    /// Set from the environment only.
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_pool")]
    pub max_connections: u32,
}

fn empty_table() -> toml::Value {
    toml::Value::Table(Default::default())
}

fn default_pool() -> u32 {
    16
}

/// Vector store (decision D9). Only the `doc` role connects, but the section is
/// required for every role so a misconfigured deployment fails at startup rather than
/// at the first document that reaches Stage 2.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QdrantConfig {
    /// gRPC endpoint. Qdrant's gRPC port is 6334; 6333 is the REST port and will not work.
    pub url: String,
    /// Env var holding the API key, if the deployment uses one.
    pub api_key_env: Option<String>,
    /// Prefixed onto every collection name, so one Qdrant can host several deployments.
    pub collection_prefix: String,
    /// Points per upsert request.
    pub batch_size: usize,
    /// Whether Qdrant applies inverse document frequency to the lexical half.
    ///
    /// Default true, which is the better ranking. The statistics behind it are the
    /// collection's, and a collection holds every tenant's points, so a term's rarity is
    /// computed across organizations — no content crosses the boundary, but the *ranking*
    /// depends on other organizations' corpora. A deployment where that is unacceptable
    /// sets this false and gets saturated term frequencies alone. It is a property of the
    /// collection, so changing it means a new collection: see `vs-vectors/SPEC.md`.
    pub idf: bool,
}

impl Default for QdrantConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:6334".into(),
            api_key_env: None,
            collection_prefix: "vs".into(),
            batch_size: 256,
            idf: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelsConfig {
    /// Path to the model registry TOML (see config/models.example.toml).
    pub registry: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorkerConfig {
    /// Max concurrent tasks per worker process.
    pub max_in_flight: usize,
    /// Share of in-flight slots reserved for the interactive lane.
    pub interactive_share: f32,
    /// Render DPI for page images.
    pub render_dpi: u32,
    /// Fraction of failed pages tolerated before a document fails.
    pub max_failed_page_fraction: f32,
    /// Where ingest spools originals (the microVM's scratch disk). Default: system temp dir.
    pub scratch_dir: Option<PathBuf>,
    /// Render child processes per ingest worker. Default: available CPUs.
    pub render_processes: Option<usize>,
    /// A page that takes longer fails its document permanently.
    pub page_render_timeout_s: u64,
    /// Documents processed at once. Each holds a render child and a spooled original
    /// through its ingest stage, so this bounds disk and memory; pages inside them
    /// contend for `max_in_flight` either way.
    pub max_documents: usize,
    /// Attempts per page before it is recorded failed. Replaces the queue's
    /// `max_deliver`: a transient failure (a busy model endpoint) is retried in process,
    /// and the page is not retried later by anything else: a resumed *document* re-runs
    /// every page that is not already final. 1 disables retrying.
    pub page_attempts: u32,
    /// Address-space limit per render child.
    pub render_mem_limit_mb: Option<u64>,
    /// How long this process's claim on a document is good for, in seconds.
    ///
    /// Renewed while the document is being worked on, so it only lapses when the process
    /// carrying it stops renewing — which is what a crash looks like from the outside.
    /// The resume sweep claims a job whose lease has lapsed, so this is also the delay
    /// before a crashed document is picked up again. Default 120.
    pub lease_ttl_s: u64,
    /// How often this copy looks for abandoned documents. 0 disables the sweep, which
    /// restores the behaviour of having no recovery at all. Default 60.
    pub resume_interval_s: u64,
    /// Abandoned documents claimed per sweep. Bounded so a copy that comes up after a
    /// fleet-wide outage does not take every orphaned document at once. Default 4.
    pub resume_batch: i64,
    /// Finest DPI a DAG may ask for with `crop_dpi`, which re-renders the page from the
    /// original so a small figure is not described from page-DPI pixels.
    ///
    /// A render is quadratic in DPI — 1200 dpi is nine times the pixels of 400 — so this
    /// is the cap a typo in a DAG runs into rather than a page turning into minutes of
    /// CPU. 0 disables re-rendering entirely, and `crop_dpi` then has no effect beyond a
    /// logged warning. Default 600.
    pub max_crop_dpi: u32,
    /// Times one document may be resumed before the job is failed. Stops a document that
    /// kills the process reading it from taking down the fleet one copy at a time.
    /// Default 3.
    pub max_resumes: i32,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 32,
            interactive_share: 0.3,
            render_dpi: 200,
            max_failed_page_fraction: 0.05,
            scratch_dir: None,
            render_processes: None,
            page_render_timeout_s: 60,
            max_documents: 4,
            page_attempts: 3,
            render_mem_limit_mb: None,
            max_crop_dpi: 600,
            lease_ttl_s: 120,
            resume_interval_s: 60,
            resume_batch: 4,
            max_resumes: 3,
        }
    }
}

/// Models and thresholds of the `page` classifier and the `doc` role. Model names are
/// registry ids (`[models].registry`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Stage2Config {
    /// Extraction LLM.
    pub extraction_model: String,
    /// Verification LLM (may equal `extraction_model`).
    pub verify_model: String,
    /// Text embedder for ontology matching, corpus resolution and graph vectors.
    pub embedding_model: String,
    /// LLM judge for ambiguous corpus-resolution candidates; none = thresholds only.
    pub judge_model: Option<String>,
    /// VLM that classifies a whole document from its opening page thumbnails.
    /// None = the document type is the weighted page vote alone.
    ///
    /// When it is set, the **ingest** stage calls it once per document, before any page
    /// is routed, and every page is then classified knowing what document it belongs to.
    /// That is the cross-page smoothing the page stage cannot do for itself: pages fan
    /// out with no shared state. The same classifier is used again in Stage 2, where it
    /// is one of the inputs to the final document type.
    pub doc_classifier_model: Option<String>,
    /// Opening pages shown to the document classifier. Three is enough to tell an invoice
    /// from a paper, and the call is per document rather than per page.
    pub doc_classifier_pages: usize,
    /// Tier 1 page classifier (VLM). None = rules-only classification.
    pub classifier_model: Option<String>,
    /// Characters per extraction window.
    pub window_chars: usize,
    /// Cosine threshold for mapping raw types onto the ontology.
    pub type_match_threshold: f32,
    /// Corpus resolution: score at or above which an entity links without a judge.
    pub link_threshold: f32,
    /// Corpus resolution: score band in which the judge decides.
    pub judge_band: (f32, f32),
    /// Texts per embedding request (0 = registry default).
    pub embed_batch_size: usize,
    /// How long a worker trusts its cached "active DAG list" mapping.
    pub dag_list_refresh_s: u64,
}

impl Default for Stage2Config {
    fn default() -> Self {
        Self {
            extraction_model: "extract-llm".into(),
            verify_model: "extract-llm".into(),
            embedding_model: "embed-text".into(),
            judge_model: None,
            doc_classifier_model: None,
            doc_classifier_pages: 3,
            classifier_model: None,
            window_chars: 12_000,
            type_match_threshold: 0.85,
            link_threshold: 0.92,
            judge_band: (0.80, 0.92),
            embed_batch_size: 0,
            dag_list_refresh_s: 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Multipart upload size limit (413 above it). Default 2 GiB.
    pub max_upload_bytes: u64,
    /// Interactive requests get 429 while the interactive ingest lane has more pending
    /// messages than this.
    pub interactive_max_pending: u64,
    /// Where multipart uploads are spooled while hashing. Default: system temp dir.
    pub scratch_dir: Option<PathBuf>,
    /// How wide a newly uploaded document is open inside its organization:
    /// `private` | `department` | `workspace` | `organization`.
    ///
    /// Default `private`, which is the identity model's answer — access is the person's
    /// (D14) — and the strict one: a deployment that wants what the API did before per-
    /// person scoping existed, where every member of an organization reads every
    /// document it owns, sets `organization` and gets exactly that.
    pub default_visibility: String,
    /// Whether a caller holding `auth.admin_role` reads every document in its
    /// organization. Default true: the role already carries routing and metrics, and an
    /// operator who cannot read a document cannot answer a question about it. Set false
    /// in a deployment where administration and access are deliberately separate.
    pub admin_reads_all: bool,
    /// Requests per second one organization may make, token-bucket, per process. 0
    /// disables the limit. Default 50.
    ///
    /// Per process rather than per deployment because there is nothing between the
    /// copies to share a counter — the same reason admission control is per copy (D13).
    /// A fleet of N copies therefore allows N × this in the worst case, which is the
    /// honest number to size against.
    pub tenant_rate_per_s: f64,
    /// Requests one organization may make at once before the rate applies. Default 100.
    pub tenant_rate_burst: f64,
    /// Days after a job finishes before the caller's subject is scrubbed from it.
    /// 0 disables the sweep (and leaves the data, which is a decision worth making
    /// deliberately). Default 90.
    pub job_retention_days: u64,
    /// How often the revocation list is re-read from Postgres. Default 30 s.
    ///
    /// This is the window in which a revoked token still verifies on a copy that has not
    /// refreshed yet. It is short because the list is small, and the alternative — a
    /// query per request — would put Postgres on the path of every call, which is what
    /// verifying signatures locally exists to avoid (D10).
    pub revocation_refresh_s: u64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            max_upload_bytes: 2 * 1024 * 1024 * 1024,
            interactive_max_pending: 1000,
            scratch_dir: None,
            default_visibility: "private".into(),
            admin_reads_all: true,
            tenant_rate_per_s: 50.0,
            tenant_rate_burst: 100.0,
            job_retention_days: 90,
            revocation_refresh_s: 30,
        }
    }
}

/// Keycloak (OIDC) token verification for `vs-api`.
///
/// `issuer` and `audience` have no defaults: an audience that defaulted to "accept
/// anything" would make every other check here decorative, since Keycloak hands the same
/// realm's tokens to every client in it. The audience needs an audience mapper on the
/// `vs-api` client in Keycloak, which is what puts `vs-api` in the token's `aud`.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// Realm issuer URL, matched against the token's `iss` exactly, and the base for
    /// OIDC discovery (`<issuer>/.well-known/openid-configuration`).
    pub issuer: String,
    /// Accepted `aud` values. Must be non-empty.
    ///
    /// Accepts a list (`["vs-api"]`) or one string, because an environment override
    /// carries only scalars: a deployment configured entirely through `VS__*` has no way
    /// to write a TOML array, and `VS__AUTH__AUDIENCE=vs-api,other` is what it would
    /// reach for. Separated by commas or whitespace.
    #[serde(deserialize_with = "audience")]
    pub audience: Vec<String>,
    /// Keycloak client whose `resource_access.<client_id>.roles` are read in addition to
    /// the realm roles. `None`: realm roles only.
    #[serde(default)]
    pub client_id: Option<String>,
    /// Claim holding the organization that becomes the `TenantId` — the isolation
    /// boundary. Dotted paths index nested objects (`koki.org`).
    #[serde(default = "org_claim")]
    pub org_claim: String,
    /// Claim holding the caller's department. Recorded on the job, and what
    /// `visibility = department` is evaluated against.
    #[serde(default = "department_claim")]
    pub department_claim: String,
    /// Claim holding the caller's workspaces: an array of strings, or one
    /// space-separated string. Recorded on the job, and what `visibility = workspace` is
    /// evaluated against — snapshotted at upload, so a share survives the uploader
    /// leaving the workspace.
    #[serde(default = "workspaces_claim")]
    pub workspaces_claim: String,
    /// Realm or client role required by the admin routes and `/metrics`.
    #[serde(default = "admin_role")]
    pub admin_role: String,
    /// How long a fetched JWKS is served from cache. Default 300 s.
    #[serde(default = "jwks_refresh_s")]
    pub jwks_refresh_s: u64,
    /// Clock-skew allowance on `exp` and `nbf`, in seconds. Default 30.
    #[serde(default = "leeway_s")]
    pub leeway_s: u64,
}

/// A TOML array, or one comma/whitespace-separated string.
fn audience<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::Many(v) => v,
        OneOrMany::One(s) => s.split([',', ' ', '\t']).map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect(),
    })
}

fn org_claim() -> String {
    "organization".into()
}
fn department_claim() -> String {
    "department".into()
}
fn workspaces_claim() -> String {
    "workspaces".into()
}
fn admin_role() -> String {
    "vs-admin".into()
}
fn jwks_refresh_s() -> u64 {
    300
}
fn leeway_s() -> u64 {
    30
}

impl AuthConfig {
    /// Rejects a section that would weaken verification rather than failing loudly.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.issuer.trim().is_empty() {
            anyhow::bail!("auth.issuer must be set");
        }
        if !self.issuer.starts_with("https://") && !self.issuer.starts_with("http://") {
            anyhow::bail!("auth.issuer must be an absolute http(s) URL");
        }
        if self.audience.iter().all(|a| a.trim().is_empty()) {
            anyhow::bail!("auth.audience must list at least one non-empty audience");
        }
        if self.admin_role.trim().is_empty() {
            anyhow::bail!("auth.admin_role must be set");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    pub otlp_endpoint: Option<String>,
    pub metrics_addr: Option<String>,
    pub json_logs: bool,
    /// Fraction of traces exported, 0.0..=1.0. Default 1.0 — every trace, which is what
    /// this did before there was a choice.
    ///
    /// The sampler is parent-based: a span whose parent was sampled is sampled, so a
    /// trace is never exported in pieces, and the ratio applies to traces that start
    /// here. One span per stage and per DAG node is affordable at 1.0; the model
    /// client's per-attempt span at volume is what this exists for.
    pub sample_ratio: f64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self { otlp_endpoint: None, metrics_addr: None, json_logs: false, sample_ratio: 1.0 }
    }
}

impl Config {
    /// Loads TOML, then applies `VS__SECTION__KEY` environment overrides.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut value: toml::Value = toml::from_str(&text)?;
        apply_env_overrides(&mut value, std::env::vars());
        Ok(value.try_into()?)
    }
}

fn apply_env_overrides(value: &mut toml::Value, vars: impl Iterator<Item = (String, String)>) {
    'vars: for (k, v) in vars {
        let Some(rest) = k.strip_prefix("VS__") else {
            continue;
        };
        let path: Vec<String> = rest.split("__").map(|s| s.to_ascii_lowercase()).collect();
        let Some((leaf, parents)) = path.split_last() else {
            continue;
        };
        let mut cur = &mut *value;
        for seg in parents {
            // Match on the reference by value so the borrow moves into `cur`.
            cur = match cur {
                toml::Value::Table(t) => t.entry(seg.clone()).or_insert_with(|| toml::Value::Table(Default::default())),
                _ => continue 'vars, // a scalar is in the way; ignore the override
            };
        }
        if let toml::Value::Table(t) = cur {
            t.insert(leaf.clone(), parse_scalar(&v));
        }
    }
}

fn parse_scalar(v: &str) -> toml::Value {
    v.parse::<i64>()
        .map(toml::Value::Integer)
        .or_else(|_| v.parse::<f64>().map(toml::Value::Float))
        .or_else(|_| v.parse::<bool>().map(toml::Value::Boolean))
        .unwrap_or_else(|_| toml::Value::String(v.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_sets_nested_key() {
        let mut v: toml::Value = toml::from_str("[worker]\nmax_in_flight = 4\n").unwrap();
        apply_env_overrides(&mut v, vec![("VS__WORKER__MAX_IN_FLIGHT".into(), "64".into())].into_iter());
        assert_eq!(v["worker"]["max_in_flight"].as_integer(), Some(64));
    }

    #[test]
    fn the_audience_can_arrive_as_a_list_or_as_one_overridable_string() {
        let list: AuthConfig = toml::from_str("issuer = \"https://sso/realms/r\"\naudience = [\"vs-api\", \"other\"]\n").expect("list");
        assert_eq!(list.audience, vec!["vs-api", "other"]);
        assert!(list.validate().is_ok());

        // The shape an environment override produces, since `parse_scalar` only ever
        // makes scalars: without this the whole config would fail to load.
        let mut v: toml::Value = toml::from_str("[auth]\nissuer = \"https://sso/realms/r\"\naudience = [\"from-toml\"]\n").expect("toml");
        apply_env_overrides(&mut v, vec![("VS__AUTH__AUDIENCE".into(), "vs-api, other".into())].into_iter());
        let cfg: AuthConfig = v["auth"].clone().try_into().expect("override");
        assert_eq!(cfg.audience, vec!["vs-api", "other"]);

        let blank: AuthConfig = toml::from_str("issuer = \"https://sso/realms/r\"\naudience = \"  \"\n").expect("blank");
        assert!(blank.audience.is_empty());
        assert!(blank.validate().is_err(), "an audience that accepts anything must not load");
    }

    #[test]
    fn example_configs_parse() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        for name in ["worker.example.toml", "worker.dev.toml"] {
            let cfg = Config::load(&root.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(cfg.blob.bucket, "visual-suite");
            assert!(cfg.classify.is_table());
            assert_eq!(cfg.api.interactive_max_pending, 1000);
            assert_eq!(cfg.stage2.embedding_model, "embed-text");
            assert_eq!(cfg.stage2.dag_list_refresh_s, 60);
        }
    }

    #[test]
    fn stage2_defaults_when_section_missing() {
        let v: toml::Value = toml::from_str("x = 1\n").unwrap();
        let s2: Stage2Config = v.get("stage2").cloned().map(|v| v.try_into().unwrap()).unwrap_or_default();
        assert_eq!(s2.extraction_model, "extract-llm");
        assert!(s2.classifier_model.is_none());
        let s2: Stage2Config = toml::from_str("classifier_model = \"page-classifier\"\njudge_band = [0.7, 0.9]\n").unwrap();
        assert_eq!(s2.classifier_model.as_deref(), Some("page-classifier"));
        assert_eq!(s2.judge_band, (0.7, 0.9));
    }

    #[test]
    fn override_through_scalar_is_ignored() {
        let mut v: toml::Value = toml::from_str("worker = 1\n").unwrap();
        apply_env_overrides(&mut v, vec![("VS__WORKER__X".into(), "2".into())].into_iter());
        assert_eq!(v["worker"].as_integer(), Some(1));
    }
}
