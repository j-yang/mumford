//! Line-level diff engine: patience anchors split the input into small
//! segments solved by exact LCS, with a linear-space Hirschberg fallback for
//! segments too large for the full matrix, and a block-replacement bailout
//! for pathological inputs that would otherwise freeze the UI.
//!
//! This is the "heavy" line diff — use it for comparing documents (text files,
//! extracted PDF/DOCX paragraphs, code listings). For word-level inline
//! highlighting within a single pair of lines, [`mumford::inline::inline_segments`]
//! uses a lighter private LCS that is sufficient for bounded single-line inputs.
//!
//! Algorithm pipeline:
//! 1. Strip common prefix/suffix (cheap, common case).
//! 2. Find patience anchors — lines unique in both sides — and take their LIS
//!    to split the problem into independent segments.
//! 3. Each anchor-free segment is solved by exact LCS (≤ 4M cells), Hirschberg
//!    (≤ 16M cells), or block replacement (> 16M cells, correct but not minimal).
//!
//! The output is a flat `Vec<Op>` of `Equal | Delete | Insert`. `Replace` is
//! never emitted here — feed the result through [`mumford::inline::pair_replacements`]
//! to merge adjacent delete+insert pairs into `Replace` rows with word-level
//! inline segments.

use crate::inline::Op;

/// Max cells for the exact full-matrix LCS. Larger segments fall back to
/// linear-space Hirschberg so memory stays bounded.
const MAX_LCS_CELLS: usize = 4 << 20;

/// Hirschberg is O(n*m) in time even though it's linear in space. Above this
/// cell count we stop computing an optimal LCS and emit a straight block
/// replacement (delete all of A, insert all of B) — the result is still
/// correct, just not minimal. 16M cells ≈ a 4000×4000 segment.
const MAX_HIRSCHBERG_CELLS: usize = 16 << 20;

/// Diff two line sequences and return an edit script.
///
/// Accepts any slice of `AsRef<str>` — `&[String]`, `&[&str]`, `&[Cow<str>]`,
/// etc. — so callers can diff borrowed or owned data without conversion.
///
/// Only emits `Equal | Delete | Insert` — never `Replace`. To get `Replace`
/// rows with inline word-level highlights, pass the result through
/// [`mumford::inline::pair_replacements`].
///
/// ```
/// use mumford::lines::diff;
/// use mumford::inline::{pair_replacements, OpType, DEFAULT_SIMILARITY};
///
/// let a: Vec<String> = vec!["foo".into(), "bar baz".into(), "qux".into()];
/// let b: Vec<String> = vec!["foo".into(), "bar qux".into(), "qux".into()];
/// let ops = diff(&a, &b);
/// let paired = pair_replacements(ops, DEFAULT_SIMILARITY);
/// assert_eq!(paired[1].typ, OpType::Replace);
/// ```
pub fn diff<A: AsRef<str>, B: AsRef<str>>(a: &[A], b: &[B]) -> Vec<Op> {
    let a: Vec<&str> = a.iter().map(AsRef::as_ref).collect();
    let b: Vec<&str> = b.iter().map(AsRef::as_ref).collect();
    let mut out: Vec<Op> = Vec::with_capacity(a.len() + b.len());
    diff_into(&a, &b, 0, 0, &mut out);
    out
}

fn diff_into(a: &[&str], b: &[&str], mut off_a: usize, mut off_b: usize, out: &mut Vec<Op>) {
    let mut p = 0;
    while p < a.len() && p < b.len() && a[p] == b[p] {
        out.push(Op::equal(off_a + p, off_b + p, a[p], b[p]));
        p += 1;
    }
    off_a += p;
    off_b += p;
    let a = &a[p..];
    let b = &b[p..];

    let mut s = 0;
    while s < a.len() && s < b.len() && a[a.len() - 1 - s] == b[b.len() - 1 - s] {
        s += 1;
    }
    let a_mid = &a[..a.len() - s];
    let b_mid = &b[..b.len() - s];

    diff_middle(a_mid, b_mid, off_a, off_b, out);

    for t in 0..s {
        let ai = a_mid.len() + t;
        let bi = b_mid.len() + t;
        out.push(Op::equal(off_a + ai, off_b + bi, a[ai], b[bi]));
    }
}

