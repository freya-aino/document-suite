//! Stage 1 workflow DAGs. See `SPEC.md`.

pub mod artifact;
pub mod dag;
pub mod exec;
pub mod step;

pub use artifact::{Artifact, ArtifactKind, BlockDraft, FigureCrop, LayoutRegion, PageImage, TextLayer, TextSpan};
pub use dag::{Dag, DagError, Node, Signature, StepSignatures, SOURCE_PAGE_IMAGE, SOURCE_TEXT_LAYER};
pub use exec::{ExecError, ExecOutput, Executor, NodeOutcome, NodeTrace};
pub use step::{Step, StepContext, StepError, StepOutput, StepRegistry};
