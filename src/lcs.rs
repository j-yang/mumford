//! Internal LCS diff shared by [`mumford::inline`] and [`crate::grid`].
//!
//! A small O(n*m) dynamic-programming LCS over string slices. Both call sites
//! feed it bounded inputs (word tokens of one line, row signatures, header
//! cells), so the full DP table is fine — no patience anchoring, Hirschberg,
//! or bailout is needed here. The heavy line-diff engine is the caller's
//! responsibility (upstream of tate); this module is a private word/row level
//! utility only.

use crate::inline::Op;

/// Diff two string slices and return the edit script as `Vec<Op>`.
///
/// Only emits `Equal | Delete | Insert` — never `Replace`. Callers that want
/// delete+insert pairs merged into `Replace` should post-process with
/// [`mumford::inline::pair_replacements`].
pub(crate) fn lcs_diff(a: &[String], b: &[String]) -> Vec<Op> {
    let n = a.len();
    let m = b.len();
    if n == 0 {
        return b.iter().enumerate().map(|(j, s)| Op::insert(j + 1, s)).collect();
    }
    if m == 0 {
        return a.iter().enumerate().map(|(i, s)| Op::delete(i + 1, s)).collect();
    }
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            if a[i - 1] == b[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }
    let mut ops: Vec<Op> = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            ops.push(Op::equal(i, j, &a[i - 1], &b[j - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            ops.push(Op::insert(j, &b[j - 1]));
            j -= 1;
        } else {
            ops.push(Op::delete(i, &a[i - 1]));
            i -= 1;
        }
    }
    ops.reverse();
    ops
}

/// Tokenize a string into alternating alphanumeric / non-alphanumeric runs so
/// the token stream reconstructs the line exactly. Each token is one diff unit,
/// giving word-granularity inline highlights.
pub(crate) fn tokenize(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_alnum: Option<bool> = None;
    for ch in s.chars() {
        let is_alnum = ch.is_alphanumeric();
        match cur_alnum {
            Some(prev) if prev == is_alnum => cur.push(ch),
            _ => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                cur.push(ch);
                cur_alnum = Some(is_alnum);
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Sørensen–Dice character similarity: 2×shared-equal-chars over total chars.
/// Used by [`mumford::inline::inline_segments`] to gate whether two lines are
/// similar enough to be worth pairing.
pub(crate) fn char_similarity(equal_chars: usize, a_len: usize, b_len: usize) -> f64 {
    let denom = (a_len + b_len).max(1) as f64;
    (2 * equal_chars) as f64 / denom
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inline::OpType;

    fn sv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn lcs_basic() {
        let ops = lcs_diff(&sv(&["a", "b", "c"]), &sv(&["a", "x", "c"]));
        let tags: Vec<OpType> = ops.iter().map(|o| o.typ).collect();
        assert_eq!(tags, vec![OpType::Equal, OpType::Delete, OpType::Insert, OpType::Equal]);
    }

    #[test]
    fn lcs_empty_a() {
        let ops = lcs_diff(&[], &sv(&["x", "y"]));
        assert_eq!(ops.len(), 2);
        assert!(ops.iter().all(|o| o.typ == OpType::Insert));
    }

    #[test]
    fn lcs_empty_b() {
        let ops = lcs_diff(&sv(&["x", "y"]), &[]);
        assert_eq!(ops.len(), 2);
        assert!(ops.iter().all(|o| o.typ == OpType::Delete));
    }

    #[test]
    fn lcs_both_empty() {
        let ops = lcs_diff(&[], &[]);
        assert!(ops.is_empty());
    }

    #[test]
    fn lcs_identical() {
        let ops = lcs_diff(&sv(&["a", "b"]), &sv(&["a", "b"]));
        assert!(ops.iter().all(|o| o.typ == OpType::Equal));
    }

    #[test]
    fn tokenize_alphanumeric_runs() {
        let toks = tokenize("foo bar123 baz");
        assert_eq!(toks, vec!["foo", " ", "bar123", " ", "baz"]);
    }

    #[test]
    fn tokenize_reconstructs_input() {
        let s = "Section A.1 Overview .... 17";
        let toks = tokenize(s);
        assert_eq!(toks.join(""), s);
    }

    #[test]
    fn similarity_identical() {
        assert!((char_similarity(5, 5, 5) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn similarity_half() {
        // 2 * 5 / (10 + 10) = 0.5
        assert!((char_similarity(5, 10, 10) - 0.5).abs() < f64::EPSILON);
    }
}