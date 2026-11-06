//! Compiled-in prompts. `prompt_ref = <id>@<sha256 prefix>`.

use sha2::{Digest, Sha256};

pub struct Prompt {
    pub id: &'static str,
    pub text: &'static str,
}

impl Prompt {
    pub fn reference(&self) -> String {
        format!("{}@{}", self.id, &hex::encode(Sha256::digest(self.text.as_bytes()))[..8])
    }

    pub fn render(&self, vars: &[(&str, &str)]) -> String {
        let mut s = self.text.to_string();
        for (k, v) in vars {
            s = s.replace(&format!("{{{k}}}"), v);
        }
        s
    }
}

pub const PAGE_CLASSIFY: Prompt = Prompt { id: "page_classify", text: include_str!("../prompts/page_classify.md") };
pub const FIGURE_DESCRIBE: Prompt = Prompt { id: "figure_describe", text: include_str!("../prompts/figure_describe.md") };
pub const CHART_TO_TABLE: Prompt = Prompt { id: "chart_to_table", text: include_str!("../prompts/chart_to_table.md") };
pub const PAGE_PARSE: Prompt = Prompt { id: "page_parse", text: include_str!("../prompts/page_parse.md") };
pub const LAYOUT_DETECT: Prompt = Prompt { id: "layout_detect", text: include_str!("../prompts/layout_detect.md") };
pub const REGION_OCR: Prompt = Prompt { id: "region_ocr", text: include_str!("../prompts/region_ocr.md") };
pub const TABLE_RECOGNIZE: Prompt = Prompt { id: "table_recognize", text: include_str!("../prompts/table_recognize.md") };
pub const FORMULA_RECOGNIZE: Prompt = Prompt { id: "formula_recognize", text: include_str!("../prompts/formula_recognize.md") };

// Literal task prompts the document-parsing VLMs are trained on. Sent verbatim (no variables).
pub const PADDLEOCR_VL_OCR: Prompt = Prompt { id: "paddleocr_vl_ocr", text: "OCR:" };
pub const PADDLEOCR_VL_TABLE: Prompt = Prompt { id: "paddleocr_vl_table", text: "Table Recognition:" };
pub const PADDLEOCR_VL_FORMULA: Prompt = Prompt { id: "paddleocr_vl_formula", text: "Formula Recognition:" };
pub const PADDLEOCR_VL_CHART: Prompt = Prompt { id: "paddleocr_vl_chart", text: "Chart Recognition:" };
pub const MINERU_LAYOUT: Prompt = Prompt { id: "mineru_layout", text: "\nLayout Detection:" };
pub const MINERU_TEXT: Prompt = Prompt { id: "mineru_text", text: "\nText Recognition:" };
pub const MINERU_TABLE: Prompt = Prompt { id: "mineru_table", text: "\nTable Recognition:" };
pub const MINERU_FORMULA: Prompt = Prompt { id: "mineru_formula", text: "\nFormula Recognition:" };
pub const DOTS_OCR_LAYOUT_ALL: Prompt = Prompt {
    id: "dots_ocr_layout_all",
    text: "Please output the layout information from the PDF image, including each layout element's bbox, its category, and the corresponding text content within the bbox.\n\n1. Bbox format: [x1, y1, x2, y2]\n\n2. Layout Categories: The possible categories are ['Caption', 'Footnote', 'Formula', 'List-item', 'Page-footer', 'Page-header', 'Picture', 'Section-header', 'Table', 'Text', 'Title'].\n\n3. Text Extraction & Formatting Rules:\n    - Picture: For the 'Picture' category, the text field should be omitted.\n    - Formula: Format its text as LaTeX.\n    - Table: Format its text as HTML.\n    - All Others (Text, Title, etc.): Format their text as Markdown.\n\n4. Constraints:\n    - The output text must be the original text from the image, with no translation.\n    - All layout elements must be sorted according to human reading order.\n\n5. Final Output: The entire output must be a single JSON object.",
};
pub const DOTS_OCR_LAYOUT_ONLY: Prompt = Prompt {
    id: "dots_ocr_layout_only",
    text: "Please output the layout information from this PDF image, including each layout's bbox and its category. The bbox should be in the format [x1, y1, x2, y2]. The layout categories for the PDF document include ['Caption', 'Footnote', 'Formula', 'List-item', 'Page-footer', 'Page-header', 'Picture', 'Section-header', 'Table', 'Text', 'Title']. Do not output the corresponding text. The layout result should be in JSON format.",
};
pub const DOTS_OCR_TEXT: Prompt = Prompt { id: "dots_ocr_text", text: "Extract the text content from this image." };

