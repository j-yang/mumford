//! Inline alignment: turn raw delete/insert edit scripts into Replace ops with
//! word-level inline highlights.
//!
//! A line-diff engine (Myers, patience, LCS, Hirschberg — any that emits
//! `Equal | Delete | Insert` ops) gives you a raw edit script. The default
//! rendering of a line that was edited from `foo bar` to `foo baz` is a
//! delete-then-insert pair — visually noisy. [`pair_replacements`] walks the
//! script and merges adjacent delete/insert pairs that are similar enough
//! into a single [`Op`] with `typ = Replace` carrying inline segments
//! (`old_segs` / `new_segs`), so your UI can highlight only the changed word.
//!
//! [`inline_segments`] does the same word-level alignment for one pair of lines
//! in isolation, usable on its own (e.g. a structured UI that already knows
//! which two cells to compare). Word-level diffing uses a small internal LCS
//! DP over alphanumeric tokens — bounded and tiny for single lines, so no
//! patience anchoring or Hirschberg reduction is needed.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::lcs::{char_similarity, lcs_diff, tokenize};

/// Op type emitted by a line-diff engine. `Replace` is never produced by raw
/// line-diff; it appears only after [`pair_replacements`] post-processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum OpType {
    Equal,
    Delete,
    Insert,
    Replace,
}

/// One edit operation between two line sequences.
///
/// `a` / `b` are 1-based indices into the old/new source slices (0 = absent on
/// that side, for Insert / Delete). `old` / `new` carry the source text; for
/// `Equal` they are equal, for `Delete` `new` is empty, for `Insert` `old` is
/// empty. `old_segs` / `new_segs` are populated only for [`OpType::Replace`].
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Op {
    #[cfg_attr(feature = "serde", serde(rename = "type"))]
    pub typ: OpType,
    pub a: usize,
    pub b: usize,
    pub old: String,
    pub new: String,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
    pub old_segs: Vec<Seg>,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
    pub new_segs: Vec<Seg>,
}

/// A word/token-level inline segment with its "changed vs the other side" flag.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Seg {
    pub text: String,
    pub changed: bool,
}

/// Default similarity threshold: lines with at least this fraction of shared
/// characters are paired into a `Replace` row instead of being shown as a
/// separate delete + insert. 0.5 means "at least half of the characters are
/// shared between the two lines".
pub const DEFAULT_SIMILARITY: f64 = 0.5;

impl Op {
    /// Construct an `Equal` op: line `a` in the old sequence matches line `b`
    /// in the new.
    pub fn equal(a: usize, b: usize, old: &str, new: &str) -> Self {
        Op {
            typ: OpType::Equal,
            a,
            b,
            old: old.to_string(),
            new: new.to_string(),
            old_segs: Vec::new(),
            new_segs: Vec::new(),
        }
    }

    /// Construct an `Insert` op: line `b` was added to the new sequence.
    pub fn insert(b: usize, new: &str) -> Self {
        Op {
            typ: OpType::Insert,
            a: 0,
            b,
            old: String::new(),
            new: new.to_string(),
            old_segs: Vec::new(),
            new_segs: Vec::new(),
        }
    }

    /// Construct a `Delete` op: line `a` was removed from the old sequence.
    pub fn delete(a: usize, old: &str) -> Self {
        Op {
            typ: OpType::Delete,
            a,
            b: 0,
            old: old.to_string(),
            new: String::new(),
            old_segs: Vec::new(),
            new_segs: Vec::new(),
        }
    }

    /// Construct a `Replace` op from a paired delete+insert with precomputed
    /// word-level inline segments.
    pub fn replace(
        a: usize,
        b: usize,
        old: &str,
        new: &str,
        old_segs: Vec<Seg>,
        new_segs: Vec<Seg>,
    ) -> Self {
        Op {
            typ: OpType::Replace,
            a,
            b,
            old: old.to_string(),
            new: new.to_string(),
            old_segs,
            new_segs,
        }
    }
}

/// Diff statistics derived from an edit script.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Stats {
    #[cfg_attr(feature = "serde", serde(rename = "additions"))]
    pub additions: usize,
    #[cfg_attr(feature = "serde", serde(rename = "deletions"))]
    pub deletions: usize,
    pub modified: usize,
    pub unchanged: usize,
}

