//! RTF diff for styled tables. We parse the RTF into a table model (cells with
//! text + semantic styling — background, color, alignment, bold, monospace),
//! align rows and cells between the two files, and tag each cell
//! added/removed/modified/equal. The output carries no layout information
//! (column widths, font sizes, borders) — consumers control rendering entirely.

use tate::inline::{inline_segments, Seg, DEFAULT_SIMILARITY, OpType};
use tate::lines::diff;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Visual style of one cell — semantic only, no layout. Consumers control
/// rendering (column widths, fonts, borders) entirely.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct CellStyle {
    /// Background color "#rrggbb" (from \clcbpat + \colortbl), empty = none.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "String::is_empty"))]
    pub bg: String,
    /// Text color "#rrggbb" (from \cf), empty = default.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "String::is_empty"))]
    pub color: String,
    /// "left" | "center" | "right".
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "String::is_empty"))]
    pub align: String,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "std::ops::Not::not"))]
    pub bold: bool,
    /// Monospace font (\f2 Courier in SAS output).
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "std::ops::Not::not"))]
    pub mono: bool,
}

/// One parsed cell: its text and style.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Cell {
    pub text: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub style: CellStyle,
}

/// A diffed cell in the output: the A and B cells (either may be absent) and the
/// change status.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DiffCell {
    pub status: String, // equal | modified | added | removed
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub a: Option<Cell>,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub b: Option<Cell>,
    /// For modified cells: word-level segments of A / B text so the UI highlights
    /// only the words that changed (like the PDF view), not the whole cell.
    #[cfg_attr(feature = "serde", serde(rename = "aSegs", default, skip_serializing_if = "Vec::is_empty"))]
    pub a_segs: Vec<Seg>,
    #[cfg_attr(feature = "serde", serde(rename = "bSegs", default, skip_serializing_if = "Vec::is_empty"))]
    pub b_segs: Vec<Seg>,
}

/// A diffed row: aligned cells plus the row-level status.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DiffRow {
    pub status: String, // equal | modified | added | removed
    pub cells: Vec<DiffCell>,
}

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct RtfResult {
    #[cfg_attr(feature = "serde", serde(rename = "fileType"))]
    pub file_type: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathA"))]
    pub path_a: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathB"))]
    pub path_b: String,
    pub rows: Vec<DiffRow>,
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
}

pub fn rtf_diff(path_a: &str, path_b: &str) -> Result<RtfResult, String> {
    let rows_a = read_rows(path_a)?;
    let rows_b = read_rows(path_b)?;

    let mut res = RtfResult {
        file_type: "rtf".into(),
        path_a: path_a.into(),
        path_b: path_b.into(),
        ..Default::default()
    };

    // Row alignment by text signature (same LCS approach as text/excel), so an
    // inserted row stays a single insert instead of cascading.
    let sig = |r: &Row| r.cells.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join("\u{0}");
    let sig_a: Vec<String> = rows_a.iter().map(sig).collect();
    let sig_b: Vec<String> = rows_b.iter().map(sig).collect();
    let ops = diff(&sig_a, &sig_b);

    let mut pending_del: Vec<usize> = Vec::new();
    let mut pending_ins: Vec<usize> = Vec::new();

    let flush = |res: &mut RtfResult, dels: &mut Vec<usize>, inss: &mut Vec<usize>| {
        // Pair similar deleted/inserted rows into "modified"; rest stay add/remove.
        let pairs = pair_rows(dels, inss, &rows_a, &rows_b);
        for p in pairs {
            match p {
                RowMatch::Modified(ai, bi) => {
                    res.rows.push(diff_row(Some(&rows_a[ai]), Some(&rows_b[bi])));
                    res.modified += 1;
                }
                RowMatch::Removed(ai) => {
                    res.rows.push(diff_row(Some(&rows_a[ai]), None));
                    res.removed += 1;
                }
                RowMatch::Added(bi) => {
                    res.rows.push(diff_row(None, Some(&rows_b[bi])));
                    res.added += 1;
                }
            }
        }
        dels.clear();
        inss.clear();
    };

    for op in &ops {
        match op.typ {
            OpType::Equal => {
                flush(&mut res, &mut pending_del, &mut pending_ins);
                let row = diff_row(Some(&rows_a[op.a]), Some(&rows_b[op.b]));
                if row.status == "modified" {
                    res.modified += 1;
                }
                res.rows.push(row);
            }
            OpType::Delete => pending_del.push(op.a),
            OpType::Insert => pending_ins.push(op.b),
            OpType::Replace => {}
        }
    }
    flush(&mut res, &mut pending_del, &mut pending_ins);

    Ok(res)
}

