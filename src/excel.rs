//! Excel (.xlsx/.xls/.xlsm) diff: read workbooks via calamine, align rows and
//! columns via [`tate::grid::grid_diff`].
//!
//! TODO: port from shtuka-core/src/excel.rs.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use tate::grid::{GridDiff, GridOptions};

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SheetDiff {
    pub name: String,
    pub status: String,
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub grid: GridDiff,
}

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ExcelResult {
    #[cfg_attr(feature = "serde", serde(rename = "fileType"))]
    pub file_type: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathA"))]
    pub path_a: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathB"))]
    pub path_b: String,
    pub sheets: Vec<SheetDiff>,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
    pub notes: Vec<String>,
}

pub fn excel_diff(path_a: &str, path_b: &str) -> Result<ExcelResult, String> {
    let _ = (path_a, path_b, GridOptions::default());
    todo!("port excel engine from shtuka-core/src/excel.rs")
}