//! 2D grid alignment via coordinate descent.
//!
//! Given two tables (rows × columns of strings), produce one aligned grid with
//! cell-, row-, and column-level change status. Format-agnostic — callers parse
//! their format (Excel, CSV, HTML tables, SQL result sets, …) into
//! `&[Vec<String>]` and hand the grid to [`grid_diff`].
//!
//! # Algorithm
//!
//! The **table edit distance** problem — finding the minimum-cost set of row,
//! column, and cell operations to transform table A into table B — is NP-hard
//! (reduction from Maximum Biclique). This module approximates it via
//! **coordinate descent**:
//!
//! 1. **Initialize** row and column alignment. The default is positional
//!    (identity) — zero assumption. When the caller provides [`Init::Header`],
//!    the header row seeds the initial column alignment as an informed prior.
//! 2. **Alternate** between two polynomial-time sub-problems:
//!    - *Fix columns, optimize rows*: 1D LCS on row keys (cells at aligned
//!      column positions).
//!    - *Fix rows, optimize columns*: 1D LCS on column keys (cells at aligned
//!      row positions).
//! 3. **Converge** when both alignments stabilise. Each step is exact (LCS),
//!    so total cost monotonically decreases → finite-step convergence to a
//!    local optimum.
//!
//! Rows and columns are handled by the **same** alignment subroutine — they
//! are duals of each other. The only asymmetry is the caller's initialisation
//! choice.
//!
//! # Cost model
//!
//! All thresholds are derived from the [`Cost`] struct, not magic numbers:
//!
//! | Operation | Cost |
//! |-----------|------|
//! | insert/delete row | `cost.row` (α) |
//! | insert/delete column | `cost.col` (β) |
//! | modify cell | `cost.cell` (γ) |
//!
//! A delete+insert pair is promoted to "modified" when modification is cheaper:
//! `(1 − similarity) × n × γ < 2α`, i.e. `similarity > 1 − 2α/(nγ)`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::inline::OpType;
use crate::inline::{inline_segments, Seg, DEFAULT_SIMILARITY};
use crate::lcs::lcs_diff;

// ─── Output types ────────────────────────────────────────────────────────────

/// Status of one cell, row, or column in the aligned grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum Status {
    Equal,
    Modified,
    Added,
    Removed,
}

/// One diffed cell: status plus the old and new text (either may be empty when
/// the cell only exists on one side). For `Modified` cells, `old_segs` /
/// `new_segs` carry word-level inline segments for highlighting only the
/// changed words within the cell.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct CellChange {
    pub status: Status,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "String::is_empty"))]
    pub old: String,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "String::is_empty"))]
    pub new: String,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
    pub old_segs: Vec<Seg>,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
    pub new_segs: Vec<Seg>,
}

/// One aligned column: its display name and its status across the two sides.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct GridColumn {
    pub name: String,
    pub status: Status,
}

/// One aligned row: its status and source-row pointers (1-based, 0 = absent).
/// `header` is set only when the caller provides [`Init::Header`] and this row
/// is the designated header row.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct GridRow {
    pub status: Status,
    #[cfg_attr(feature = "serde", serde(rename = "rowA"))]
    pub row_a: usize,
    #[cfg_attr(feature = "serde", serde(rename = "rowB"))]
    pub row_b: usize,
    pub header: bool,
    pub cells: Vec<CellChange>,
}

/// The result of diffing two grids: aligned columns, rows, and per-cell status.
#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct GridDiff {
    pub columns: Vec<GridColumn>,
    pub rows: Vec<GridRow>,
    #[cfg_attr(feature = "serde", serde(rename = "addedRows"))]
    pub added_rows: usize,
    #[cfg_attr(feature = "serde", serde(rename = "removedRows"))]
    pub removed_rows: usize,
    #[cfg_attr(feature = "serde", serde(rename = "modifiedRows"))]
    pub modified_rows: usize,
    #[cfg_attr(feature = "serde", serde(rename = "addedCols"))]
    pub added_cols: usize,
    #[cfg_attr(feature = "serde", serde(rename = "removedCols"))]
    pub removed_cols: usize,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
    pub notes: Vec<String>,
}