fn read_rows(path: &str) -> Result<Vec<Row>, String> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {}", path, e))?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(parse_rtf_tables(&text))
}

// --- row matching ----------------------------------------------------------

enum RowMatch {
    Modified(usize, usize),
    Removed(usize),
    Added(usize),
}

/// pair_rows greedily matches deleted rows to inserted rows by cell-text
/// similarity (>=0.5), turning close matches into modified pairs.
fn pair_rows(dels: &[usize], ins: &[usize], rows_a: &[Row], rows_b: &[Row]) -> Vec<RowMatch> {
    let mut used = vec![false; ins.len()];
    let mut out: Vec<RowMatch> = Vec::new();
    for &ai in dels {
        let mut best_j: isize = -1;
        let mut best = 0.0f64;
        let mut best_label = false;
        for (j, &bi) in ins.iter().enumerate() {
            if used[j] {
                continue;
            }
            let s = row_text_similarity(&rows_a[ai], &rows_b[bi]);
            // The first non-empty cell is the row label (primary key) in these
            // tables, e.g. "Any AE". If labels match, this is the same row with
            // changed values — pair it even when every number differs.
            let label = labels_match(&rows_a[ai], &rows_b[bi]);
            // Prefer a label match; among those (or among non-matches) take the
            // highest cell similarity.
            let better = match (label, best_label) {
                (true, false) => true,
                (false, true) => false,
                _ => s > best,
            };
            if better {
                best = s;
                best_j = j as isize;
                best_label = label;
            }
        }
        if best_j >= 0 && (best_label || best >= 0.5) {
            used[best_j as usize] = true;
            out.push(RowMatch::Modified(ai, ins[best_j as usize]));
        } else {
            out.push(RowMatch::Removed(ai));
        }
    }
    for (j, &bi) in ins.iter().enumerate() {
        if !used[j] {
            out.push(RowMatch::Added(bi));
        }
    }
    out
}

/// The row's label = its first non-empty cell text. Two rows "match by label"
/// when those are equal and non-empty — the primary-key signal for these tables.
fn first_label(r: &Row) -> &str {
    r.cells.iter().map(|c| c.text.trim()).find(|t| !t.is_empty()).unwrap_or("")
}

fn labels_match(a: &Row, b: &Row) -> bool {
    let la = first_label(a);
    !la.is_empty() && la == first_label(b)
}

fn row_text_similarity(a: &Row, b: &Row) -> f64 {
    let n = a.cells.len().max(b.cells.len());
    if n == 0 {
        return 1.0;
    }
    // Single-cell rows (footnotes, the program-name path line) carry one long
    // string; "whole cell equal?" is too coarse. Use token overlap (split on
    // non-alphanumeric, so "/" and spaces both delimit) — a path line differing
    // only in one segment ("primary_dr3" -> "ia1") still pairs as a modification.
    if a.cells.len() == 1 && b.cells.len() == 1 {
        return token_overlap(&a.cells[0].text, &b.cells[0].text);
    }
    let mut same = 0;
    for i in 0..n {
        let ta = a.cells.get(i).map(|c| c.text.as_str()).unwrap_or("");
        let tb = b.cells.get(i).map(|c| c.text.as_str()).unwrap_or("");
        if ta == tb {
            same += 1;
        }
    }
    same as f64 / n as f64
}

/// token_overlap splits both strings on non-alphanumeric runs (so "/", spaces,
/// punctuation all delimit) and returns shared tokens / longer token count.
fn token_overlap(a: &str, b: &str) -> f64 {
    let toks = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .collect()
    };
    let ta = toks(a);
    let tb = toks(b);
    let max = ta.len().max(tb.len());
    if max == 0 {
        return 1.0;
    }
    let mut counts: std::collections::HashMap<&str, i32> = std::collections::HashMap::new();
    for t in &tb {
        *counts.entry(t.as_str()).or_insert(0) += 1;
    }
    let mut shared = 0;
    for t in &ta {
        if let Some(c) = counts.get_mut(t.as_str()) {
            if *c > 0 {
                *c -= 1;
                shared += 1;
            }
        }
    }
    shared as f64 / max as f64
}

