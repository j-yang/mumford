//! Plain-text line diff. Uses [`tate::lines::diff`] for the edit script and
//! [`tate::inline::pair_replacements`] for word-level Replace rows.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use tate::inline::{Op, OpType, DEFAULT_SIMILARITY};

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct TextSummary {
    pub equal: usize,
    pub insert: usize,
    pub delete: usize,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct TextResult {
    #[cfg_attr(feature = "serde", serde(rename = "fileType"))]
    pub file_type: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathA"))]
    pub path_a: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathB"))]
    pub path_b: String,
    pub ops: Vec<Op>,
    pub summary: TextSummary,
}

pub fn text_diff(path_a: &str, path_b: &str) -> std::io::Result<TextResult> {
    let a = read_lines(path_a)?;
    let b = read_lines(path_b)?;
    Ok(build_text_result(path_a, path_b, a, b))
}

pub fn build_text_result(path_a: &str, path_b: &str, a: Vec<String>, b: Vec<String>) -> TextResult {
    let raw = tate::lines::diff(&a, &b);
    let ops = tate::inline::pair_replacements(raw, DEFAULT_SIMILARITY);
    let mut summary = TextSummary::default();
    for op in &ops {
        match op.typ {
            OpType::Equal => summary.equal += 1,
            OpType::Insert => summary.insert += 1,
            OpType::Delete => summary.delete += 1,
            OpType::Replace => {
                summary.delete += 1;
                summary.insert += 1;
            }
        }
    }
    TextResult {
        file_type: "text".into(),
        path_a: path_a.into(),
        path_b: path_b.into(),
        ops,
        summary,
    }
}

pub fn read_lines(path: &str) -> std::io::Result<Vec<String>> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let normalized = text.replace("\r\n", "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(|s| s.to_string()).collect();
    if lines.last().map(|s| s.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_text_diff() {
        let a = vec!["hello".into(), "world".into()];
        let b = vec!["hello".into(), "rust".into()];
        let r = build_text_result("a", "b", a, b);
        assert!(r.summary.delete >= 1 || r.summary.insert >= 1);
    }

    #[test]
    fn identical_text_no_changes() {
        let a = vec!["line1".into(), "line2".into()];
        let r = build_text_result("a", "a", a.clone(), a);
        assert_eq!(r.summary.equal, 2);
        assert_eq!(r.summary.insert, 0);
        assert_eq!(r.summary.delete, 0);
    }
}