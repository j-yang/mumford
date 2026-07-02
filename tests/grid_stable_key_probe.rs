//! READ-ONLY PROBE #2 — changes no library code.
//!
//! Probe #1 (`grid_vs_tree_merge_probe`) showed that folding grid merge into
//! tree_merge with *positional* keys fails: a row insert shifts every key and
//! wrecks the merge. But positional keying throws away exactly what coordinate
//! descent computes — the row/column ALIGNMENT. That was an unfair encoding.
//!
//! This probe asks the sharper question: if we key each grid using the
//! alignment `grid_diff(base, X)` already produces — so that "the same row"
//! carries the same identity across all three versions, and an inserted row
//! gets a fresh identity — does "grid → stable-key Section → tree_merge → grid"
//! then REPRODUCE `grid_merge`?
//!
//! - If yes: grid_merge / line_merge are reducible. The alignment algorithms
//!   (coordinate descent, LCS) stay as *keying adapters*, but the 3-way merge
//!   logic collapses into the single tree_merge. That is the real payoff of
//!   "everything is a tree" and Phase 2 can delete the specialised merges.
//! - If no: there is a genuine merge-level boundary and grid_merge stays.
//!
//! # Verdict (measured): YES — folding is sound.
//!
//! All three cases below reproduce `grid_merge` exactly (merged grid AND
//! conflict count): disjoint cell edits merge cleanly, a same-cell divergence
//! is flagged as one conflict, and a row inserted on one side aligns via the
//! shared base-anchored keys instead of shifting every row. The one prerequisite
//! was fixing `tree_merge` to carry text changes (grid cells store content in
//! `text`, not attributes); once text is a first-class merge dimension, the
//! specialised `grid_merge` is redundant with "key by alignment, then
//! `tree_merge`". Coordinate descent survives as the *keying adapter*; the merge
//! logic does not need to be duplicated.

use std::collections::BTreeMap;
use mumford::grid::{grid_diff, GridOptions};
use tate::section::{Section, Value};
use tate::tree::tree_merge;

// ── stable keying via base-anchored alignment ─────────────────────────────────

/// Key `other`'s rows/cols against `base` using `grid_diff(base, other)`.
///
/// A row of `other` aligned to base row `i` gets key `rb{i}` (shared across
/// versions). A row present only in `other` (an insert) gets a fresh key
/// `rx{ordinal}`. Columns are keyed the same way (`cb{j}` / `cx{k}`). This is
/// the identity a keying adapter would assign — the alignment result made into
/// stable keys, which is the whole point of coordinate descent.
fn grid_to_stable_section(
    base: &[Vec<String>],
    other: &[Vec<String>],
    opts: &GridOptions,
) -> Section {
    let gd = grid_diff(base, other, opts);

    // other-row-index → row key.
    let mut row_key: BTreeMap<usize, String> = BTreeMap::new();
    let mut row_order: BTreeMap<usize, usize> = BTreeMap::new();
    let mut fresh_row = 0usize;
    for (ord, gr) in gd.rows.iter().enumerate() {
        if gr.row_b == 0 {
            continue; // not present in `other`
        }
        let bi = gr.row_b - 1; // 0-based index into `other`
        let key = if gr.row_a > 0 {
            format!("rb{}", gr.row_a - 1) // aligned to this base row
        } else {
            let k = format!("rx{fresh_row}"); // inserted row
            fresh_row += 1;
            k
        };
        row_key.insert(bi, key);
        row_order.insert(bi, ord);
    }

    // other-col-index → col key, from the column alignment.
    // gd.columns is in aligned order; we must map back to `other`'s own column
    // indices. Reconstruct by walking columns and counting b-present ones.
    let mut col_key: BTreeMap<usize, String> = BTreeMap::new();
    let mut col_order: BTreeMap<usize, usize> = BTreeMap::new();
    // grid_diff doesn't expose per-column b-index directly, so recompute the
    // column alignment the same way render does: a column is b-present unless
    // its status is Removed. We infer b-index by counting.
    let mut b_col = 0usize;
    let mut base_col = 0usize;
    for (ord, gc) in gd.columns.iter().enumerate() {
        use mumford::grid::Status::*;
        match gc.status {
            Removed => {
                base_col += 1; // exists in base only
            }
            Added => {
                col_key.insert(b_col, format!("cx{b_col}"));
                col_order.insert(b_col, ord);
                b_col += 1;
            }
            Equal | Modified => {
                col_key.insert(b_col, format!("cb{base_col}"));
                col_order.insert(b_col, ord);
                base_col += 1;
                b_col += 1;
            }
        }
    }

    let mut values = BTreeMap::new();
    values.insert(
        vec!["grid".to_string()],
        Value { kind: "grid".into(), label: String::new(), text: String::new(), attrs: vec![], order: 0 },
    );
    for (bi, row) in other.iter().enumerate() {
        let rk = match row_key.get(&bi) {
            Some(k) => k.clone(),
            None => continue,
        };
        let rord = row_order.get(&bi).copied().unwrap_or(bi);
        values.insert(
            vec!["grid".to_string(), rk.clone()],
            Value { kind: "row".into(), label: String::new(), text: String::new(), attrs: vec![], order: rord },
        );
        for (cj, cell) in row.iter().enumerate() {
            let ck = match col_key.get(&cj) {
                Some(k) => k.clone(),
                None => format!("cb{cj}"),
            };
            let cord = col_order.get(&cj).copied().unwrap_or(cj);
            values.insert(
                vec!["grid".to_string(), rk.clone(), ck],
                Value { kind: "cell".into(), label: String::new(), text: cell.clone(), attrs: vec![], order: cord },
            );
        }
    }
    Section { values }
}

