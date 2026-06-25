//! Folder comparison with sha256 content hashing and rename detection.
//!
//! TODO: port from shtuka-core/src/folder.rs.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Comparison {
    #[cfg_attr(feature = "serde", serde(rename = "pathA"))]
    pub path_a: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathB"))]
    pub path_b: String,
}

pub fn compare(path_a: &str, path_b: &str) -> std::io::Result<Comparison> {
    let _ = (path_a, path_b);
    todo!("port folder engine from shtuka-core/src/folder.rs")
}