//! Excel (.xlsx/.xls/.xlsm) diff: read workbooks via calamine, align rows and
//! columns via [`mumford::grid::grid_diff`].
//!
//! Calamine gives us raw cell values (no number-format styling); we normalize
//! them to strings, hand the string grid to `grid_diff`, and wrap the result
//! in per-sheet `SheetDiff` / `ExcelResult` containers with sheet names and
//! sheet-level status.

use calamine::{open_workbook_auto, Data, Reader};
use sha2::{Digest, Sha256};
use crate::grid::{grid_diff, GridDiff, GridOptions, Init};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

const MAX_EXCEL_ROWS: usize = 50_000;
const MAX_EXCEL_COLS: usize = 256;

/// One sheet's diff result: wraps [`GridDiff`] with the sheet name and a
/// sheet-level status (`equal` / `modified` / `added` / `removed`).
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

type Workbook = std::collections::BTreeMap<String, Vec<Vec<String>>>;

/// Options controlling how [`excel_diff_with`] aligns each sheet.
///
/// The default is a zero-assumption positional alignment (identity init, no
/// column locking) — the same behaviour as [`excel_diff`]. Callers that know
/// their sheets are tabular (a header row followed by data rows) can opt into
/// header-based column alignment and column locking, which keeps a column whose
/// every value changed (a timestamp column, say) as one modified column instead
/// of mis-reading it as a delete+insert of whole columns.
#[derive(Debug, Clone, Default)]
pub struct ExcelOptions {
    /// Detect each sheet's header row and seed column alignment from it (via
    /// [`Init::Header`]). When `false`, columns are aligned positionally.
    pub detect_header: bool,
    /// Lock the column alignment after initialization so coordinate descent
    /// never re-derives columns from cell content (see [`GridOptions::lock_columns`]).
    /// Most useful together with `detect_header` on fixed-schema tables.
    pub lock_columns: bool,
}

/// Diff two workbooks with the default (positional) alignment. Equivalent to
/// [`excel_diff_with`] with [`ExcelOptions::default`].
pub fn excel_diff(path_a: &str, path_b: &str) -> Result<ExcelResult, String> {
    excel_diff_with(path_a, path_b, &ExcelOptions::default())
}

/// Diff two workbooks with explicit [`ExcelOptions`].
pub fn excel_diff_with(
    path_a: &str,
    path_b: &str,
    opts: &ExcelOptions,
) -> Result<ExcelResult, String> {
    let mut res = ExcelResult {
        file_type: "excel".into(),
        path_a: path_a.into(),
        path_b: path_b.into(),
        ..Default::default()
    };

    let book_a = if path_a.is_empty() {
        None
    } else {
        Some(read_workbook(path_a, &mut res)?)
    };
    let book_b = if path_b.is_empty() {
        None
    } else {
        Some(read_workbook(path_b, &mut res)?)
    };

    let sheets_a: Vec<String> = book_a.as_ref().map(|b| b.keys().cloned().collect()).unwrap_or_default();
    let sheets_b: Vec<String> = book_b.as_ref().map(|b| b.keys().cloned().collect()).unwrap_or_default();
    let union = union_strings(&sheets_a, &sheets_b);

    for name in &union {
        let has_a = sheets_a.iter().any(|s| s == name);
        let has_b = sheets_b.iter().any(|s| s == name);
        let empty: Vec<Vec<String>> = Vec::new();
        let rows_a = book_a.as_ref().and_then(|b| b.get(name)).unwrap_or(&empty);
        let rows_b = book_b.as_ref().and_then(|b| b.get(name)).unwrap_or(&empty);

        let init = if opts.detect_header {
            Init::Header {
                a: detect_header_row(rows_a),
                b: detect_header_row(rows_b),
            }
        } else {
            Init::Positional
        };
        let grid_opts = GridOptions {
            init,
            lock_columns: opts.lock_columns,
            ..GridOptions::default()
        };

        let grid = grid_diff(rows_a, rows_b, &grid_opts);

        let status = if !has_a {
            "added".into()
        } else if !has_b {
            "removed".into()
        } else if grid.added_rows + grid.removed_rows + grid.modified_rows + grid.added_cols + grid.removed_cols > 0 {
            "modified".into()
        } else {
            "equal".into()
        };

        res.sheets.push(SheetDiff { name: name.clone(), status, grid });
    }

    Ok(res)
}

fn read_workbook(path: &str, res: &mut ExcelResult) -> Result<Workbook, String> {
    let mut wb = open_workbook_auto(path).map_err(|e| format!("open {}: {}", path, e))?;
    let mut out: Workbook = std::collections::BTreeMap::new();
    for name in wb.sheet_names().to_vec() {
        let range = match wb.worksheet_range(&name) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut truncated_rows = false;
        let mut truncated_cols = false;
        for row in range.rows() {
            if rows.len() >= MAX_EXCEL_ROWS {
                truncated_rows = true;
                break;
            }
            let (cells, tc) = normalize_row(row);
            truncated_cols |= tc;
            rows.push(cells);
        }
        while rows.last().map(|r| r.is_empty()).unwrap_or(false) {
            rows.pop();
        }
        if truncated_rows {
            res.notes.push(format!("sheet {name:?} truncated to {MAX_EXCEL_ROWS} rows"));
        }
        if truncated_cols {
            res.notes.push(format!("sheet {name:?} truncated to {MAX_EXCEL_COLS} columns"));
        }
        out.insert(name, rows);
    }
    Ok(out)
}

