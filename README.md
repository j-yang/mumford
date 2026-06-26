# mumford

Format-aware diff engines for common document types — PDF, Word, Excel, RTF, PowerPoint, JSON, plain text. Built on top of [`tate`](https://crates.io/crates/tate).

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## Overview

Mumford provides file-format parsing and diffing for real-world documents. All diff algorithms come from `tate`; mumford handles the format-specific work of reading files and converting them into the data structures tate operates on.

## Supported formats

| Format | Extensions | Engine |
|--------|-----------|--------|
| Plain text | `.txt`, `.csv`, `.log`, `.md`, code files | Line-level diff via `tate::lines` |
| PDF | `.pdf` | Text extraction via pdfium, running-header stripping, line diff |
| Word | `.docx` | OOXML paragraph + table extraction, paragraph diff |
| Excel | `.xlsx`, `.xls`, `.xlsm` | Cell-level grid alignment via `tate::grid` |
| RTF | `.rtf` | Styled-table parsing, row/cell diff with formatting preserved |
| PowerPoint | `.pptx` | Slide text extraction, slide-level alignment + line diff |
| JSON | `.json` | Structural diff via `tate::tree::TreeNode` |
| Folders | directories | Recursive comparison with sha256 hashing and rename detection |

## Architecture

```
tate (algorithms: lines, inline, grid, tree)
  ↑
mumford (format engines: parse → feed to tate → wrap result)
  ↑
your app (domain adapters + UI)
```

## Usage

```toml
[dependencies]
mumford = "0.1"
```

### Diff two files (auto-dispatch by extension)

```rust
use mumford::dispatch;

let result = dispatch("old.xlsx", "new.xlsx")?;
if let Some(excel) = result.excel {
    for sheet in &excel.sheets {
        println!("{}: {} rows modified", sheet.name, sheet.grid.modified_rows);
    }
}
```

### JSON structural diff

```rust
use mumford::json::json_diff;

let diff = json_diff(r#"{"port": 8080}"#, r#"{"port": 9090}"#)?;
assert_eq!(diff.changes[0].id, "port");
```

## Notes

- **PDF requires pdfium**: The native pdfium library must be available at runtime (via `PDFIUM_LIB_PATH`, next to the executable, or the system loader). See [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) for pre-built binaries.

## License

MIT