/// diff_row aligns the cells of an A row and a B row positionally (these tables
/// have a fixed column count per row) and tags each cell.
fn diff_row(a: Option<&Row>, b: Option<&Row>) -> DiffRow {
    let empty: Vec<Cell> = Vec::new();
    let ca = a.map(|r| &r.cells).unwrap_or(&empty);
    let cb = b.map(|r| &r.cells).unwrap_or(&empty);
    let width = ca.len().max(cb.len());

    let row_status = match (a.is_some(), b.is_some()) {
        (true, false) => "removed",
        (false, true) => "added",
        _ => "equal",
    };

    let mut cells: Vec<DiffCell> = Vec::with_capacity(width);
    let mut any_mod = false;
    for i in 0..width {
        let ac = ca.get(i).cloned();
        let bc = cb.get(i).cloned();
        let mut a_segs = Vec::new();
        let mut b_segs = Vec::new();
        let status = match row_status {
            "added" => "added",
            "removed" => "removed",
            _ => {
                let ta = ac.as_ref().map(|c| c.text.as_str()).unwrap_or("");
                let tb = bc.as_ref().map(|c| c.text.as_str()).unwrap_or("");
                if ta != tb {
                    any_mod = true;
                    // Word-level segments so only the changed words highlight
                    // (e.g. a footnote line where one path token differs).
                    if let Some((sa, sb)) = inline_segments(ta, tb, DEFAULT_SIMILARITY) {
                        a_segs = sa;
                        b_segs = sb;
                    }
                    "modified"
                } else {
                    "equal"
                }
            }
        };
        cells.push(DiffCell { status: status.into(), a: ac, b: bc, a_segs, b_segs });
    }

    let status = if row_status == "equal" && any_mod { "modified" } else { row_status };
    DiffRow { status: status.into(), cells }
}

// --- RTF parsing -----------------------------------------------------------

#[derive(Debug, Clone)]
struct Row {
    cells: Vec<Cell>,
    region: Region,
}

impl Default for Row {
    fn default() -> Self {
        Row { cells: Vec::new(), region: Region::Body }
    }
}

/// Character formatting state carried while scanning, applied to each cell.
#[derive(Debug, Clone, Default)]
struct CharState {
    color_idx: usize, // \cf
    bold: bool,
    mono: bool,
}

/// Per-cell-definition state from \trowd (shading, alignment).
#[derive(Debug, Clone, Default)]
struct CellDef {
    bg_idx: usize,   // \clcbpat color index
    align: String,   // from \ql/\qc/\qr seen before \cell
}

fn is_ascii_letter(c: char) -> bool {
    c.is_ascii_alphabetic()
}

fn is_destination(word: &str) -> bool {
    matches!(
        word,
        "fonttbl" | "stylesheet" | "info" | "pict"
            | "listtable" | "listoverridetable" | "rsidtbl" | "generator" | "themedata"
            | "datastore" | "latentstyles"
    )
}

/// Which part of the document the current rows belong to. Headers/footers repeat
/// on every page in SAS RTF, so we collect them separately and de-duplicate.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Region {
    Body,
    Header,
    Footer,
}