// ─── Configuration ───────────────────────────────────────────────────────────

/// Cost model for the table edit distance. All algorithmic thresholds (when to
/// promote a delete+insert pair to `Modified`) are derived from these three
/// numbers.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Cost {
    /// α — cost of inserting or deleting a row.
    pub row: f64,
    /// β — cost of inserting or deleting a column.
    pub col: f64,
    /// γ — cost of modifying a single cell value.
    pub cell: f64,
}

impl Default for Cost {
    fn default() -> Self {
        Cost { row: 1.0, col: 1.0, cell: 1.0 }
    }
}

/// Initialization strategy for coordinate descent.
///
/// The algorithm is always the same (positional init → coordinate descent).
/// `Init` only controls the **starting point** — a better prior accelerates
/// convergence and avoids bad local optima, but the algorithm's correctness
/// and convergence guarantee do not depend on it.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Init {
    /// Identity (positional) alignment — zero assumption. The most general
    /// choice; coordinate descent discovers the correct alignment from data
    /// alone.
    #[default]
    Positional,
    /// Use the specified rows as headers to seed the initial column alignment
    /// via LCS on header text.
    Header {
        /// 0-based header row index in table A.
        a: usize,
        /// 0-based header row index in table B.
        b: usize,
    },
}

/// Configuration for [`grid_diff`].
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct GridOptions {
    /// Cost model — determines all thresholds.
    pub cost: Cost,
    /// Maximum coordinate-descent iterations. Each iteration re-aligns both
    /// rows and columns. Convergence typically in 1–2.
    pub max_iters: usize,
    /// Row-count cap for coordinate descent. Tables exceeding this are aligned
    /// positionally (O(n), no LCS). Raise for server use, lower for
    /// interactive previews.
    pub max_rows: usize,
    /// Initialization strategy.
    pub init: Init,
    /// Lock the column alignment to the initialization and never re-derive it
    /// from cell content. When `true`, coordinate descent only re-aligns rows;
    /// columns stay exactly as [`Init`] set them. Use with [`Init::Header`] when
    /// the table has a stable header row (e.g. a fixed schema): a column whose
    /// every value changed (a timestamp column, say) then stays one modified
    /// column instead of being mis-read as a delete+insert of whole columns.
    pub lock_columns: bool,
}

impl Default for GridOptions {
    fn default() -> Self {
        GridOptions {
            cost: Cost::default(),
            max_iters: 3,
            max_rows: 4_000,
            init: Init::Positional,
            lock_columns: false,
        }
    }
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Diff two string grids and produce one aligned grid.
///
/// `rows_a` / `rows_b` are arbitrary `&[Vec<String>]`. Each row is one record;
/// each cell is already stringified. The algorithm has no knowledge of any
/// particular file format.
pub fn grid_diff(
    rows_a: &[Vec<String>],
    rows_b: &[Vec<String>],
    opts: &GridOptions,
) -> GridDiff {
    let wa = max_width(rows_a);
    let wb = max_width(rows_b);

    // ── 1. Initialize alignment ──
    let (mut col_align, mut row_align) = init_alignment(rows_a, rows_b, &opts.init, wa, wb);

    // ── 2. Coordinate descent ──
    let too_many = rows_a.len() > opts.max_rows || rows_b.len() > opts.max_rows;
    if !too_many {
        for _ in 0..opts.max_iters {
            let new_rows = align_rows(rows_a, rows_b, &col_align, &opts.cost);
            // When columns are locked, keep the initialization alignment and
            // only converge rows; otherwise re-derive columns from cell content.
            let new_cols = if opts.lock_columns {
                col_align.clone()
            } else {
                align_cols(rows_a, rows_b, &new_rows, &opts.cost)
            };
            if new_cols == col_align && new_rows == row_align {
                break;
            }
            col_align = new_cols;
            row_align = new_rows;
        }
    }

    // ── 3. Render ──
    render(rows_a, rows_b, &col_align, &row_align, opts, too_many)
}

// ─── Internal: alignment pair ────────────────────────────────────────────────

/// One aligned pair: 0-based indices into A and B, or -1 when absent.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Pair {
    a: isize,
    b: isize,
}