impl Stats {
    /// `true` when the diff contains no changes.
    pub fn is_clean(&self) -> bool {
        self.additions + self.deletions + self.modified == 0
    }

    /// Total number of changed lines (additions + deletions + modified).
    pub fn changed(&self) -> usize {
        self.additions + self.deletions + self.modified
    }
}

/// Count additions, deletions, modifications, and unchanged lines in an edit
/// script.
///
/// ```
/// use mumford::lines::diff;
/// use mumford::inline::stats;
///
/// let ops = diff(&["a", "b", "c"], &["a", "x", "c"]);
/// let s = stats(&ops);
/// assert_eq!(s.changed(), 2); // 1 delete + 1 insert
/// ```
pub fn stats(ops: &[Op]) -> Stats {
    let mut s = Stats::default();
    for op in ops {
        match op.typ {
            OpType::Equal => s.unchanged += 1,
            OpType::Insert => s.additions += 1,
            OpType::Delete => s.deletions += 1,
            OpType::Replace => s.modified += 1,
        }
    }
    s
}

/// Pair consecutive delete+insert blocks in `ops` into `Replace` ops.
///
/// `ops` should come from a line-diff engine that only emits `Equal | Delete |
/// Insert`; any `Replace` ops already present are passed through untouched.
/// Within each maximal block of deletes followed by inserts, lines are paired
/// positionally (1st-with-1st, …); each pair whose similarity is at or above
/// `threshold` is collapsed into a `Replace` carrying word-level segments,
/// the rest stay separate `Delete` + `Insert` ops.
pub fn pair_replacements(ops: Vec<Op>, threshold: f64) -> Vec<Op> {
    let mut out: Vec<Op> = Vec::with_capacity(ops.len());
    let mut i = 0;
    while i < ops.len() {
        if ops[i].typ == OpType::Equal || ops[i].typ == OpType::Replace {
            out.push(ops[i].clone());
            i += 1;
            continue;
        }
        let block_start = i;
        while i < ops.len() && ops[i].typ == OpType::Delete {
            i += 1;
        }
        let dels = &ops[block_start..i];
        let ins_start = i;
        while i < ops.len() && ops[i].typ == OpType::Insert {
            i += 1;
        }
        let inss = &ops[ins_start..i];

        let pairs = dels.len().min(inss.len());
        for k in 0..pairs {
            let d = &dels[k];
            let s = &inss[k];
            if let Some((old_segs, new_segs)) = inline_segments(&d.old, &s.new, threshold) {
                out.push(Op::replace(d.a, s.b, &d.old, &s.new, old_segs, new_segs));
            } else {
                out.push(d.clone());
                out.push(s.clone());
            }
        }
        for d in &dels[pairs..] {
            out.push(d.clone());
        }
        for s in &inss[pairs..] {
            out.push(s.clone());
        }
    }
    out
}

