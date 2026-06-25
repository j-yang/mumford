//! Word (.docx) diff: extract paragraphs and tables from OOXML via zip +
//! quick-xml, then diff paragraphs via [`tate::lines::diff`].
//!
//! TODO: port from shtuka-core/src/docx.rs.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DocxResult {
    #[cfg_attr(feature = "serde", serde(rename = "fileType"))]
    pub file_type: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathA"))]
    pub path_a: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathB"))]
    pub path_b: String,
}

pub fn docx_diff(path_a: &str, path_b: &str) -> Result<DocxResult, String> {
    let _ = (path_a, path_b);
    todo!("port docx engine from shtuka-core/src/docx.rs")
}