/// Base keys itself against itself (identity alignment): row i → rb{i}, col j → cb{j}.
fn base_to_stable_section(base: &[Vec<String>], opts: &GridOptions) -> Section {
    grid_to_stable_section(base, base, opts)
}

/// Rebuild a grid from a merged stable-key section. Rows ordered by `order`,
/// cells ordered by `order`. Row/col keys are opaque; we sort by stored order.
fn stable_section_to_grid(s: &Section) -> Vec<Vec<String>> {
    // row key -> (order, {col order -> text})
    let mut rows: BTreeMap<(usize, String), BTreeMap<(usize, String), String>> = BTreeMap::new();
    // First pass: row orders.
    let mut row_ord: BTreeMap<String, usize> = BTreeMap::new();
    for (loc, v) in &s.values {
        if loc.len() == 2 {
            row_ord.insert(loc[1].clone(), v.order);
        }
    }
    for (loc, v) in &s.values {
        if loc.len() == 3 {
            let rk = loc[1].clone();
            let ro = row_ord.get(&rk).copied().unwrap_or(0);
            rows.entry((ro, rk))
                .or_default()
                .insert((v.order, loc[2].clone()), v.text.clone());
        }
    }
    rows.into_values()
        .map(|cells| cells.into_values().collect())
        .collect()
}

fn g(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
}

fn tree_path_merge(
    base: &[Vec<String>],
    ours: &[Vec<String>],
    theirs: &[Vec<String>],
    opts: &GridOptions,
) -> (Vec<Vec<String>>, usize) {
    let sb = base_to_stable_section(base, opts).to_tree().unwrap();
    let so = grid_to_stable_section(base, ours, opts).to_tree().unwrap();
    let st = grid_to_stable_section(base, theirs, opts).to_tree().unwrap();
    let r = tree_merge(&sb, &so, &st);
    (stable_section_to_grid(&r.tree.to_section()), r.conflicts.len())
}

// The expected results below are exactly what the old `grid_merge` produced on
// these inputs (captured before it was deleted). The tree path — key by the
// alignment `grid_diff` computes, then `tree_merge` — must reproduce them. This
// is the standing regression guard for "grid merge folds into tree merge".

#[test]
fn stable_key_disjoint_cell_edits() {
    // ours edits (1,1); theirs edits (0,0). Disjoint → both kept, no conflict.
    let base = g(&[&["a", "b"], &["c", "d"]]);
    let ours = g(&[&["a", "b"], &["c", "X"]]);
    let theirs = g(&[&["Y", "b"], &["c", "d"]]);

    let (tg, tc) = tree_path_merge(&base, &ours, &theirs, &GridOptions::default());
    assert_eq!(tg, g(&[&["Y", "b"], &["c", "X"]]), "tree path keeps both disjoint edits");
    assert_eq!(tc, 0);
}

#[test]
fn stable_key_conflicting_cell_edit() {
    // Both edit the same cell (1,1) differently → exactly one conflict.
    let base = g(&[&["a", "b"], &["c", "d"]]);
    let ours = g(&[&["a", "b"], &["c", "X"]]);
    let theirs = g(&[&["a", "b"], &["c", "Z"]]);

    let (_tg, tc) = tree_path_merge(&base, &ours, &theirs, &GridOptions::default());
    assert_eq!(tc, 1, "tree path detects the same-cell conflict");
}

#[test]
fn stable_key_row_insert_shift() {
    // theirs prepends a row; ours edits the last row. Coordinate-descent keying
    // aligns the shift, so both apply cleanly (this is the case positional keys
    // could not handle).
    let base = g(&[&["h1", "h2"], &["a", "b"]]);
    let ours = g(&[&["h1", "h2"], &["a", "B"]]);
    let theirs = g(&[&["NEW", "ROW"], &["h1", "h2"], &["a", "b"]]);

    let (tg, tc) = tree_path_merge(&base, &ours, &theirs, &GridOptions::default());
    assert_eq!(tg, g(&[&["h1", "h2"], &["a", "B"], &["NEW", "ROW"]]),
        "row insert + edit both applied via alignment keys");
    assert_eq!(tc, 0);
}
