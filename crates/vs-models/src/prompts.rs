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