impl Pair {
    fn both(a: usize, b: usize) -> Self { Pair { a: a as isize, b: b as isize } }
    fn a_only(a: usize) -> Self { Pair { a: a as isize, b: -1 } }
    fn b_only(b: usize) -> Self { Pair { a: -1, b: b as isize } }
}

// ─── Initialization ──────────────────────────────────────────────────────────

fn init_alignment(
    a: &[Vec<String>],
    b: &[Vec<String>],
    init: &Init,
    wa: usize,
    wb: usize,
) -> (Vec<Pair>, Vec<Pair>) {
    let rows = positional_pairs(a.len(), b.len());
    let cols = match init {
        Init::Positional => positional_pairs(wa, wb),
        Init::Header { a: ha, b: hb } => {
            if *ha < a.len() && *hb < b.len() {
                header_pairs(&a[*ha], &b[*hb], wa, wb)
            } else {
                positional_pairs(wa, wb)
            }
        }
    };
    (cols, rows)
}

fn positional_pairs(na: usize, nb: usize) -> Vec<Pair> {
    let n = na.max(nb);
    (0..n)
        .map(|i| Pair {
            a: if i < na { i as isize } else { -1 },
            b: if i < nb { i as isize } else { -1 },
        })
        .collect()
}

fn header_pairs(head_a: &[String], head_b: &[String], wa: usize, wb: usize) -> Vec<Pair> {
    let norm = |s: &str| s.trim().to_lowercase();
    let ka: Vec<String> = (0..wa).map(|i| norm(head_a.get(i).map(|s| s.as_str()).unwrap_or(""))).collect();
    let kb: Vec<String> = (0..wb).map(|i| norm(head_b.get(i).map(|s| s.as_str()).unwrap_or(""))).collect();
    let ops = lcs_diff(&ka, &kb);
    let mut pairs = Vec::new();
    for op in &ops {
        match op.typ {
            OpType::Equal => pairs.push(Pair::both(op.a - 1, op.b - 1)),
            OpType::Delete => pairs.push(Pair::a_only(op.a - 1)),
            OpType::Insert => pairs.push(Pair::b_only(op.b - 1)),
            OpType::Replace => {}
        }
    }
    pairs
}

// ─── Core: unified 1D alignment ──────────────────────────────────────────────

/// Align two sequences via LCS, then greedily promote similar delete+insert
/// pairs to Modified. The single primitive used for both row and column
/// alignment.
fn align_1d(
    keys_a: &[String],
    keys_b: &[String],
    sim_fn: &dyn Fn(usize, usize) -> f64,
    modify_threshold: f64,
) -> Vec<Pair> {
    let ops = lcs_diff(keys_a, keys_b);
    let mut pairs = Vec::new();
    let mut pending_del: Vec<usize> = Vec::new();
    let mut pending_ins: Vec<usize> = Vec::new();

    for op in &ops {
        match op.typ {
            OpType::Equal => {
                repair_gap(&pending_del, &pending_ins, &mut pairs, sim_fn, modify_threshold);
                pending_del.clear();
                pending_ins.clear();
                pairs.push(Pair::both(op.a - 1, op.b - 1));
            }
            OpType::Delete => pending_del.push(op.a - 1),
            OpType::Insert => pending_ins.push(op.b - 1),
            OpType::Replace => {}
        }
    }
    repair_gap(&pending_del, &pending_ins, &mut pairs, sim_fn, modify_threshold);
    pairs
}

