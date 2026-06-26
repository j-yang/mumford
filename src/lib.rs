//! Mumford: format-aware diff engines for common document types.
//!
//! Built on top of [`tate`] (the algorithm layer), mumford adds file-format
//! parsing and dispatching so applications can diff real documents without
//! writing their own parsers.
//!
//! ## Supported formats
//!
//! - **Plain text** — line-level diff with word-level inline highlights
//! - **PDF** — text extraction via pdfium, running-header stripping, line diff
//! - **Word (.docx)** — OOXML paragraph + table extraction, paragraph diff
//! - **Excel (.xlsx/.xls/.xlsm)** — cell-level grid alignment via `tate::grid`
//! - **RTF** — styled-table parsing, row/cell diff with formatting preserved
//! - **Folders** — recursive comparison with sha256 hashing and rename detection
//!
//! ## Architecture
//!
//! `tate` (algorithms: lines, inline, grid, tree) is the foundation.
//! `mumford` (format engines: parse → feed to tate → wrap result) sits on top.
//! Your app (domain adapters + UI) sits above mumford.
//!
//! Each format engine:
//! 1. Parses the file into a Rust data structure (lines, grids, paragraphs…)
//! 2. Feeds the parsed data to the appropriate tate algorithm
//! 3. Wraps the result in a format-specific output type with paths and metadata
//!
//! The dispatch function ([`dispatch`]) routes by file extension to the right
//! engine, returning a unified [`DiffResult`].

pub mod text;
pub mod pdf;
pub mod docx;
pub mod excel;
pub mod rtf;
pub mod folder;
#[cfg(feature = "json")]
pub mod json;

use std::path::Path;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Lowercase hex encoding, shared by the folder hasher.
pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// The unified diff result that applications consume. Exactly one of the
/// format-specific fields is set, depending on which engine `dispatch` routed to.
#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DiffResult {
    #[cfg_attr(feature = "serde", serde(rename = "fileType"))]
    pub file_type: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathA"))]
    pub path_a: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathB"))]
    pub path_b: String,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub text: Option<text::TextResult>,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub excel: Option<excel::ExcelResult>,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub docx: Option<docx::DocxResult>,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub rtf: Option<rtf::RtfResult>,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "String::is_empty"))]
    pub error: String,
}

/// Dispatch routes a file pair to the right diff engine by extension.
/// An empty path on one side means the file is absent there (added/removed).
pub fn dispatch(path_a: &str, path_b: &str) -> Result<DiffResult, String> {
    let ext = ext_of(path_a).or_else(|| ext_of(path_b)).unwrap_or_default();

    let mut res = DiffResult {
        path_a: path_a.into(),
        path_b: path_b.into(),
        ..Default::default()
    };

    match ext.as_str() {
        "xlsx" | "xls" | "xlsm" => {
            let r = excel::excel_diff(path_a, path_b)?;
            res.file_type = "excel".into();
            res.excel = Some(r);
        }
        "docx" => {
            let r = docx::docx_diff(path_a, path_b)?;
            res.file_type = "docx".into();
            res.docx = Some(r);
        }
        "rtf" => {
            let r = rtf::rtf_diff(path_a, path_b)?;
            res.file_type = "rtf".into();
            res.rtf = Some(r);
        }
        "pdf" => {
            let r = pdf::pdf_diff(path_a, path_b)?;
            res.file_type = "text".into();
            res.text = Some(r);
        }
        _ => {
            let r = text::text_diff(path_a, path_b).map_err(|e| e.to_string())?;
            res.file_type = "text".into();
            res.text = Some(r);
        }
    }

    Ok(res)
}

fn ext_of(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .filter(|e| !e.is_empty())
}