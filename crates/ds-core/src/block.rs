use serde::{Deserialize, Serialize};

use crate::{BBox, HashKey, PageProfile, Provenance, RunId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Title,
    Heading,
    Paragraph,
    List,
    Table,
    Figure,
    Chart,
    Formula,
    Caption,
    Footnote,
    Header,
    Footer,
    PageNumber,
    Reference,
    Handwriting,
    Stamp,
    Code,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockContent {
    Text { text: String },
    /// Table as HTML (`<table>` with rowspan/colspan); the canonical table form.
    Table { html: String },
    Formula { latex: String },
    Figure { crop_uri: String, description: Option<String> },
    Chart { crop_uri: String, data_html: Option<String>, description: Option<String> },
    Empty,
}

/// One layout region of a page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub block_hk: HashKey,
    pub index: u32,
    pub kind: BlockKind,
    pub bbox: BBox,
    /// Reading order on the page (0-based).
    pub reading_order: u32,
    pub content: BlockContent,
    /// Plain-text projection of `content`; evidence character spans refer to this.
    pub text: String,
    /// Heading level for `Heading` blocks (1 = top).
    pub level: Option<u8>,
    pub confidence: f32,
    pub provenance: Provenance,
}

/// All blocks of one page produced by one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageResult {
    pub schema_version: u32,
    pub document_hk: HashKey,
    pub page_hk: HashKey,
    pub page_no: u32,
    pub run_id: RunId,
    pub render_width_px: u32,
    pub render_height_px: u32,
    pub profile: PageProfile,
    /// `dag_id@version` that produced this page.
    pub dag_ref: String,
    pub blocks: Vec<Block>,
}

impl PageResult {
    pub const SCHEMA_VERSION: u32 = 1;
}
