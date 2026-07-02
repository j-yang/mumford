//! Mumford: format-aware diff engines for common document types.
//!
//! Built on top of [`tate`] (the pure tree algebra), mumford owns everything
//! format- and alignment-related: the diff/keying engines and the file-format
//! parsers, so applications can diff real documents without writing their own.
//!
//! ## Supported formats
//!
//! - **Plain text** — line-level diff with word-level inline highlights
//! - **PDF** — text extraction via pdfium, running-header stripping, line diff
//! - **Word (.docx)** — OOXML paragraph + table extraction, paragraph diff
//! - **Excel (.xlsx/.xls/.xlsm)** — cell-level grid alignment via `mumford::grid`
//! - **PowerPoint (.pptx)** — slide text extraction and alignment, line diff
//! - **JSON** (optional `json` feature) — structural tree diff via `tate::tree`
//! - **Folders** — recursive comparison with sha256 hashing and rename detection
//!
//! ## Architecture
//!
//! `tate` is the pure tree algebra (the `Section` object, `tree_diff` /
//! `tree_merge`, and the `patch` algebra) — it knows nothing about formats.
//! `mumford` sits on top and owns everything format- and alignment-related: the
//! diff/keying engines (`lines`, `inline`, `grid`, `unified`) that turn un-keyed
//! sequence and grid data into aligned, keyable form, plus the format engines
//! (`text`, `pdf`, `docx`, `excel`, `pptx`, `json`, `folder`). Your app (domain
//! adapters + UI) sits above mumford.

// Diff/alignment engines and keying adapters (formerly in tate; moved here so
// tate stays a pure tree-algebra crate). These are the algorithms that turn
// un-keyed sequence/grid data into an aligned, keyable form.
pub mod lines;
pub mod inline;
pub mod grid;
pub mod unified;
mod lcs;

// Format engines.
pub mod text;
pub mod pdf;
pub mod docx;
pub mod excel;
pub mod pptx;
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
    pub pptx: Option<pptx::PptxResult>,
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
        "pptx" => {
            let r = pptx::pptx_diff(path_a, path_b)?;
            res.file_type = "pptx".into();
            res.pptx = Some(r);
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

/// Extract the local name from a potentially namespace-prefixed XML tag name.
/// `"w:p"` → `"p"`, `"p"` → `"p"`.
pub(crate) fn xml_local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.into_owned(),
    }
}