//! Unified diff output: render an edit script as `git diff`-style text with
//! hunks and context lines.
//!
//! Typical usage:
//! ```
//! use mumford::lines::diff;
//! use mumford::unified::to_unified;
//!
//! let ops = diff(&["a", "b", "c", "d"], &["a", "x", "c", "d"]);
//! let text = to_unified(&ops, 3);
//! assert!(text.contains("@@ -1,4 +1,4 @@"));
//! ```

use crate::inline::{Op, OpType};

/// One line in a hunk: context, deleted, or inserted.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HunkLine {
    pub kind: LineKind,
    pub text: String,
}

/// Kind of line within a unified diff hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum LineKind {
    Context,
    Delete,
    Insert,
}

/// A hunk: a contiguous group of changes with surrounding context lines.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Hunk {
    /// 1-based start line in the old (A) sequence.
    pub old_start: usize,
    /// Number of old lines covered (0 for pure insertion).
    pub old_count: usize,
    /// 1-based start line in the new (B) sequence.
    pub new_start: usize,
    /// Number of new lines covered (0 for pure deletion).
    pub new_count: usize,
    pub lines: Vec<HunkLine>,
}

/// Split an edit script into hunks with `context` lines of surrounding context.
///
/// Adjacent change groups within `2 * context` lines of each other are merged
/// into a single hunk (matching `git diff` behaviour).
pub fn hunks(ops: &[Op], context: usize) -> Vec<Hunk> {
    let change_indices: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.typ != OpType::Equal)
        .map(|(i, _)| i)
        .collect();

    if change_indices.is_empty() {
        return Vec::new();
    }

    // Group changes that are within 2*context of each other.
    let mut groups: Vec<(usize, usize)> = Vec::new(); // (first_change_idx, last_change_idx)
    for &ci in &change_indices {
        if let Some(last) = groups.last_mut() {
            if ci <= last.1 + 2 * context {
                last.1 = ci;
                continue;
            }
        }
        groups.push((ci, ci));
    }

    let mut result = Vec::with_capacity(groups.len());
    for (g_start, g_end) in groups {
        let hunk_start = g_start.saturating_sub(context);
        let hunk_end = (g_end + context + 1).min(ops.len());

        let mut old_start = 1usize;
        let mut new_start = 1usize;
        if let Some(first) = ops.get(hunk_start) {
            old_start = first.a + 1;
            new_start = first.b + 1;
        }

        let mut old_count = 0;
        let mut new_count = 0;
        let mut lines = Vec::with_capacity(hunk_end - hunk_start);

        for op in &ops[hunk_start..hunk_end] {
            match op.typ {
                OpType::Equal => {
                    old_count += 1;
                    new_count += 1;
                    lines.push(HunkLine { kind: LineKind::Context, text: op.old.clone() });
                }
                OpType::Delete => {
                    old_count += 1;
                    lines.push(HunkLine { kind: LineKind::Delete, text: op.old.clone() });
                }
                OpType::Insert => {
                    new_count += 1;
                    lines.push(HunkLine { kind: LineKind::Insert, text: op.new.clone() });
                }
                OpType::Replace => {
                    old_count += 1;
                    new_count += 1;
                    lines.push(HunkLine { kind: LineKind::Delete, text: op.old.clone() });
                    lines.push(HunkLine { kind: LineKind::Insert, text: op.new.clone() });
                }
            }
        }

        result.push(Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines,
        });
    }
    result
}

/// Render an edit script as unified diff text.
///
/// `context` is the number of unchanged lines to show around each change
/// (default in `git diff` is 3). The output uses standard unified diff format:
///
/// ```text
/// @@ -old_start,old_count +new_start,new_count @@
///  context line
/// -deleted line
/// +inserted line
/// ```
pub fn to_unified(ops: &[Op], context: usize) -> String {
    let hunks = hunks(ops, context);
    let mut out = String::new();
    for h in &hunks {
        out.push_str(&format_hunk_header(h));
        for line in &h.lines {
            let prefix = match line.kind {
                LineKind::Context => ' ',
                LineKind::Delete => '-',
                LineKind::Insert => '+',
            };
            out.push(prefix);
            out.push_str(&line.text);
            out.push('\n');
        }
    }
    out
}

fn format_hunk_header(h: &Hunk) -> String {
    // When count is 0, the line number should point to the line before the insertion.
    // Following GNU diff convention: start = (line == 0) ? 0 : line, but count 0
    // means the hunk is a pure insertion at that position.
    let old_s = if h.old_count == 0 { h.old_start.saturating_sub(1) } else { h.old_start };
    let new_s = if h.new_count == 0 { h.new_start.saturating_sub(1) } else { h.new_start };
    format!("@@ -{},{} +{},{} @@\n", old_s, h.old_count, new_s, h.new_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lines::diff;

    #[test]
    fn single_change_produces_hunk() {
        let ops = diff(&["a", "b", "c"], &["a", "x", "c"]);
        let hs = hunks(&ops, 3);
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].old_count, 3);
        assert_eq!(hs[0].new_count, 3);
    }

    #[test]
    fn no_changes_no_hunks() {
        let ops = diff(&["a", "b"], &["a", "b"]);
        assert!(hunks(&ops, 3).is_empty());
    }

    #[test]
    fn unified_format_basic() {
        let ops = diff(&["line1", "line2", "line3"], &["line1", "CHANGED", "line3"]);
        let text = to_unified(&ops, 3);
        assert!(text.starts_with("@@ -1,3 +1,3 @@"));
        assert!(text.contains(" line1\n"));
        assert!(text.contains("-line2\n"));
        assert!(text.contains("+CHANGED\n"));
        assert!(text.contains(" line3\n"));
    }

    #[test]
    fn context_zero_shows_only_changes() {
        let ops = diff(&["a", "b", "c", "d"], &["a", "x", "c", "d"]);
        let text = to_unified(&ops, 0);
        assert!(!text.contains(" a\n"));
        assert!(text.contains("-b\n"));
        assert!(text.contains("+x\n"));
    }

    #[test]
    fn distant_changes_form_separate_hunks() {
        let a: Vec<String> = (0..20).map(|i| format!("line{i}")).collect();
        let mut b = a.clone();
        b[1] = "CHANGED1".into();
        b[18] = "CHANGED2".into();
        let ops = diff(&a, &b);
        let hs = hunks(&ops, 3);
        assert_eq!(hs.len(), 2, "should have 2 separate hunks");
    }

    #[test]
    fn nearby_changes_merge_into_one_hunk() {
        let a: Vec<String> = (0..20).map(|i| format!("line{i}")).collect();
        let mut b = a.clone();
        b[5] = "X".into();
        b[10] = "Y".into();
        let ops = diff(&a, &b);
        let hs = hunks(&ops, 3);
        assert_eq!(hs.len(), 1, "changes 5 apart with context 3 should merge");
    }

    #[test]
    fn pure_insertion_hunk() {
        let ops = diff(&["a", "c"], &["a", "b", "c"]);
        let text = to_unified(&ops, 0);
        assert!(text.contains("+b\n"));
    }

    #[test]
    fn pure_deletion_hunk() {
        let ops = diff(&["a", "b", "c"], &["a", "c"]);
        let text = to_unified(&ops, 0);
        assert!(text.contains("-b\n"));
    }
}
