//! JSON structural diff: parse a JSON string via serde_json, convert to
//! [`tate::tree::TreeNode`], and run [`tate::tree::tree_diff`].
//!
//! ## JSON → TreeNode mapping
//!
//! JSON objects become TreeNode children (one child per key):
//! - Object key → `kind` (the key name) + `identity` (same as key, so siblings match)
//! - Scalar values (string/number/bool/null) → `text` (stringified) + `attributes` (one entry: `value` = stringified)
//! - Nested object → `children` (recursion)
//! - Array → one child per item with `kind = "[array]"` and no identity (positional matching)
//!
//! This mapping means a top-level JSON config like:
//! ```json
//! {"server": {"port": 8080, "host": "localhost"}}
//! ```
//! becomes:
//! ```text
//! root (kind="root")
//!   └─ "server" (identity="server")
//!        ├─ "port" (identity="port", attr value="8080")
//!        └─ "host" (identity="host", attr value="localhost")
//! ```
//! and `tree_diff` reports changes like `{Modified, id="port", changed_attrs=[(value, 8080→9090)]}`.

use tate::json::from_json_value;
use tate::tree::{tree_diff, TreeDiff, TreeNode};

/// Parse a JSON string into a [`TreeNode`] for use with `tate::tree` functions.
///
/// The `Value → TreeNode` mapping lives in tate ([`tate::json::from_json_value`]);
/// this wrapper only adds the string-parsing step.
pub fn json_to_tree(json: &str) -> Result<TreeNode, String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("parse: {e}"))?;
    Ok(from_json_value("root", &value))
}

/// Diff two JSON strings and return the structural changes.
///
/// # Example
/// ```
/// use mumford::json::json_diff;
/// use tate::tree::ChangeKind;
///
/// let a = r#"{"server": {"port": 8080}}"#;
/// let b = r#"{"server": {"port": 9090}}"#;
/// let diff = json_diff(a, b).unwrap();
/// assert_eq!(diff.changes.len(), 1);
/// assert_eq!(diff.changes[0].kind, ChangeKind::Modified);
/// assert_eq!(diff.changes[0].id, "port");
/// ```
pub fn json_diff(a: &str, b: &str) -> Result<TreeDiff, String> {
    let va: serde_json::Value = serde_json::from_str(a).map_err(|e| format!("parse A: {e}"))?;
    let vb: serde_json::Value = serde_json::from_str(b).map_err(|e| format!("parse B: {e}"))?;
    let ta = from_json_value("root", &va);
    let tb = from_json_value("root", &vb);
    Ok(tree_diff(&ta, &tb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tate::tree::ChangeKind;

    #[test]
    fn modified_scalar_value() {
        let a = r#"{"port": 8080, "host": "localhost"}"#;
        let b = r#"{"port": 9090, "host": "localhost"}"#;
        let d = json_diff(a, b).unwrap();
        assert_eq!(d.changes.len(), 1);
        assert_eq!(d.changes[0].kind, ChangeKind::Modified);
        assert_eq!(d.changes[0].id, "port");
    }

    #[test]
    fn added_key() {
        let a = r#"{"a": 1}"#;
        let b = r#"{"a": 1, "b": 2}"#;
        let d = json_diff(a, b).unwrap();
        assert!(d.changes.iter().any(|c| c.kind == ChangeKind::Added && c.id == "b"));
    }

    #[test]
    fn removed_key() {
        let a = r#"{"a": 1, "b": 2}"#;
        let b = r#"{"a": 1}"#;
        let d = json_diff(a, b).unwrap();
        assert!(d.changes.iter().any(|c| c.kind == ChangeKind::Removed && c.id == "b"));
    }

    #[test]
    fn identical_json_no_changes() {
        let a = r#"{"x": 1, "y": [1, 2, 3]}"#;
        let d = json_diff(a, a).unwrap();
        assert!(d.changes.is_empty());
    }

    #[test]
    fn nested_object_change() {
        let a = r#"{"server": {"port": 8080, "host": "localhost"}}"#;
        let b = r#"{"server": {"port": 9090, "host": "localhost"}}"#;
        let d = json_diff(a, b).unwrap();
        assert_eq!(d.changes.len(), 1);
        assert_eq!(d.changes[0].id, "port");
    }

    #[test]
    fn array_item_change() {
        let a = r#"{"items": ["a", "b", "c"]}"#;
        let b = r#"{"items": ["a", "x", "c"]}"#;
        let d = json_diff(a, b).unwrap();
        // Array items are keyless → changes bubble up through "items" (keyless
        // to tate because it's the only child... actually "items" has identity="items")
        // → "items" surfaces as Modified.
        assert!(d.changes.iter().any(|c| c.id == "items"));
    }

    #[test]
    fn array_item_added() {
        let a = r#"{"items": ["a", "b"]}"#;
        let b = r#"{"items": ["a", "b", "c"]}"#;
        let d = json_diff(a, b).unwrap();
        assert!(!d.changes.is_empty(), "added array item should be detected");
    }

    #[test]
    fn parse_error() {
        let result = json_diff("not json", r#"{}"#);
        assert!(result.is_err());
    }

    #[test]
    fn bool_and_null_values() {
        let a = r#"{"flag": true, "nothing": null}"#;
        let b = r#"{"flag": false, "nothing": null}"#;
        let d = json_diff(a, b).unwrap();
        assert!(d.changes.iter().any(|c| c.id == "flag"));
    }

    #[test]
    fn deep_nesting() {
        let a = r#"{"a": {"b": {"c": {"d": 1}}}}"#;
        let b = r#"{"a": {"b": {"c": {"d": 2}}}}"#;
        let d = json_diff(a, b).unwrap();
        assert!(d.changes.iter().any(|c| c.id == "d"));
    }
}