fn normalize_row(row: &[Data]) -> (Vec<String>, bool) {
    let mut cells: Vec<String> = row.iter().map(cell_to_string).collect();
    let mut truncated = false;
    if cells.len() > MAX_EXCEL_COLS {
        cells.truncate(MAX_EXCEL_COLS);
        truncated = true;
    }
    while cells.last().map(|c| c.is_empty()).unwrap_or(false) {
        cells.pop();
    }
    (cells, truncated)
}

/// Content fingerprint: every sheet's normalized cell grid hashed with sha256,
/// ignoring zip packaging noise (timestamps, docProps). Used by the folder diff
/// to avoid false "modified" on xlsx files with identical cells.
pub fn content_fingerprint(path: &str) -> Option<String> {
    let mut wb = open_workbook_auto(path).ok()?;
    let mut hasher = Sha256::new();
    for name in wb.sheet_names().to_vec() {
        let Ok(range) = wb.worksheet_range(&name) else { continue };
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
        let mut rows: Vec<Vec<String>> = Vec::new();
        for row in range.rows() {
            if rows.len() >= MAX_EXCEL_ROWS {
                break;
            }
            rows.push(normalize_row(row).0);
        }
        while rows.last().map(|r| r.is_empty()).unwrap_or(false) {
            rows.pop();
        }
        for r in &rows {
            for c in r {
                hasher.update(c.as_bytes());
                hasher.update([0x1f]);
            }
            hasher.update([0x1e]);
        }
    }
    Some(crate::hex(&hasher.finalize()))
}

fn cell_to_string(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Float(f) => format_float(*f),
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => if *b { "true".into() } else { "false".into() },
        Data::DateTime(dt) => format_float(dt.as_f64()),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("{e:?}"),
    }
}

fn format_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// Detect the header row of a sheet: the first row at least HEADER_FILL_NUM /
/// HEADER_FILL_DEN full of non-empty cells. Many spreadsheets carry a title and
/// a metadata block (report title, parameters, blank lines) above the real
/// column header, so row 0 is not a safe assumption; the first densely-filled
/// row is the header. Falls back to row 0 when nothing qualifies (e.g. a very
/// sparse sheet).
fn detect_header_row(rows: &[Vec<String>]) -> usize {
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if width < 2 {
        return 0;
    }
    rows.iter()
        .position(|r| {
            let filled = r.iter().filter(|c| !c.trim().is_empty()).count();
            filled * HEADER_FILL_DEN >= width * HEADER_FILL_NUM
        })
        .unwrap_or(0)
}

/// Header-row detection threshold: a row is the header when at least
/// HEADER_FILL_NUM / HEADER_FILL_DEN (= 80%) of the sheet width is non-empty.
const HEADER_FILL_NUM: usize = 8;
const HEADER_FILL_DEN: usize = 10;

fn union_strings(a: &[String], b: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for s in a.iter().chain(b.iter()) {
        if seen.insert(s.clone()) {
            out.push(s.clone());
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
    }

    #[test]
    fn diff_sheet_basic() {
        let a = grid(&[&["id", "v"], &["1", "a"], &["2", "b"], &["3", "c"]]);
        let b = grid(&[&["id", "v"], &["1", "a"], &["2", "CHANGED"], &["3", "c"]]);
        let opts = GridOptions::default();
        let gd = grid_diff(&a, &b, &opts);
        assert_eq!(gd.modified_rows, 1);
    }

    #[test]
    fn cell_to_string_integers() {
        assert_eq!(cell_to_string(&Data::Int(42)), "42");
        assert_eq!(cell_to_string(&Data::Float(3.0)), "3");
        assert_eq!(cell_to_string(&Data::Float(3.14)), "3.14");
        assert_eq!(cell_to_string(&Data::Bool(true)), "true");
        assert_eq!(cell_to_string(&Data::Empty), "");
    }

    #[test]
    fn empty_grids_produce_empty_result() {
        let a: Vec<Vec<String>> = Vec::new();
        let b: Vec<Vec<String>> = Vec::new();
        let opts = GridOptions::default();
        let gd = grid_diff(&a, &b, &opts);
        assert!(gd.rows.is_empty());
    }

    #[test]
    fn detect_header_row_skips_metadata_block() {
        // A spec-style sheet: title + metadata rows above the real header.
        let g = grid(&[
            &["My Spec Title"],
            &["Protocol", "", "", "D8450C00005"],
            &[],
            &["Domain", "Variable", "Label", "Type", "Length"],
            &["DM", "AGE", "Age", "num", "8"],
        ]);
        assert_eq!(detect_header_row(&g), 3);
    }

    #[test]
    fn detect_header_row_defaults_to_zero_when_dense() {
        let g = grid(&[
            &["Output", "Program", "Programmer", "Time", "Status"],
            &["adsl", "adsl.sas", "x", "10:00", "ok"],
        ]);
        assert_eq!(detect_header_row(&g), 0);
    }

    #[test]
    fn union_strings_preserves_unique_sorted() {
        let a = vec!["Sheet2".into(), "Sheet1".into()];
        let b = vec!["Sheet3".into(), "Sheet1".into()];
        let u = union_strings(&a, &b);
        assert_eq!(u, vec!["Sheet1", "Sheet2", "Sheet3"]);
    }
}