/// Greedily match deleted elements to inserted elements by similarity. Pairs
/// at or above `modify_threshold` become matched pairs (rendered as Modified);
/// leftovers stay pure delete/insert.
fn repair_gap(
    dels: &[usize],
    ins: &[usize],
    pairs: &mut Vec<Pair>,
    sim_fn: &dyn Fn(usize, usize) -> f64,
    modify_threshold: f64,
) {
    let mut used_ins = vec![false; ins.len()];
    let mut match_of_del: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();

    for &d in dels {
        let mut best_j: isize = -1;
        let mut best_sim = f64::NEG_INFINITY;
        for (j, &i) in ins.iter().enumerate() {
            if used_ins[j] {
                continue;
            }
            let s = sim_fn(d, i);
            if s > best_sim {
                best_sim = s;
                best_j = j as isize;
            }
        }
        if best_j >= 0 && best_sim >= modify_threshold {
            used_ins[best_j as usize] = true;
            match_of_del.insert(d, ins[best_j as usize]);
        }
    }

    for &d in dels {
        if let Some(&i) = match_of_del.get(&d) {
            pairs.push(Pair::both(d, i));
        } else {
            pairs.push(Pair::a_only(d));
        }
    }
    for (j, &i) in ins.iter().enumerate() {
        if !used_ins[j] {
            pairs.push(Pair::b_only(i));
        }
    }
}

// ─── Row alignment: fix columns, optimize rows ───────────────────────────────

fn align_rows(
    a: &[Vec<String>],
    b: &[Vec<String>],
    col_align: &[Pair],
    cost: &Cost,
) -> Vec<Pair> {
    let common: Vec<(usize, usize)> = col_align
        .iter()
        .filter(|p| p.a >= 0 && p.b >= 0)
        .map(|p| (p.a as usize, p.b as usize))
        .collect();

    if common.is_empty() {
        return positional_pairs(a.len(), b.len());
    }

    let cols_a: Vec<usize> = common.iter().map(|(c, _)| *c).collect();
    let cols_b: Vec<usize> = common.iter().map(|(_, c)| *c).collect();
    let keys_a = compute_row_keys(a, &cols_a);
    let keys_b = compute_row_keys(b, &cols_b);

    let n = common.len();
    let threshold = modify_threshold(cost.row, cost.cell, n);

    let sim = |ra: usize, rb: usize| -> f64 {
        let mut same = 0;
        for &(ca, cb) in &common {
            let va = a.get(ra).and_then(|r| r.get(ca)).map(|s| s.as_str()).unwrap_or("");
            let vb = b.get(rb).and_then(|r| r.get(cb)).map(|s| s.as_str()).unwrap_or("");
            if va == vb {
                same += 1;
            }
        }
        same as f64 / n as f64
    };

    align_1d(&keys_a, &keys_b, &sim, threshold)
}

// ─── Column alignment: fix rows, optimize columns (dual) ─────────────────────

fn align_cols(
    a: &[Vec<String>],
    b: &[Vec<String>],
    row_align: &[Pair],
    cost: &Cost,
) -> Vec<Pair> {
    let common: Vec<(usize, usize)> = row_align
        .iter()
        .filter(|p| p.a >= 0 && p.b >= 0)
        .map(|p| (p.a as usize, p.b as usize))
        .collect();

    let wa = max_width(a);
    let wb = max_width(b);

    if common.is_empty() {
        return positional_pairs(wa, wb);
    }

    let rows_a: Vec<usize> = common.iter().map(|(r, _)| *r).collect();
    let rows_b: Vec<usize> = common.iter().map(|(_, r)| *r).collect();
    let keys_a = compute_col_keys(a, &rows_a, wa);
    let keys_b = compute_col_keys(b, &rows_b, wb);

    let m = common.len();
    let threshold = modify_threshold(cost.col, cost.cell, m);

    let sim = |ca: usize, cb: usize| -> f64 {
        let mut same = 0;
        for &(ra, rb) in &common {
            let va = a.get(ra).and_then(|r| r.get(ca)).map(|s| s.as_str()).unwrap_or("");
            let vb = b.get(rb).and_then(|r| r.get(cb)).map(|s| s.as_str()).unwrap_or("");
            if va == vb {
                same += 1;
            }
        }
        same as f64 / m as f64
    };

    align_1d(&keys_a, &keys_b, &sim, threshold)
}

// ─── Key computation ─────────────────────────────────────────────────────────