/// Every compiled-in prompt (for `params.prompt` lookups by id).
pub const ALL: &[&Prompt] = &[
    &PAGE_CLASSIFY, &FIGURE_DESCRIBE, &CHART_TO_TABLE, &PAGE_PARSE, &LAYOUT_DETECT, &REGION_OCR, &TABLE_RECOGNIZE,
    &FORMULA_RECOGNIZE, &PADDLEOCR_VL_OCR, &PADDLEOCR_VL_TABLE, &PADDLEOCR_VL_FORMULA, &PADDLEOCR_VL_CHART, &MINERU_LAYOUT,
    &MINERU_TEXT, &MINERU_TABLE, &MINERU_FORMULA, &DOTS_OCR_LAYOUT_ALL, &DOTS_OCR_LAYOUT_ONLY, &DOTS_OCR_TEXT,
];

pub fn by_id(id: &str) -> Option<&'static Prompt> {
    ALL.iter().copied().find(|p| p.id == id)
}

/// Default task prompt for a family and task (`page_parse`, `layout`, `ocr`, `table`, `formula`,
/// `chart`). `None` means the generic prompt file applies.
pub fn family_task_prompt(family: &str, task: &str) -> Option<&'static Prompt> {
    Some(match (family, task) {
        ("paddleocr_vl", "ocr") | ("paddleocr_vl", "page_parse") => &PADDLEOCR_VL_OCR,
        ("paddleocr_vl", "table") => &PADDLEOCR_VL_TABLE,
        ("paddleocr_vl", "formula") => &PADDLEOCR_VL_FORMULA,
        ("paddleocr_vl", "chart") => &PADDLEOCR_VL_CHART,
        ("mineru", "layout") | ("mineru", "page_parse") => &MINERU_LAYOUT,
        ("mineru", "ocr") => &MINERU_TEXT,
        ("mineru", "table") => &MINERU_TABLE,
        ("mineru", "formula") => &MINERU_FORMULA,
        ("dots_ocr", "page_parse") => &DOTS_OCR_LAYOUT_ALL,
        ("dots_ocr", "layout") => &DOTS_OCR_LAYOUT_ONLY,
        ("dots_ocr", "ocr") | ("dots_ocr", "table") | ("dots_ocr", "formula") => &DOTS_OCR_TEXT,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_is_id_at_hash_prefix() {
        let r = PAGE_CLASSIFY.reference();
        let (id, hash) = r.split_once('@').unwrap();
        assert_eq!(id, "page_classify");
        assert_eq!(hash.len(), 8);
    }

    #[test]
    fn render_replaces_variables() {
        let p = Prompt { id: "t", text: "lang={language} x={x}" };
        assert_eq!(p.render(&[("language", "de"), ("x", "1")]), "lang=de x=1");
    }

    #[test]
    fn ids_are_unique_and_lookup_works() {
        let mut ids: Vec<&str> = ALL.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ALL.len());
        assert_eq!(by_id("table_recognize").unwrap().id, "table_recognize");
        assert!(by_id("nope").is_none());
    }

    #[test]
    fn family_prompts() {
        assert_eq!(family_task_prompt("paddleocr_vl", "table").unwrap().text, "Table Recognition:");
        assert!(family_task_prompt("generic_json", "ocr").is_none());
    }
}
