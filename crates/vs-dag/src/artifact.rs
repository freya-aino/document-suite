use serde::{Deserialize, Serialize};
use std::sync::Arc;

use vs_core::{BBox, BlockContent, BlockKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    PageImage,
    TextLayer,
    Layout,
    Blocks,
    FigureCrops,
}

#[derive(Debug, Clone)]
pub struct PageImage {
    pub uri: String,
    /// Encoded image bytes (PNG/JPEG).
    pub bytes: Arc<[u8]>,
    pub width_px: u32,
    pub height_px: u32,
    pub dpi: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextSpan {
    pub text: String,
    pub bbox: BBox,
    pub font_size: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TextLayer {
    pub spans: Vec<TextSpan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutRegion {
    pub kind: BlockKind,
    pub bbox: BBox,
    pub reading_order: u32,
    pub score: f32,
}

/// A block before hash keys and provenance are attached by the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDraft {
    pub kind: BlockKind,
    pub bbox: BBox,
    pub reading_order: u32,
    pub content: BlockContent,
    pub text: String,
    pub level: Option<u8>,
    pub confidence: f32,
    /// Node that produced this draft (for provenance lookup).
    pub node_id: String,
}

#[derive(Debug, Clone)]
pub struct FigureCrop {
    pub region: LayoutRegion,
    pub uri: String,
    pub bytes: Arc<[u8]>,
}

#[derive(Debug, Clone)]
pub enum Artifact {
    PageImage(PageImage),
    TextLayer(TextLayer),
    Layout(Vec<LayoutRegion>),
    Blocks(Vec<BlockDraft>),
    FigureCrops(Vec<FigureCrop>),
}

impl Artifact {
    pub fn kind(&self) -> ArtifactKind {
        match self {
            Artifact::PageImage(_) => ArtifactKind::PageImage,
            Artifact::TextLayer(_) => ArtifactKind::TextLayer,
            Artifact::Layout(_) => ArtifactKind::Layout,
            Artifact::Blocks(_) => ArtifactKind::Blocks,
            Artifact::FigureCrops(_) => ArtifactKind::FigureCrops,
        }
    }

    /// Empty artefact of a kind; used for failed optional nodes.
    /// `PageImage` has no empty form and returns `None`.
    pub fn empty(kind: ArtifactKind) -> Option<Artifact> {
        match kind {
            ArtifactKind::PageImage => None,
            ArtifactKind::TextLayer => Some(Artifact::TextLayer(TextLayer::default())),
            ArtifactKind::Layout => Some(Artifact::Layout(vec![])),
            ArtifactKind::Blocks => Some(Artifact::Blocks(vec![])),
            ArtifactKind::FigureCrops => Some(Artifact::FigureCrops(vec![])),
        }
    }
}