/// Compute a hash key per row. Two rows with the same key are guaranteed equal
/// across all aligned column positions, so LCS can compare keys in O(1).
fn compute_row_keys(rows: &[Vec<String>], col_indices: &[usize]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let mut h = DefaultHasher::new();
            for &c in col_indices {
                row.get(c).map(|s| s.as_str()).unwrap_or("").hash(&mut h);
            }
            format!("{:016x}", h.finish())
        })
        .collect()
}

fn compute_col_keys(rows: &[Vec<String>], row_indices: &[usize], width: usize) -> Vec<String> {
    (0..width)
        .map(|col| {
            let mut h = DefaultHasher::new();
            for &r in row_indices {
                rows.get(r)
                    .and_then(|row| row.get(col))
                    .map(|s| s.as_str())
                    .unwrap_or("")
                    .hash(&mut h);
            }
            format!("{:016x}", h.finish())
        })
        .collect()
}

// ─── Cost-derived threshold ──────────────────────────────────────────────────

/// Derive the similarity threshold for promoting a delete+insert pair to
/// Modified: `sim > 1 − 2α/(nγ)`.
fn modify_threshold(unit_cost: f64, cell_cost: f64, n: usize) -> f64 {
    if n == 0 || cell_cost <= 0.0 {
        return f64::INFINITY;
    }
    1.0 - (2.0 * unit_cost) / (cell_cost * n as f64)
}

// ─── Rendering ───────────────────────────────────────────────────────────────

fn render(
    a: &[Vec<String>],
    b: &[Vec<String>],
    col_align: &[Pair],
    row_align: &[Pair],
    opts: &GridOptions,
    too_many: bool,
) -> GridDiff {
    let (head_a, head_b) = match &opts.init {
        Init::Header { a: ha, b: hb } => (
            a.get(*ha).cloned().unwrap_or_default(),
            b.get(*hb).cloned().unwrap_or_default(),
        ),
        Init::Positional => (Vec::new(), Vec::new()),
    };

    let mut gd = GridDiff::default();

    for (k, pair) in col_align.iter().enumerate() {
        let status = pair_status(*pair);
        match status {
            Status::Added => gd.added_cols += 1,
            Status::Removed => gd.removed_cols += 1,
            _ => {}
        }
        let name = column_name(k, *pair, &head_a, &head_b, &opts.init);
        gd.columns.push(GridColumn { name, status });
    }

    for rp in row_align {
        let mut gr = build_grid_row(*rp, a, b, col_align);
        if let Init::Header { a: ha, b: hb } = &opts.init {
            if (gr.row_a != 0 && gr.row_a == *ha + 1) || (gr.row_b != 0 && gr.row_b == *hb + 1) {
                gr.header = true;
            }
        }
        match gr.status {
            Status::Added => gd.added_rows += 1,
            Status::Removed => gd.removed_rows += 1,
            Status::Modified => gd.modified_rows += 1,
            _ => {}
        }
        gd.rows.push(gr);
    }

    if too_many {
        gd.notes.push(format!(
            "row count exceeds budget ({}); rows matched by position",
            opts.max_rows
        ));
    }

    gd
}

fn pair_status(pair: Pair) -> Status {
    match (pair.a >= 0, pair.b >= 0) {
        (false, true) => Status::Added,
        (true, false) => Status::Removed,
        _ => Status::Equal,
    }
}

fn column_name(k: usize, pair: Pair, head_a: &[String], head_b: &[String], init: &Init) -> String {
    match init {
        Init::Positional => col_letter(k),
        Init::Header { .. } => {
            let from_b = if pair.b >= 0 {
                head_b.get(pair.b as usize).map(|s| s.trim()).filter(|s| !s.is_empty())
            } else {
                None
            };
            let from_a = if pair.a >= 0 {
                head_a.get(pair.a as usize).map(|s| s.trim()).filter(|s| !s.is_empty())
            } else {
                None
            };
            from_b.or(from_a).map(|s| s.to_string()).unwrap_or_else(|| col_letter(k))
        }
    }
}