fn diff_middle(a: &[&str], b: &[&str], off_a: usize, off_b: usize, out: &mut Vec<Op>) {
    if a.is_empty() {
        for (j, bv) in b.iter().copied().enumerate() {
            out.push(Op::insert(off_b + j, bv));
        }
        return;
    }
    if b.is_empty() {
        for (i, av) in a.iter().copied().enumerate() {
            out.push(Op::delete(off_a + i, av));
        }
        return;
    }

    let anchors = patience_anchors(a, b);
    if anchors.is_empty() {
        solve_exact(a, b, off_a, off_b, out);
        return;
    }

    let (mut prev_a, mut prev_b) = (0usize, 0usize);
    for an in &anchors {
        diff_into(&a[prev_a..an.a], &b[prev_b..an.b], off_a + prev_a, off_b + prev_b, out);
        out.push(Op::equal(off_a + an.a, off_b + an.b, a[an.a], b[an.b]));
        prev_a = an.a + 1;
        prev_b = an.b + 1;
    }
    diff_into(&a[prev_a..], &b[prev_b..], off_a + prev_a, off_b + prev_b, out);
}

fn solve_exact(a: &[&str], b: &[&str], off_a: usize, off_b: usize, out: &mut Vec<Op>) {
    let cells = a.len().saturating_mul(b.len());
    if cells <= MAX_LCS_CELLS {
        lcs_full(a, b, off_a, off_b, out);
    } else if cells <= MAX_HIRSCHBERG_CELLS {
        hirschberg(a, b, off_a, off_b, out);
    } else {
        for (i, av) in a.iter().copied().enumerate() {
            out.push(Op::delete(off_a + i, av));
        }
        for (j, bv) in b.iter().copied().enumerate() {
            out.push(Op::insert(off_b + j, bv));
        }
    }
}

#[derive(Clone, Copy)]
struct AnchorPair {
    a: usize,
    b: usize,
}

