//! RTF diff: parse RTF into a styled table model (cells with text + shading /
//! borders / font / alignment), align rows via LCS on text signatures, tag
//! each cell added/removed/modified/equal.
//!
//! TODO: port from shtuka-core/src/rtf.rs.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct RtfResult {
    #[cfg_attr(feature = "serde", serde(rename = "fileType"))]
    pub file_type: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathA"))]
    pub path_a: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathB"))]
    pub path_b: String,
}

pub fn rtf_diff(path_a: &str, path_b: &str) -> Result<RtfResult, String> {
    let _ = (path_a, path_b);
    todo!("port rtf engine from shtuka-core/src/rtf.rs")
}