fn build_grid_row(
    rp: Pair,
    rows_a: &[Vec<String>],
    rows_b: &[Vec<String>],
    col_align: &[Pair],
) -> GridRow {
    let ra: &[String] = if rp.a >= 0 { &rows_a[rp.a as usize] } else { &[] };
    let rb: &[String] = if rp.b >= 0 { &rows_b[rp.b as usize] } else { &[] };

    let row_status = if rp.a < 0 {
        Status::Added
    } else if rp.b < 0 {
        Status::Removed
    } else {
        Status::Equal
    };

    let mut gr = GridRow {
        status: row_status,
        row_a: if rp.a >= 0 { rp.a as usize + 1 } else { 0 },
        row_b: if rp.b >= 0 { rp.b as usize + 1 } else { 0 },
        header: false,
        cells: Vec::with_capacity(col_align.len()),
    };

    let get = |row: &[String], idx: isize| -> String {
        if idx < 0 {
            String::new()
        } else {
            row.get(idx as usize).cloned().unwrap_or_default()
        }
    };

    let mut modified = false;
    for slot in col_align {
        let va = get(ra, slot.a);
        let vb = get(rb, slot.b);
        let cc = if slot.b < 0 {
            CellChange { status: Status::Removed, old: va, new: String::new(), old_segs: Vec::new(), new_segs: Vec::new() }
        } else if slot.a < 0 {
            CellChange { status: Status::Added, old: String::new(), new: vb, old_segs: Vec::new(), new_segs: Vec::new() }
        } else {
            match row_status {
                Status::Added => CellChange { status: Status::Added, old: String::new(), new: vb, old_segs: Vec::new(), new_segs: Vec::new() },
                Status::Removed => CellChange { status: Status::Removed, old: va, new: String::new(), old_segs: Vec::new(), new_segs: Vec::new() },
                _ => {
                    if va != vb {
                        modified = true;
                        let (old_segs, new_segs) = inline_segments(&va, &vb, DEFAULT_SIMILARITY)
                            .unwrap_or_else(|| (Vec::new(), Vec::new()));
                        CellChange { status: Status::Modified, old: va, new: vb, old_segs, new_segs }
                    } else {
                        CellChange { status: Status::Equal, old: String::new(), new: vb, old_segs: Vec::new(), new_segs: Vec::new() }
                    }
                }
            }
        };
        gr.cells.push(cc);
    }
    if gr.status == Status::Equal && modified {
        gr.status = Status::Modified;
    }
    gr
}

// ─── Utilities ───────────────────────────────────────────────────────────────

fn max_width(rows: &[Vec<String>]) -> usize {
    rows.iter().map(|r| r.len()).max().unwrap_or(0)
}

