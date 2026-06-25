//! PDF text diff: extract per-page text with pdfium, strip repeated running
//! headers/footers, then diff the flat line stream via [`crate::text`].
//!
//! pdfium is bound at runtime (libloading) — nothing is linked at build time.
//! TODO: port from shtuka-core/src/pdf.rs.

use crate::text::TextResult;

pub fn pdf_diff(path_a: &str, path_b: &str) -> Result<TextResult, String> {
    let _ = (path_a, path_b);
    todo!("port pdf engine from shtuka-core/src/pdf.rs")
}