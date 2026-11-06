//! Output parsers: one per model family. See SPEC.md "Output parsers".

use vs_core::BlockKind;
use vs_dag::{BlockDraft, PageImage};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("{family}: {reason}")]
    Invalid { family: &'static str, reason: String },
    #[error("no parser for family {0:?}")]
    UnknownFamily(String),
}

pub trait OutputParser: Send + Sync {
    fn family(&self) -> &'static str;
    /// `model_input_size` = (w, h) of the image as the model saw it (after its resizing).
    fn parse(&self, raw: &str, page: &PageImage, model_input_size: (u32, u32), node_id: &str) -> Result<Vec<BlockDraft>, ParseError>;
}

pub fn parser_for(family: &str) -> Result<Box<dyn OutputParser>, ParseError> {
    match family {
        "paddleocr_vl" => Ok(Box::new(Unimplemented("paddleocr_vl"))),
        "mineru" => Ok(Box::new(Unimplemented("mineru"))),
        "dots_ocr" => Ok(Box::new(Unimplemented("dots_ocr"))),
        "logics_parsing" => Ok(Box::new(Unimplemented("logics_parsing"))),
        "generic_json" => Ok(Box::new(Unimplemented("generic_json"))),
        other => Err(ParseError::UnknownFamily(other.to_string())),
    }
}

/// Placeholder until each family's format is implemented against captured fixtures.
struct Unimplemented(&'static str);

impl OutputParser for Unimplemented {
    fn family(&self) -> &'static str {
        self.0
    }
    fn parse(&self, _raw: &str, _page: &PageImage, _size: (u32, u32), _node_id: &str) -> Result<Vec<BlockDraft>, ParseError> {
        Err(ParseError::Invalid { family: self.0, reason: "parser not implemented yet".into() })
    }
}

/// Shared label mapping; families map their labels to these names first.
pub fn block_kind_from_label(label: &str) -> BlockKind {
    match label.to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
        "title" | "doc_title" => BlockKind::Title,
        "heading" | "section_header" | "paragraph_title" | "header_1" | "header_2" => BlockKind::Heading,
        "text" | "paragraph" | "plain_text" | "abstract" => BlockKind::Paragraph,
        "list" | "list_item" => BlockKind::List,
        "table" => BlockKind::Table,
        "figure" | "image" | "picture" => BlockKind::Figure,
        "chart" | "plot" => BlockKind::Chart,
        "formula" | "equation" | "display_formula" | "isolate_formula" => BlockKind::Formula,
        "caption" | "figure_caption" | "table_caption" => BlockKind::Caption,
        "footnote" => BlockKind::Footnote,
        "header" | "page_header" => BlockKind::Header,
        "footer" | "page_footer" => BlockKind::Footer,
        "page_number" => BlockKind::PageNumber,
        "reference" | "references" | "bibliography" => BlockKind::Reference,
        "handwriting" | "handwritten" => BlockKind::Handwriting,
        "seal" | "stamp" => BlockKind::Stamp,
        "code" | "algorithm" => BlockKind::Code,
        _ => BlockKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_mapping() {
        assert_eq!(block_kind_from_label("Section-Header"), BlockKind::Heading);
        assert_eq!(block_kind_from_label("seal"), BlockKind::Stamp);
        assert_eq!(block_kind_from_label("weird"), BlockKind::Other);
    }
}