/// Convert a 0-based column index to its Excel-style letter (0 → A, 25 → Z,
/// 26 → AA).
pub fn col_letter(mut i: usize) -> String {
    let mut b: Vec<u8> = Vec::new();
    i += 1;
    while i > 0 {
        i -= 1;
        b.insert(0, b'A' + (i % 26) as u8);
        i /= 26;
    }
    String::from_utf8(b).unwrap()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
    }

    fn counts(gd: &GridDiff) -> (usize, usize, usize, usize) {
        let (mut a, mut r, mut m, mut e) = (0, 0, 0, 0);
        for row in &gd.rows {
            match row.status {
                Status::Added => a += 1,
                Status::Removed => r += 1,
                Status::Modified => m += 1,
                Status::Equal => e += 1,
            }
        }
        (a, r, m, e)
    }

    #[test]
    fn col_letter_basic() {
        assert_eq!(col_letter(0), "A");
        assert_eq!(col_letter(25), "Z");
        assert_eq!(col_letter(26), "AA");
        assert_eq!(col_letter(27), "AB");
    }

    #[test]
    fn inserted_row_no_cascade() {
        let a = grid(&[&["name", "amount"], &["Alice", "100"], &["Bob", "200"], &["Carl", "300"], &["Dave", "400"]]);
        let b = grid(&[&["name", "amount"], &["Alice", "100"], &["Bob", "200"], &["NEW", "999"], &["Carl", "300"], &["Dave", "400"]]);
        let gd = grid_diff(&a, &b, &GridOptions::default());
        let (added, removed, modified, equal) = counts(&gd);
        assert_eq!((added, modified, removed), (1, 0, 0));
        assert_eq!(equal, 5);
    }

    #[test]
    fn single_cell_edit_is_modified() {
        let a = grid(&[&["id", "v"], &["1", "a"], &["2", "b"], &["3", "c"]]);
        let b = grid(&[&["id", "v"], &["1", "a"], &["2", "CHANGED"], &["3", "c"]]);
        let gd = grid_diff(&a, &b, &GridOptions::default());
        let (added, removed, modified, equal) = counts(&gd);
        assert_eq!((modified, added, removed), (1, 0, 0));
        assert_eq!(equal, 3);
    }

    #[test]
    fn positional_init_detects_column_insert() {
        let a = grid(&[&["a", "c"], &["1", "3"], &["4", "6"]]);
        let b = grid(&[&["a", "b", "c"], &["1", "2", "3"], &["4", "5", "6"]]);
        let gd = grid_diff(&a, &b, &GridOptions::default());
        assert!(gd.added_cols >= 1, "inserted column should be detected without header init");
    }

    #[test]
    fn appended_row_is_added() {
        let a = grid(&[&["Sku", "Category"], &["A001", "Tools"]]);
        let b = grid(&[&["Sku", "Category"], &["A001", "Tools"], &["A002", "Toys"]]);
        let gd = grid_diff(&a, &b, &GridOptions::default());
        let (added, removed, modified, _) = counts(&gd);
        assert_eq!((added, removed, modified), (1, 0, 0));
    }

    #[test]
    fn empty_grids() {
        let a: Vec<Vec<String>> = Vec::new();
        let b: Vec<Vec<String>> = Vec::new();
        let gd = grid_diff(&a, &b, &GridOptions::default());
        assert!(gd.rows.is_empty());
        assert_eq!(gd.added_rows + gd.removed_rows + gd.modified_rows, 0);
    }

    #[test]
    fn identical_grids_all_equal() {
        let a = grid(&[&["h1", "h2"], &["x", "y"], &["1", "2"]]);
        let b = grid(&[&["h1", "h2"], &["x", "y"], &["1", "2"]]);
        let gd = grid_diff(&a, &b, &GridOptions::default());
        let (added, removed, modified, equal) = counts(&gd);
        assert_eq!((added, removed, modified, equal), (0, 0, 0, 3));
    }

    #[test]
    fn large_table_falls_back_to_positional() {
        let mut big_a: Vec<Vec<String>> = Vec::new();
        let mut big_b: Vec<Vec<String>> = Vec::new();
        for i in 0..5 {
            big_a.push(vec![format!("r{i}"), "x".into()]);
            big_b.push(vec![format!("r{i}"), "x".into()]);
        }
        let opts = GridOptions { max_rows: 2, ..GridOptions::default() };
        let gd = grid_diff(&big_a, &big_b, &opts);
        assert!(!gd.notes.is_empty());
        assert!(gd.notes.iter().any(|n| n.contains("position")));
    }

    #[test]
    fn header_init_sets_header_flag() {
        let a = grid(&[&["h1", "h2", "h3", "h4"], &["1", "2", "3", "4"]]);
        let b = grid(&[&["h1"], &["1"]]);
        let opts = GridOptions {
            init: Init::Header { a: 0, b: 0 },
            ..GridOptions::default()
        };
        let gd = grid_diff(&a, &b, &opts);
        assert!(gd.rows.iter().any(|r| r.header), "header flag should be set");
    }

    #[test]
    fn column_names_from_header() {
        let a = grid(&[&["Name", "Value"], &["x", "1"]]);
        let b = grid(&[&["Name", "Value"], &["x", "2"]]);
        let opts = GridOptions {
            init: Init::Header { a: 0, b: 0 },
            ..GridOptions::default()
        };
        let gd = grid_diff(&a, &b, &opts);
        assert_eq!(gd.columns[0].name, "Name");
        assert_eq!(gd.columns[1].name, "Value");
    }

    #[test]
    fn positional_init_uses_letters() {
        let a = grid(&[&["Name", "Value"], &["x", "1"]]);
        let b = grid(&[&["Name", "Value"], &["x", "2"]]);
        let gd = grid_diff(&a, &b, &GridOptions::default());
        assert_eq!(gd.columns[0].name, "A");
        assert_eq!(gd.columns[1].name, "B");
    }

    #[test]
    fn header_init_sparse_layout() {
        let a = grid(&[
            &["Inventory Report"],
            &["Region:", "", "", "EMEA"],
            &[],
            &["Sku", "Description", "Category", "Stock"],
            &["A001", "Widget", "Tools", "20"],
            &["A002", "Gadget", "Toys", "2"],
        ]);
        let b = grid(&[
            &["Inventory Report"],
            &["Region:", "", "", "EMEA"],
            &[],
            &["Sku", "Description", "Category", "Stock"],
            &["A001", "Widget", "Tools", "21"],
            &["A002", "Gadget", "Toys", "2"],
        ]);
        let opts = GridOptions {
            init: Init::Header { a: 3, b: 3 },
            ..GridOptions::default()
        };
        let gd = grid_diff(&a, &b, &opts);
        assert_eq!(gd.columns.len(), 4);
        assert_eq!(gd.columns[0].name, "Sku");
        assert_eq!(gd.columns[3].name, "Stock");
        let (added, removed, modified, equal) = counts(&gd);
        assert_eq!((modified, added, removed), (1, 0, 0));
        assert_eq!(equal, 5);
        for row in &gd.rows {
            if row.status == Status::Modified {
                assert_eq!(row.cells.len(), 4);
                assert_eq!(row.cells[3].status, Status::Modified);
                assert_eq!(row.cells[3].old, "20");
                assert_eq!(row.cells[3].new, "21");
            }
        }
    }

    #[test]
    fn lock_columns_keeps_full_changed_column_aligned() {
        // Two tables, same header, but one whole column (col 1) has every value
        // changed. Without lock_columns the content-based column alignment reads
        // that as delete+insert of a column; with lock_columns + header init the
        // column stays put and the changes are reported as modified rows.
        let a = grid(&[
            &["id", "time", "status"],
            &["1", "09:00", "ok"],
            &["2", "09:05", "ok"],
            &["3", "09:10", "ok"],
        ]);
        let b = grid(&[
            &["id", "time", "status"],
            &["1", "10:00", "ok"],
            &["2", "10:05", "ok"],
            &["3", "10:10", "ok"],
        ]);
        let opts = GridOptions {
            init: Init::Header { a: 0, b: 0 },
            lock_columns: true,
            ..GridOptions::default()
        };
        let gd = grid_diff(&a, &b, &opts);
        assert_eq!(gd.added_cols, 0, "no columns should be added");
        assert_eq!(gd.removed_cols, 0, "no columns should be removed");
        assert_eq!(gd.columns.len(), 3);
        assert!(gd.columns.iter().all(|c| c.status == Status::Equal));
        // The three data rows each changed in the time column.
        assert_eq!(gd.modified_rows, 3);
    }

    #[test]
    fn cost_affects_modify_threshold() {
        let a = grid(&[&["a", "b", "c", "d"], &["1", "2", "3", "4"]]);
        let b = grid(&[&["a", "b", "c", "d"], &["5", "6", "7", "8"]]);

        // Default cost (α=1, γ=1, n=4): threshold = 0.5. sim = 0/4 → NOT modified.
        let gd = grid_diff(&a, &b, &GridOptions::default());
        let (_, _, modified, _) = counts(&gd);
        assert_eq!(modified, 0, "0% similarity should not be modified with default cost");

        // High row cost (α=10, γ=1, n=4): threshold = -4. sim = 0 ≥ -4 → Modified.
        let opts = GridOptions {
            cost: Cost { row: 10.0, ..Cost::default() },
            ..GridOptions::default()
        };
        let gd = grid_diff(&a, &b, &opts);
        let (_, _, modified, _) = counts(&gd);
        assert_eq!(modified, 1, "high row cost forces modified even with 0% similarity");
    }
}