fn patience_anchors(a: &[&str], b: &[&str]) -> Vec<AnchorPair> {
    use std::collections::HashMap;
    let mut count_a: HashMap<&str, i32> = HashMap::with_capacity(a.len());
    for x in a {
        *count_a.entry(*x).or_insert(0) += 1;
    }
    let mut count_b: HashMap<&str, i32> = HashMap::with_capacity(b.len());
    for x in b {
        *count_b.entry(*x).or_insert(0) += 1;
    }
    let mut pos_b: HashMap<&str, usize> = HashMap::with_capacity(b.len());
    for (j, x) in b.iter().enumerate() {
        if *count_b.get(*x).unwrap_or(&0) == 1 {
            pos_b.insert(*x, j);
        }
    }

    let mut seq: Vec<AnchorPair> = Vec::new();
    for (i, x) in a.iter().enumerate() {
        if *count_a.get(*x).unwrap_or(&0) != 1 {
            continue;
        }
        if let Some(&j) = pos_b.get(*x) {
            seq.push(AnchorPair { a: i, b: j });
        }
    }
    if seq.is_empty() {
        return Vec::new();
    }

    let mut piles: Vec<usize> = Vec::with_capacity(seq.len());
    let mut prev: Vec<isize> = vec![-1; seq.len()];
    for i in 0..seq.len() {
        let (mut lo, mut hi) = (0usize, piles.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if seq[piles[mid]].b < seq[i].b {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        prev[i] = if lo > 0 { piles[lo - 1] as isize } else { -1 };
        if lo == piles.len() {
            piles.push(i);
        } else {
            piles[lo] = i;
        }
    }

    let mut out: Vec<AnchorPair> = Vec::with_capacity(piles.len());
    let mut k = *piles.last().unwrap() as isize;
    while k >= 0 {
        out.push(seq[k as usize]);
        k = prev[k as usize];
    }
    out.reverse();
    out
}

fn lcs_full(a: &[&str], b: &[&str], off_a: usize, off_b: usize, out: &mut Vec<Op>) {
    let (n, m) = (a.len(), b.len());
    let stride = m + 1;
    let mut dp = vec![0i32; (n + 1) * stride];
    for i in 1..=n {
        let ai = a[i - 1];
        let (prev_part, cur_part) = dp.split_at_mut(i * stride);
        let prev_row = &prev_part[(i - 1) * stride..(i - 1) * stride + stride];
        let row = &mut cur_part[..stride];
        for j in 1..=m {
            if ai == b[j - 1] {
                row[j] = prev_row[j - 1] + 1;
            } else if prev_row[j] >= row[j - 1] {
                row[j] = prev_row[j];
            } else {
                row[j] = row[j - 1];
            }
        }
    }

    let mut tmp: Vec<Op> = Vec::with_capacity(n + m);
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            tmp.push(Op::equal(off_a + i - 1, off_b + j - 1, a[i - 1], b[j - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i * stride + j - 1] >= dp[(i - 1) * stride + j]) {
            tmp.push(Op::insert(off_b + j - 1, b[j - 1]));
            j -= 1;
        } else {
            tmp.push(Op::delete(off_a + i - 1, a[i - 1]));
            i -= 1;
        }
    }
    out.extend(tmp.into_iter().rev());
}

fn hirschberg(a: &[&str], b: &[&str], off_a: usize, off_b: usize, out: &mut Vec<Op>) {
    if a.is_empty() {
        for (j, bv) in b.iter().copied().enumerate() {
            out.push(Op::insert(off_b + j, bv));
        }
        return;
    }
    if b.is_empty() {
        for (i, av) in a.iter().copied().enumerate() {
            out.push(Op::delete(off_a + i, av));
        }
        return;
    }
    if a.len() == 1 {
        let mut idx: isize = -1;
        for (j, bv) in b.iter().enumerate() {
            if *bv == a[0] {
                idx = j as isize;
                break;
            }
        }
        if idx < 0 {
            out.push(Op::delete(off_a, a[0]));
            for (j, bv) in b.iter().copied().enumerate() {
                out.push(Op::insert(off_b + j, bv));
            }
            return;
        }
        let idx = idx as usize;
        for (j, bv) in b[..idx].iter().copied().enumerate() {
            out.push(Op::insert(off_b + j, bv));
        }
        out.push(Op::equal(off_a, off_b + idx, a[0], b[idx]));
        for (j, bv) in b[idx + 1..].iter().copied().enumerate() {
            out.push(Op::insert(off_b + idx + 1 + j, bv));
        }
        return;
    }

    let mid = a.len() / 2;
    let score_l = lcs_row(&a[..mid], b, false);
    let score_r = lcs_row(&a[mid..], b, true);

    let mut best: i32 = -1;
    let mut best_k = 0usize;
    for k in 0..=b.len() {
        let s = score_l[k] + score_r[b.len() - k];
        if s > best {
            best = s;
            best_k = k;
        }
    }

    hirschberg(&a[..mid], &b[..best_k], off_a, off_b, out);
    hirschberg(&a[mid..], &b[best_k..], off_a + mid, off_b + best_k, out);
}

fn lcs_row(a: &[&str], b: &[&str], rev: bool) -> Vec<i32> {
    let mut prev = vec![0i32; b.len() + 1];
    let mut cur = vec![0i32; b.len() + 1];
    let at = |s: &[&str], i: usize| -> usize {
        if rev { s.len() - 1 - i } else { i }
    };
    for i in 0..a.len() {
        let ai = a[at(a, i)];
        for j in 1..=b.len() {
            let bj = b[at(b, j - 1)];
            if ai == bj {
                cur[j] = prev[j - 1] + 1;
            } else if prev[j] >= cur[j - 1] {
                cur[j] = prev[j];
            } else {
                cur[j] = cur[j - 1];
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inline::OpType;

    fn sv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn validate(a: &[String], b: &[String], ops: &[Op]) {
        let mut got_a: Vec<String> = Vec::new();
        let mut got_b: Vec<String> = Vec::new();
        for op in ops {
            match op.typ {
                OpType::Equal => {
                    got_a.push(op.old.clone());
                    got_b.push(op.new.clone());
                }
                OpType::Delete => got_a.push(op.old.clone()),
                OpType::Insert => got_b.push(op.new.clone()),
                OpType::Replace => {
                    got_a.push(op.old.clone());
                    got_b.push(op.new.clone());
                }
            }
        }
        assert_eq!(got_a, a, "reconstructed old mismatch");
        assert_eq!(got_b, b, "reconstructed new mismatch");
    }

    fn lcs_len(ops: &[Op]) -> usize {
        ops.iter().filter(|o| o.typ == OpType::Equal).count()
    }

    #[test]
    fn diff_basic() {
        let cases: Vec<(Vec<String>, Vec<String>)> = vec![
            (sv(&[]), sv(&[])),
            (sv(&["a"]), sv(&[])),
            (sv(&[]), sv(&["a"])),
            (sv(&["a", "b", "c"]), sv(&["a", "b", "c"])),
            (sv(&["a", "b", "c"]), sv(&["a", "x", "c"])),
            (sv(&["a", "b", "c", "d"]), sv(&["a", "c"])),
            (sv(&["a", "c"]), sv(&["a", "b", "c", "d"])),
            (sv(&["x", "a", "b", "c"]), sv(&["a", "b", "c"])),
        ];
        for (a, b) in &cases {
            let ops = diff(a, b);
            validate(a, b, &ops);
        }
    }

    #[test]
    fn diff_accepts_str_slices() {
        let ops = diff(&["a", "b", "c"], &["a", "x", "c"]);
        assert_eq!(ops.len(), 4); // Equal, Delete, Insert, Equal
        assert_eq!(ops[0].typ, OpType::Equal);
        assert_eq!(ops[1].typ, OpType::Delete);
        assert_eq!(ops[2].typ, OpType::Insert);
    }

    #[test]
    fn diff_matches_full_lcs() {
        let mk = |n: usize, md: usize| -> Vec<String> {
            (0..n).map(|i| format!("line-{}", i % md)).collect()
        };
        for (n, m, md) in [(50usize, 40usize, 13usize), (100, 100, 7), (200, 150, 200)] {
            let a = mk(n, md);
            let b = mk(m, md * 2);
            let ops = diff(&a, &b);
            validate(&a, &b, &ops);
            let a_refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
            let b_refs: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
            let mut reference: Vec<Op> = Vec::new();
            lcs_full(&a_refs, &b_refs, 0, 0, &mut reference);
            assert_eq!(lcs_len(&ops), lcs_len(&reference), "LCS len mismatch n={n} m={m} mod={md}");
        }
    }

    #[test]
    fn diff_large_no_oom() {
        let n = 300_000;
        let mut a: Vec<String> = Vec::with_capacity(n);
        for i in 0..n {
            a.push(format!("row {i} content here"));
        }
        let mut b = a.clone();
        let mut i = 1000;
        while i < n {
            b[i] = format!("CHANGED {i}");
            i += 5000;
        }
        let ops = diff(&a, &b);
        validate(&a, &b, &ops);
        assert!(lcs_len(&ops) > 0, "expected large equal run");
    }

    #[test]
    fn hirschberg_direct() {
        let a = sv(&["q", "w", "e", "r", "t"]);
        let b = sv(&["w", "r", "t", "z"]);
        let a_refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
        let b_refs: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
        let mut out: Vec<Op> = Vec::new();
        hirschberg(&a_refs, &b_refs, 0, 0, &mut out);
        validate(&a, &b, &out);
    }

    #[test]
    fn diff_all_different_is_fast_and_valid() {
        let n = 20_000;
        let a: Vec<String> = (0..n).map(|i| format!("alpha line {i} aaaa")).collect();
        let b: Vec<String> = (0..n).map(|i| format!("BETA line {i} bbbb")).collect();
        let ops = diff(&a, &b);
        validate(&a, &b, &ops);
    }

    #[test]
    fn diff_with_pair_replacements_end_to_end() {
        let a: Vec<String> = sv(&["Section A.1 .... 17", "foo bar", "tail"]);
        let b: Vec<String> = sv(&["Section A.1 .... 18", "foo baz", "tail"]);
        let ops = diff(&a, &b);
        let paired = crate::inline::pair_replacements(ops, crate::inline::DEFAULT_SIMILARITY);
        assert_eq!(paired[0].typ, OpType::Replace);
        assert_eq!(paired[1].typ, OpType::Replace);
        assert_eq!(paired[2].typ, OpType::Equal);
    }
}