/// Compute word-level inline highlight segments for two lines, returning
/// `(old_segs, new_segs)`. Returns `None` when the lines are below `threshold`
/// similar — caller should show them as a delete + insert rather than a Replace.
///
/// Tokenization splits on alphanumeric runs, so `Section A.1 ... 17` and
/// `Section A.1 ... 18` produce segments where only `17` / `18` are flagged
/// `changed = true`. Unchanged tokens scattered across the line stay
/// un-highlighted.
pub fn inline_segments(a: &str, b: &str, threshold: f64) -> Option<(Vec<Seg>, Vec<Seg>)> {
    let ta = tokenize(a);
    let tb = tokenize(b);
    let ops = lcs_diff(&ta, &tb);

    let mut equal_chars = 0usize;
    for op in &ops {
        if op.typ == OpType::Equal {
            equal_chars += op.old.chars().count();
        }
    }
    let similarity = char_similarity(equal_chars, a.chars().count(), b.chars().count());
    if similarity < threshold {
        return None;
    }

    let mut old_segs: Vec<Seg> = Vec::new();
    let mut new_segs: Vec<Seg> = Vec::new();
    let push = |segs: &mut Vec<Seg>, text: &str, changed: bool| {
        if text.is_empty() {
            return;
        }
        if let Some(last) = segs.last_mut() {
            if last.changed == changed {
                last.text.push_str(text);
                return;
            }
        }
        segs.push(Seg { text: text.to_string(), changed });
    };
    for op in &ops {
        match op.typ {
            OpType::Equal => {
                push(&mut old_segs, &op.old, false);
                push(&mut new_segs, &op.new, false);
            }
            OpType::Delete => push(&mut old_segs, &op.old, true),
            OpType::Insert => push(&mut new_segs, &op.new, true),
            OpType::Replace => {}
        }
    }
    Some((old_segs, new_segs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_number_change_pairs_as_replace() {
        let ops = vec![
            Op::delete(1, "Section A.1 Overview .... 17"),
            Op::insert(1, "Section A.1 Overview .... 18"),
        ];
        let out = pair_replacements(ops, DEFAULT_SIMILARITY);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].typ, OpType::Replace);
        let old_chg: String = out[0].old_segs.iter().filter(|s| s.changed).map(|s| s.text.clone()).collect();
        let new_chg: String = out[0].new_segs.iter().filter(|s| s.changed).map(|s| s.text.clone()).collect();
        assert_eq!(old_chg, "17");
        assert_eq!(new_chg, "18");
    }

    #[test]
    fn unrelated_lines_stay_separate() {
        let ops = vec![
            Op::delete(1, "the quick brown fox"),
            Op::insert(1, "completely different text here"),
        ];
        let out = pair_replacements(ops, DEFAULT_SIMILARITY);
        assert!(out.iter().all(|o| o.typ != OpType::Replace));
        assert!(out.iter().any(|o| o.typ == OpType::Delete));
        assert!(out.iter().any(|o| o.typ == OpType::Insert));
    }

    #[test]
    fn inline_only_marks_changed_word() {
        let (old_segs, new_segs) =
            inline_segments("foo bar", "foo baz", DEFAULT_SIMILARITY).expect("similar enough");
        let old_changed: String = old_segs.iter().filter(|s| s.changed).map(|s| s.text.clone()).collect();
        let new_changed: String = new_segs.iter().filter(|s| s.changed).map(|s| s.text.clone()).collect();
        assert_eq!(old_changed, "bar");
        assert_eq!(new_changed, "baz");
    }

    #[test]
    fn scattered_number_changes_word_level() {
        let a = "ROW01 12 0.0617 0.020 0.0400 0.075";
        let b = "ROW01 15 0.0580 0.020 0.0400 0.075";
        let (old_segs, new_segs) = inline_segments(a, b, DEFAULT_SIMILARITY).expect("similar enough");
        assert_eq!(old_segs.len(), new_segs.len());
        let new_changed: Vec<String> = new_segs
            .iter()
            .filter(|s| s.changed)
            .map(|s| s.text.trim().to_string())
            .collect();
        assert!(new_changed.iter().any(|s| s.contains("15")));
        assert!(new_changed.iter().any(|s| s.contains("0580")));
        let unchanged: String = new_segs.iter().filter(|s| !s.changed).map(|s| s.text.clone()).collect();
        assert!(unchanged.contains("0.0400"));
    }

    #[test]
    fn unequal_block_pairs_leftovers_pass_through() {
        let ops = vec![
            Op::delete(1, "gone"),
            Op::delete(2, "also gone"),
            Op::insert(1, "new"),
        ];
        let out = pair_replacements(ops, DEFAULT_SIMILARITY);
        assert!(out.iter().any(|o| o.typ == OpType::Delete));
        assert!(out.iter().any(|o| o.typ == OpType::Insert));
        assert!(out.iter().all(|o| o.typ != OpType::Replace));
    }

    #[test]
    fn equal_ops_pass_through() {
        let ops = vec![
            Op::equal(1, 1, "same", "same"),
            Op::delete(2, "removed"),
            Op::insert(2, "added"),
            Op::equal(3, 3, "tail", "tail"),
        ];
        let out = pair_replacements(ops, DEFAULT_SIMILARITY);
        assert_eq!(out[0].typ, OpType::Equal);
        assert_eq!(out.last().unwrap().typ, OpType::Equal);
    }

    #[test]
    fn empty_inputs() {
        let out = pair_replacements(Vec::new(), DEFAULT_SIMILARITY);
        assert!(out.is_empty());
        assert!(inline_segments("", "", DEFAULT_SIMILARITY).is_none()
            || inline_segments("", "", DEFAULT_SIMILARITY).map(|(a, b)| a.is_empty() && b.is_empty()).unwrap_or(false));
    }
}