/// parse_rtf_tables walks the RTF and emits table rows with styled cells.
fn parse_rtf_tables(s: &str) -> Vec<Row> {
    let runes: Vec<char> = s.chars().collect();
    let n = runes.len();

    let mut colors: Vec<String> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();

    // Current row being built.
    let mut cell_defs: Vec<CellDef> = Vec::new(); // from \trowd...\cellx
    let mut row_cells: Vec<Cell> = Vec::new();
    let mut cur_text = String::new();
    let mut cur_align = String::new();
    let mut pending_def = CellDef::default(); // accumulates \clcbpat/\clbrdr for next \cellx

    let mut cs = CharState::default();
    let mut cs_stack: Vec<CharState> = Vec::new();

    let mut skip_depth: isize = -1;
    let mut depth: isize = 0;
    let mut i = 0usize;
    let mut in_colortbl = false;
    let mut cur_color = ColorAccum::default();
    // Header/footer region tracking: a \header or \footer opens a group whose
    // rows belong to that region until the group (at region_depth) closes.
    let mut region = Region::Body;
    let mut region_depth: isize = -1;

    while i < n {
        let c = runes[i];
        match c {
            '{' => {
                cs_stack.push(cs.clone());
                depth += 1;
                i += 1;
            }
            '}' => {
                if in_colortbl {
                    in_colortbl = false;
                }
                if skip_depth >= 0 && depth <= skip_depth {
                    skip_depth = -1;
                }
                //Leaving the header/footer group returns us to the body.
                if region_depth >= 0 && depth <= region_depth {
                    region = Region::Body;
                    region_depth = -1;
                }
                cs = cs_stack.pop().unwrap_or_default();
                depth -= 1;
                i += 1;
            }
            '\\' => {
                if i + 1 >= n {
                    i += 1;
                    continue;
                }
                let next = runes[i + 1];
                if next == '\\' || next == '{' || next == '}' {
                    if skip_depth < 0 {
                        cur_text.push(next);
                    }
                    i += 2;
                    continue;
                }
                if next == '*' {
                    if skip_depth < 0 {
                        skip_depth = depth;
                    }
                    i += 2;
                    continue;
                }
                if next == '\'' && i + 3 < n {
                    let hex: String = runes[i + 2..i + 4].iter().collect();
                    if let Ok(v) = i64::from_str_radix(&hex, 16) {
                        if skip_depth < 0 && !in_colortbl {
                            if let Some(ch) = char::from_u32(v as u32) {
                                cur_text.push(ch);
                            }
                        }
                    }
                    i += 4;
                    continue;
                }
                if is_ascii_letter(next) {
                    let mut j = i + 1;
                    while j < n && is_ascii_letter(runes[j]) {
                        j += 1;
                    }
                    let word: String = runes[i + 1..j].iter().collect();
                    let num_start = j;
                    let mut neg = false;
                    if j < n && (runes[j] == '-' || runes[j].is_ascii_digit()) {
                        if runes[j] == '-' {
                            neg = true;
                        }
                        j += 1;
                        while j < n && runes[j].is_ascii_digit() {
                            j += 1;
                        }
                    }
                    let param_str: String = if j > num_start {
                        runes[num_start..j].iter().collect()
                    } else {
                        String::new()
                    };
                    let param: i64 = param_str.parse().unwrap_or(0);
                    if j < n && runes[j] == ' ' {
                        j += 1;
                    }

                    handle_word(
                        &word, param, neg, &param_str, depth,
                        &mut skip_depth, &mut in_colortbl, &mut cur_color, &mut colors,
                        &mut cs, &mut cur_align, &mut pending_def, &mut cell_defs,
                        &mut row_cells, &mut cur_text, &mut rows,
                        &mut region, &mut region_depth,
                    );

                    if word == "u" && j < n {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                i += 2;
            }
            ';' if in_colortbl => {
                // End of one color table entry.
                colors.push(cur_color.to_hex());
                cur_color = ColorAccum::default();
                i += 1;
            }
            _ => {
                if skip_depth < 0 && !in_colortbl {
                    cur_text.push(c);
                }
                i += 1;
            }
        }
    }

    // Resolve color indices -> hex now that the color table is known.
    for row in &mut rows {
        for cell in &mut row.cells {
            resolve_colors(cell, &colors);
        }
    }

    // Headers/footers repeat on every page; keep only the first occurrence of
    // each distinct row, and lay the document out as: headers, body, footers.
    let key = |r: &Row| r.cells.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join("\u{1}");
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let (mut headers, mut body, mut footers) = (Vec::new(), Vec::new(), Vec::new());
    for row in rows {
        match row.region {
            Region::Body => body.push(row),
            Region::Header => {
                if seen.insert(format!("h\u{2}{}", key(&row))) {
                    headers.push(row);
                }
            }
            Region::Footer => {
                if seen.insert(format!("f\u{2}{}", key(&row))) {
                    footers.push(row);
                }
            }
        }
    }
    let mut out = Vec::with_capacity(headers.len() + body.len() + footers.len());
    out.append(&mut headers);
    out.append(&mut body);
    out.append(&mut footers);
    out
}

#[derive(Default)]
struct ColorAccum {
    r: u8,
    g: u8,
    b: u8,
    seen: bool,
}
impl ColorAccum {
    fn to_hex(&self) -> String {
        // The default (first) entry is often empty -> treat as "auto"/none.
        if !self.seen {
            return String::new();
        }
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Cells store color *indices* during parse (encoded in style.bg/color as
/// "idx:N"); resolve_colors swaps them for hex now that \colortbl is parsed.
fn resolve_colors(cell: &mut Cell, colors: &[String]) {
    if let Some(idx) = cell.style.bg.strip_prefix("idx:").and_then(|s| s.parse::<usize>().ok()) {
        cell.style.bg = colors.get(idx).cloned().unwrap_or_default();
    }
    if let Some(idx) = cell.style.color.strip_prefix("idx:").and_then(|s| s.parse::<usize>().ok()) {
        cell.style.color = colors.get(idx).cloned().unwrap_or_default();
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_word(
    word: &str,
    param: i64,
    _neg: bool,
    param_str: &str,
    depth: isize,
    skip_depth: &mut isize,
    in_colortbl: &mut bool,
    cur_color: &mut ColorAccum,
    _colors: &mut [String],
    cs: &mut CharState,
    cur_align: &mut String,
    pending_def: &mut CellDef,
    cell_defs: &mut Vec<CellDef>,
    row_cells: &mut Vec<Cell>,
    cur_text: &mut String,
    rows: &mut Vec<Row>,
    region: &mut Region,
    region_depth: &mut isize,
) {
    if word == "colortbl" {
        *in_colortbl = true;
        return;
    }
    // \header / \footer open a region group whose rows we keep (deduped later).
    if (word == "header" || word == "footer") && *skip_depth < 0 {
        *region = if word == "header" { Region::Header } else { Region::Footer };
        *region_depth = depth;
        return;
    }
    if is_destination(word) && *skip_depth < 0 {
        *skip_depth = depth;
        return;
    }
    if *skip_depth >= 0 {
        return;
    }
    if *in_colortbl {
        match word {
            "red" => {
                cur_color.r = param as u8;
                cur_color.seen = true;
            }
            "green" => {
                cur_color.g = param as u8;
                cur_color.seen = true;
            }
            "blue" => {
                cur_color.b = param as u8;
                cur_color.seen = true;
            }
            _ => {}
        }
        return;
    }

    match word {
        // --- character formatting ---
        "cf" => cs.color_idx = param as usize,
        "b" => cs.bold = param_str.is_empty() || param != 0,
        "f" => cs.mono = param == 2, // \f2 = Courier in SAS output
        "plain" => *cs = CharState::default(),
        // --- alignment (applies to current paragraph/cell) ---
        "ql" => *cur_align = "left".into(),
        "qc" => *cur_align = "center".into(),
        "qr" => *cur_align = "right".into(),
        // --- table row/cell definition ---
        "trowd" => {
            cell_defs.clear();
            *pending_def = CellDef::default();
        }
        "clcbpat" => pending_def.bg_idx = param as usize,
        "cellx" => {
            pending_def.align = cur_align.clone();
            cell_defs.push(std::mem::take(pending_def));
        }
        // --- content breaks ---
        "cell" => {
            let idx = row_cells.len();
            let def = cell_defs.get(idx).cloned().unwrap_or_default();
            row_cells.push(make_cell(cur_text, cs, &def, cur_align));
            cur_text.clear();
        }
        "row" => {
            if !row_cells.is_empty() {
                rows.push(Row { cells: std::mem::take(row_cells), region: *region });
            }
            cur_text.clear();
        }
        "par" | "line" => cur_text.push('\n'),
        "tab" => cur_text.push('\t'),
        "u" => {
            if let Ok(mut v) = param_str.parse::<i64>() {
                if v < 0 {
                    v += 65536;
                }
                if let Some(ch) = char::from_u32(v as u32) {
                    cur_text.push(ch);
                }
            }
        }
        _ => {}
    }
}

fn make_cell(
    text: &str,
    cs: &CharState,
    def: &CellDef,
    cur_align: &str,
) -> Cell {
    let align = if !def.align.is_empty() {
        def.align.clone()
    } else if !cur_align.is_empty() {
        cur_align.to_string()
    } else {
        String::new()
    };

    let mut style = CellStyle {
        align,
        bold: cs.bold,
        mono: cs.mono,
        ..Default::default()
    };
    if def.bg_idx > 0 {
        style.bg = format!("idx:{}", def.bg_idx);
    }
    if cs.color_idx > 0 {
        style.color = format!("idx:{}", cs.color_idx);
    }
    Cell { text: normalize_cell(text), style }
}

/// normalize_cell tidies a cell's text WITHOUT destroying structure: it keeps
/// intra-cell line breaks (from \line/\par) and each line's leading indentation
/// (SAS encodes the SOC→PT hierarchy as leading spaces), only trimming trailing
/// whitespace and collapsing interior whitespace runs. Leading tabs become two
/// spaces so indentation is visible. Fully-blank leading/trailing lines drop.
fn normalize_cell(s: &str) -> String {
    let mut lines: Vec<String> = s
        .split('\n')
        .map(|line| {
            // Count leading indentation (spaces/tabs), tab = 2 spaces.
            let mut indent = 0usize;
            for ch in line.chars() {
                match ch {
                    ' ' => indent += 1,
                    '\t' => indent += 2,
                    _ => break,
                }
            }
            let body = line.trim();
            if body.is_empty() {
                String::new()
            } else {
                format!("{}{}", " ".repeat(indent), collapse_interior(body))
            }
        })
        .collect();
    while lines.first().map(|l| l.is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    // Drop interior blank lines: SAS emits \line + a literal newline (two breaks)
    // between header sub-lines, which would otherwise double-space the cell. We
    // keep only non-empty lines, joined by single newlines.
    lines.retain(|l| !l.is_empty());
    lines.join("\n")
}

/// collapse_interior squeezes runs of 2+ interior spaces/tabs to one space
/// (SAS pads columns heavily) while leaving the already-stripped ends alone.
fn collapse_interior(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        let ws = ch == ' ' || ch == '\t';
        if ws {
            if !prev_ws {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
        prev_ws = ws;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rows_cells_and_style() {
        let rtf = r"{\rtf1\ansi{\colortbl;\red255\green0\blue0;}\trowd\clcbpat1\cellx5000\cellx10000\intbl A\cell B\cell\row}";
        let rows = parse_rtf_tables(rtf);
        assert_eq!(rows.len(), 1, "rows: {:?}", rows);
        assert_eq!(rows[0].cells.len(), 2);
        assert_eq!(rows[0].cells[0].text, "A");
        assert_eq!(rows[0].cells[1].text, "B");
        // first cell got \clcbpat1 -> red bg
        assert_eq!(rows[0].cells[0].style.bg, "#ff0000");
    }

    #[test]
    fn self_diff_all_equal() {
        let rtf = r"{\rtf1\trowd\cellx5000\cellx10000\intbl Name\cell Age\cell\row\trowd\cellx5000\cellx10000\intbl Alice\cell 30\cell\row}";
        // write to temp and diff against itself
        let dir = std::env::temp_dir();
        let p = dir.join(format!("rtf-self-{}.rtf", std::process::id()));
        std::fs::write(&p, rtf).unwrap();
        let ps = p.to_string_lossy();
        let r = rtf_diff(&ps, &ps).unwrap();
        assert_eq!(r.added + r.modified + r.removed, 0, "self diff not clean: {:?}", (r.added, r.modified, r.removed));
        assert_eq!(r.rows.len(), 2);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn modified_cell_detected() {
        // A row with several cells where only one changes -> similarity stays
        // high so the pair is recognized as "modified", and the cell is tagged.
        let a = r"{\rtf1\trowd\cellx3\cellx6\cellx9\intbl ID\cell Alice\cell old\cell\row}";
        let b = r"{\rtf1\trowd\cellx3\cellx6\cellx9\intbl ID\cell Alice\cell new\cell\row}";
        let dir = std::env::temp_dir();
        let pa = dir.join(format!("rtf-a-{}.rtf", std::process::id()));
        let pb = dir.join(format!("rtf-b-{}.rtf", std::process::id()));
        std::fs::write(&pa, a).unwrap();
        std::fs::write(&pb, b).unwrap();
        let r = rtf_diff(&pa.to_string_lossy(), &pb.to_string_lossy()).unwrap();
        assert_eq!(r.modified, 1, "rows: {:?}", r.rows);
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }
}
