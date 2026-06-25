//! PDF text diff: extract per-page text with pdfium, strip repeated running
//! headers/footers, then diff the flat line stream. Ported from internal/diff/pdf.go.
//!
//! pdfium is bound at RUNTIME (libloading) — nothing is linked at build time, so
//! cross-compiling is unaffected. The library (`pdfium.dll` on Windows,
//! `libpdfium.so` on Linux) is located via `PDFIUM_LIB_PATH`, then next to the
//! executable, then the system loader.

use tate::lines::diff;
use tate::inline::OpType;
use crate::text::{build_text_result, TextResult};
use pdfium_render::prelude::{PdfRenderConfig, Pdfium};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// pdfium may only be bound once per process; cache the binding (or the binding
/// error) so repeated diffs reuse it. `get_or_init` guarantees the bind runs
/// exactly once even under concurrent calls.
static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();

/// pdfium is not thread-safe across document lifecycles, so serialize all
/// extraction work process-wide. PDF diffs are I/O/CPU bound and run one at a
/// time anyway, so this gates concurrent callers without hurting throughput.
static PDFIUM_LOCK: Mutex<()> = Mutex::new(());

fn pdfium() -> Result<&'static Pdfium, String> {
    PDFIUM
        .get_or_init(bind_pdfium)
        .as_ref()
        .map_err(|e| e.clone())
}

/// Progress callback: (side "A"/"B", pages_done, pages_total). Called at ~1%
/// granularity during extraction so a GUI can show "extracting 1200/3000".
pub type ProgressFn<'a> = dyn FnMut(&str, usize, usize) + 'a;

pub fn pdf_diff(path_a: &str, path_b: &str) -> Result<TextResult, String> {
    pdf_diff_progress(path_a, path_b, &mut |_, _, _| {})
}

/// pdf_diff with extraction progress reporting.
pub fn pdf_diff_progress(
    path_a: &str,
    path_b: &str,
    progress: &mut ProgressFn,
) -> Result<TextResult, String> {
    let pdfium = pdfium()?;
    let _guard = PDFIUM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let a = extract_lines(pdfium, path_a, &mut |done, total| progress("A", done, total))?;
    let b = extract_lines(pdfium, path_b, &mut |done, total| progress("B", done, total))?;
    Ok(build_text_result(path_a, path_b, a, b))
}

/// extract_pdf_lines returns the document's text as an ordered slice of lines,
/// with running headers/footers stripped. Binds pdfium per call (convenience
/// entry used by tests and one-off extraction).
pub fn extract_pdf_lines(path: &str) -> Result<Vec<String>, String> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let pdfium = pdfium()?;
    let _guard = PDFIUM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    extract_lines(pdfium, path, &mut |_, _| {})
}

/// extract_lines pulls per-page text via pdfium, reports progress, then strips
/// running headers/footers and flattens to a single line stream.
fn extract_lines(
    pdfium: &Pdfium,
    path: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Vec<String>, String> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {}", path, e))?;
    let doc = pdfium
        .load_pdf_from_byte_slice(&bytes, None)
        .map_err(|e| format!("open pdf {}: {}", path, e))?;

    let pages = doc.pages();
    let total = pages.len() as usize;
    // Throttle progress to ~1% steps so huge docs don't flood the callback.
    let step = (total / 100).max(1);

    let mut page_lines: Vec<Vec<String>> = Vec::with_capacity(total);
    for (i, page) in pages.iter().enumerate() {
        let text = page
            .text()
            .map_err(|e| format!("page {} text: {}", i + 1, e))?
            .all();
        page_lines.push(split_non_empty(&text));
        if i % step == 0 || i + 1 == total {
            progress(i + 1, total);
        }
    }

    page_lines = strip_running_headers_footers(page_lines);
    let mut out: Vec<String> = Vec::new();
    for lines in page_lines {
        out.extend(lines);
    }
    Ok(out)
}

/// Locate and bind the pdfium dynamic library at runtime. Order: PDFIUM_LIB_PATH
/// (file or dir), the executable's own directory, the current directory, then
/// the system loader.
fn bind_pdfium() -> Result<Pdfium, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(p) = std::env::var("PDFIUM_LIB_PATH") {
        let pb = PathBuf::from(&p);
        if pb.is_dir() {
            candidates.push(Pdfium::pdfium_platform_library_name_at_path(&pb));
        } else {
            candidates.push(pb);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(Pdfium::pdfium_platform_library_name_at_path(&dir));
        }
    }
    candidates.push(Pdfium::pdfium_platform_library_name_at_path("./"));

    let mut tried: Vec<String> = Vec::new();
    for c in &candidates {
        match Pdfium::bind_to_library(c) {
            Ok(b) => return Ok(Pdfium::new(b)),
            Err(e) => tried.push(format!("  {} → {}", c.display(), e)),
        }
    }
    match Pdfium::bind_to_system_library() {
        Ok(b) => Ok(Pdfium::new(b)),
        Err(e) => Err(format!(
            "could not load the pdfium library — place pdfium.dll next to the application. Tried:\n{}\n  system → {}",
            tried.join("\n"),
            e
        )),
    }
}

/// render_page renders one page (0-based) of `path` to a PNG image scaled to a
/// target pixel width, returning the encoded PNG bytes.
pub fn render_page(path: &str, page_index: usize, target_width: u32) -> Result<Vec<u8>, String> {
    let pdfium = pdfium()?;
    let _guard = PDFIUM_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {}", path, e))?;
    let doc = pdfium
        .load_pdf_from_byte_slice(&bytes, None)
        .map_err(|e| format!("open pdf {}: {}", path, e))?;

    let page = doc
        .pages()
        .get(page_index as i32)
        .map_err(|e| format!("page {}: {}", page_index + 1, e))?;

    // Preserve aspect ratio: derive height from the page's point dimensions.
    let w_pts = page.width().value.max(1.0);
    let h_pts = page.height().value.max(1.0);
    let target_w = target_width.clamp(64, 4000) as i32;
    let target_h = ((target_w as f32) * (h_pts / w_pts)).round() as i32;

    let config = PdfRenderConfig::new()
        .set_target_width(target_w)
        .set_target_height(target_h.max(1));
    let bitmap = page
        .render_with_config(&config)
        .map_err(|e| format!("render page {}: {}", page_index + 1, e))?;

    let w = bitmap.width() as u32;
    let h = bitmap.height() as u32;
    // as_rgba_bytes normalizes pdfium's native BGRA to RGBA for the PNG encoder.
    let rgba = bitmap.as_rgba_bytes();

    encode_png(w, h, &rgba)
}

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("png header: {}", e))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("png data: {}", e))?;
    }
    Ok(out)
}

/// A clickable in-page link (e.g. a TOC entry): its rectangle on the page,
/// normalized 0..1 (top-left origin), and the 0-based page it jumps to.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PageLink {
    pub rect: HiRect,
    /// 0-based destination page index within the same document.
    pub target: usize,
}

/// page_links returns the GoTo link annotations on one page (TOC entries link to
/// their target page). Only intra-document destinations are returned; external
/// links and links without a resolvable page are skipped. Cheap per page, but it
/// must load the page, so callers should only request the front pages (the TOC).
pub fn page_links(path: &str, page_index: usize) -> Result<Vec<PageLink>, String> {
    let pdfium = pdfium()?;
    let _guard = PDFIUM_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {}", path, e))?;
    let doc = pdfium
        .load_pdf_from_byte_slice(&bytes, None)
        .map_err(|e| format!("open pdf {}: {}", path, e))?;
    let page = doc
        .pages()
        .get(page_index as i32)
        .map_err(|e| format!("page {}: {}", page_index + 1, e))?;
    let pw = page.width().value.max(1.0);
    let ph = page.height().value.max(1.0);

    let mut out: Vec<PageLink> = Vec::new();
    // Iterate (not len()) — len() does an expensive probe per the crate docs.
    for link in page.links().iter() {
        let target = match link.destination().and_then(|d| d.page_index().ok()) {
            Some(idx) => idx as usize,
            None => continue, // no resolvable in-document destination
        };
        let rect = match link.rect() {
            Ok(r) => HiRect {
                x: r.left().value / pw,
                y: (ph - r.top().value) / ph,
                w: (r.right().value - r.left().value) / pw,
                h: (r.top().value - r.bottom().value) / ph,
            },
            Err(_) => continue,
        };
        out.push(PageLink { rect, target });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Per-word highlight rectangles (the Beyond-Compare overlay on rendered pages)
// ---------------------------------------------------------------------------

/// A highlight rectangle on a page, normalized to 0..1 (top-left origin) so the
/// frontend can scale it to whatever pixel size the page image was rendered at.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct HiRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// One character with its on-page normalized rectangle.
#[derive(Clone)]
struct CharBox {
    ch: char,
    rect: Option<HiRect>,
}

/// union_rect merges the glyph rects of chars [start,end) into one bounding box.
fn union_rect(boxes: &[CharBox], start: usize, end: usize) -> Option<HiRect> {
    let mut acc: Option<(f32, f32, f32, f32)> = None; // x0,y0,x1,y1
    for b in &boxes[start..end.min(boxes.len())] {
        if let Some(r) = &b.rect {
            let (x0, y0, x1, y1) = (r.x, r.y, r.x + r.w, r.y + r.h);
            acc = Some(match acc {
                None => (x0, y0, x1, y1),
                Some((ax0, ay0, ax1, ay1)) => {
                    (ax0.min(x0), ay0.min(y0), ax1.max(x1), ay1.max(y1))
                }
            });
        }
    }
    acc.map(|(x0, y0, x1, y1)| HiRect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
}

// ---------------------------------------------------------------------------
// doc_diff: single-document (B-primary) highlight model
//
// Instead of pairing pages (which breaks when a PDF is repaginated and every
// footer/page-number differs), we do ONE global line diff of A vs B, then map
// each changed B line back to the page + rectangle it occupies in B. The UI then
// renders only B's pages and paints highlights on them. Deletions (text in A
// that's gone in B) are surfaced as a thin marker at the B position where the
// removal happened, with the removed text available on hover.
// ---------------------------------------------------------------------------

/// Which side of the diff a rectangle set belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum Side {
    A,
    B,
}

/// What kind of change a line represents. In side-by-side, Removed shows on A,
/// Added on B, Modified on both (with word-level highlights on each side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

/// Two-sided diff summary for the side-by-side view. The frontend renders both
/// documents and asks for per-page rects lazily; `align` lets it keep the two
/// columns scroll-synced and jump between changes.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DocDiff {
    #[cfg_attr(feature = "serde", serde(rename = "pagesA"))]
    pub pages_a: usize,
    #[cfg_attr(feature = "serde", serde(rename = "pagesB"))]
    pub pages_b: usize,
    /// A pages that have removed/modified content, in order.
    #[cfg_attr(feature = "serde", serde(rename = "changedPagesA"))]
    pub changed_pages_a: Vec<ChangedPage>,
    /// B pages that have added/modified content, in order.
    #[cfg_attr(feature = "serde", serde(rename = "changedPagesB"))]
    pub changed_pages_b: Vec<ChangedPage>,
    /// Paired page rows for a single-scroll side-by-side layout: each row places
    /// an A page next to its aligned B page (either may be absent). Rows cover
    /// every page of both documents in order, so one shared scrollbar keeps the
    /// columns perfectly aligned — no JS scroll-syncing needed.
    pub rows: Vec<RowPair>,
    /// Row indices that contain a change, ascending — for "jump to next change".
    #[cfg_attr(feature = "serde", serde(rename = "changeRows"))]
    pub change_rows: Vec<usize>,
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
}

/// One row of the paired layout. `a`/`b` are 0-based page indices, or None when
/// that side has no page in this row (a gap opposite an added/removed run).
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct RowPair {
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub a: Option<usize>,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub b: Option<usize>,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ChangedPage {
    pub page: usize,
    pub count: usize,
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PageAlign {
    #[cfg_attr(feature = "serde", serde(rename = "aPage"))]
    pub a_page: usize,
    #[cfg_attr(feature = "serde", serde(rename = "bPage"))]
    pub b_page: usize,
}

/// A highlight on a page, produced by the per-page geometry pass.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PageChange {
    pub kind: ChangeKind,
    pub rects: Vec<HiRect>,
    /// The other side's text for this change (old for B-modified, new for ... ),
    /// shown on hover so the user sees before→after. May be empty.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "String::is_empty"))]
    pub counterpart: String,
    pub text: String,
}

/// An aligned change spanning both sides. For Removed, `b_*` is None; for Added,
/// `a_*` is None; for Modified, both are set.
#[derive(Clone)]
struct BiChange {
    kind: ChangeKind,
    a_index: Option<usize>,
    a_page: Option<usize>,
    a_segs: Vec<tate::inline::Seg>,
    a_text: String,
    b_index: Option<usize>,
    b_page: Option<usize>,
    b_segs: Vec<tate::inline::Seg>,
    b_text: String,
}

/// doc_diff: global A-vs-B line diff producing a two-sided summary. Documents are
/// extracted once (with geometry) and cached, so the heavy pdfium pass runs only
/// the first time; later doc_diff/page_changes/render calls reuse the cache.
pub fn doc_diff(path_a: &str, path_b: &str) -> Result<DocDiff, String> {
    doc_diff_progress(path_a, path_b, &mut |_, _, _| {})
}

/// doc_diff with extraction progress: the callback is (side "A"/"B", pages_done,
/// pages_total), fired while each document is first extracted (the slow pass).
/// Cached documents fire no progress.
pub fn doc_diff_progress(
    path_a: &str,
    path_b: &str,
    progress: &mut ProgressFn,
) -> Result<DocDiff, String> {
    let lines_a = cached_lines_progress(path_a, &mut |d, t| progress("A", d, t))?;
    let lines_b = cached_lines_progress(path_b, &mut |d, t| progress("B", d, t))?;
    let pages_a = lines_a.iter().map(|l| l.page + 1).max().unwrap_or(0);
    let pages_b = lines_b.iter().map(|l| l.page + 1).max().unwrap_or(0);

    let (changes, align) = classify(&lines_a, &lines_b);

    let mut per_a: HashMap<usize, usize> = HashMap::new();
    let mut per_b: HashMap<usize, usize> = HashMap::new();
    let (mut added, mut modified, mut removed) = (0, 0, 0);
    for c in &changes {
        if let Some(p) = c.a_page {
            *per_a.entry(p).or_insert(0) += 1;
        }
        if let Some(p) = c.b_page {
            *per_b.entry(p).or_insert(0) += 1;
        }
        match c.kind {
            ChangeKind::Added => added += 1,
            ChangeKind::Modified => modified += 1,
            ChangeKind::Removed => removed += 1,
        }
    }
    let to_pages = |m: &HashMap<usize, usize>| {
        let mut v: Vec<ChangedPage> =
            m.iter().map(|(&page, &count)| ChangedPage { page, count }).collect();
        v.sort_unstable_by_key(|c| c.page);
        v
    };

    let rows = build_rows(pages_a, pages_b, &align);
    // A row is "changed" if either of its pages has any change.
    let change_rows: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.a.map(|p| per_a.contains_key(&p)).unwrap_or(false)
                || r.b.map(|p| per_b.contains_key(&p)).unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();

    Ok(DocDiff {
        pages_a,
        pages_b,
        changed_pages_a: to_pages(&per_a),
        changed_pages_b: to_pages(&per_b),
        rows,
        change_rows,
        added,
        modified,
        removed,
    })
}

/// build_rows lays A and B pages into aligned rows using the equal-line page
/// anchors. At each anchor the two pages sit in the same row; pages between
/// anchors are paired 1:1 and any surplus on one side gets a blank opposite.
fn build_rows(pages_a: usize, pages_b: usize, align: &[PageAlign]) -> Vec<RowPair> {
    // Dedup anchors to strictly-increasing (a,b) pairs.
    let mut anchors: Vec<(usize, usize)> = Vec::new();
    for al in align {
        if let Some(&(la, lb)) = anchors.last() {
            if al.a_page <= la || al.b_page <= lb {
                continue;
            }
        }
        anchors.push((al.a_page, al.b_page));
    }

    let mut rows: Vec<RowPair> = Vec::new();
    let (mut a, mut b) = (0usize, 0usize);

    // Emit a span of pages [a..a_to) × [b..b_to): pair 1:1, blanks for surplus.
    let emit = |rows: &mut Vec<RowPair>, a: usize, a_to: usize, b: usize, b_to: usize| {
        let (mut i, mut j) = (a, b);
        while i < a_to && j < b_to {
            rows.push(RowPair { a: Some(i), b: Some(j) });
            i += 1;
            j += 1;
        }
        while i < a_to {
            rows.push(RowPair { a: Some(i), b: None });
            i += 1;
        }
        while j < b_to {
            rows.push(RowPair { a: None, b: Some(j) });
            j += 1;
        }
    };

    for &(aa, ba) in &anchors {
        if aa < a || ba < b {
            continue; // already past this anchor
        }
        emit(&mut rows, a, aa, b, ba); // gap before the anchor
        rows.push(RowPair { a: Some(aa), b: Some(ba) }); // the anchor row itself
        a = aa + 1;
        b = ba + 1;
    }
    emit(&mut rows, a, pages_a, b, pages_b); // tail after the last anchor
    rows
}

/// page_changes: highlight rectangles for one page of one side. Reuses the cached
/// lines (with geometry), so no re-extraction — just maps that page to rects.
pub fn page_changes(
    path_a: &str,
    path_b: &str,
    side: Side,
    page: usize,
) -> Result<Vec<PageChange>, String> {
    let lines_a = cached_lines(path_a)?;
    let lines_b = cached_lines(path_b)?;
    let (changes, _) = classify(&lines_a, &lines_b);

    let mut out: Vec<PageChange> = Vec::new();
    for c in &changes {
        match side {
            Side::A => {
                if c.a_page != Some(page) {
                    continue;
                }
                // A side shows Removed (whole line) and Modified (old words).
                let boxes = c.a_index.and_then(|i| lines_a.get(i)).map(|l| l.boxes.as_slice());
                match c.kind {
                    ChangeKind::Removed => {
                        let rects = boxes.and_then(|b| union_rect(b, 0, b.len())).into_iter().collect();
                        out.push(PageChange { kind: c.kind, rects, counterpart: String::new(), text: c.a_text.clone() });
                    }
                    ChangeKind::Modified => {
                        let rects = boxes.map(|b| word_rects(b, &c.a_segs)).unwrap_or_default();
                        out.push(PageChange { kind: c.kind, rects, counterpart: c.b_text.clone(), text: c.a_text.clone() });
                    }
                    ChangeKind::Added => {}
                }
            }
            Side::B => {
                if c.b_page != Some(page) {
                    continue;
                }
                let boxes = c.b_index.and_then(|i| lines_b.get(i)).map(|l| l.boxes.as_slice());
                match c.kind {
                    ChangeKind::Added => {
                        let rects = boxes.and_then(|b| union_rect(b, 0, b.len())).into_iter().collect();
                        out.push(PageChange { kind: c.kind, rects, counterpart: String::new(), text: c.b_text.clone() });
                    }
                    ChangeKind::Modified => {
                        let rects = boxes.map(|b| word_rects(b, &c.b_segs)).unwrap_or_default();
                        out.push(PageChange { kind: c.kind, rects, counterpart: c.a_text.clone(), text: c.b_text.clone() });
                    }
                    ChangeKind::Removed => {}
                }
            }
        }
    }
    Ok(out)
}

/// classify runs the paired line diff into two-sided changes, and also returns a
/// page-alignment list built from equal-line anchors (for linked scrolling).
fn classify(lines_a: &[GeomLine], lines_b: &[GeomLine]) -> (Vec<BiChange>, Vec<PageAlign>) {
    let text_a: Vec<String> = lines_a.iter().map(|l| l.text.clone()).collect();
    let text_b: Vec<String> = lines_b.iter().map(|l| l.text.clone()).collect();
    let ops = tate::inline::pair_replacements(diff(&text_a, &text_b), tate::inline::DEFAULT_SIMILARITY);

    let mut changes: Vec<BiChange> = Vec::new();
    let mut align: Vec<PageAlign> = Vec::new();
    let mut last_align: Option<(usize, usize)> = None;

    for op in &ops {
        match op.typ {
            OpType::Equal => {
                // Record a page-alignment anchor when the page pair advances.
                if let (Some(al), Some(bl)) = (lines_a.get(op.a), lines_b.get(op.b)) {
                    let pair = (al.page, bl.page);
                    if last_align != Some(pair) {
                        align.push(PageAlign { a_page: al.page, b_page: bl.page });
                        last_align = Some(pair);
                    }
                }
            }
            OpType::Insert => {
                if let Some(bl) = lines_b.get(op.b) {
                    changes.push(BiChange {
                        kind: ChangeKind::Added,
                        a_index: None,
                        a_page: None,
                        a_segs: Vec::new(),
                        a_text: String::new(),
                        b_index: Some(op.b),
                        b_page: Some(bl.page),
                        b_segs: Vec::new(),
                        b_text: truncate(&bl.text, 300),
                    });
                }
            }
            OpType::Replace => {
                let (al, bl) = (lines_a.get(op.a), lines_b.get(op.b));
                changes.push(BiChange {
                    kind: ChangeKind::Modified,
                    a_index: Some(op.a),
                    a_page: al.map(|l| l.page),
                    a_segs: op.old_segs.clone(),
                    a_text: al.map(|l| truncate(&l.text, 300)).unwrap_or_default(),
                    b_index: Some(op.b),
                    b_page: bl.map(|l| l.page),
                    b_segs: op.new_segs.clone(),
                    b_text: bl.map(|l| truncate(&l.text, 300)).unwrap_or_default(),
                });
            }
            OpType::Delete => {
                if let Some(al) = lines_a.get(op.a) {
                    changes.push(BiChange {
                        kind: ChangeKind::Removed,
                        a_index: Some(op.a),
                        a_page: Some(al.page),
                        a_segs: Vec::new(),
                        a_text: truncate(&al.text, 300),
                        b_index: None,
                        b_page: None,
                        b_segs: Vec::new(),
                        b_text: String::new(),
                    });
                }
            }
        }
    }
    (changes, align)
}

/// word_rects maps the changed inline segments of a Modified line back to glyph
/// rectangles, so only the words that differ are highlighted.
fn word_rects(boxes: &[CharBox], b_segs: &[tate::inline::Seg]) -> Vec<HiRect> {
    if b_segs.is_empty() {
        return union_rect(boxes, 0, boxes.len()).into_iter().collect();
    }
    let mut rects: Vec<HiRect> = Vec::new();
    let mut box_idx = 0usize;
    for seg in b_segs {
        let seg_len = seg.text.chars().filter(|c| !c.is_whitespace()).count();
        if seg_len == 0 {
            continue;
        }
        let start = advance_to_nonspace(boxes, box_idx);
        let mut count = 0;
        let mut j = start;
        while j < boxes.len() && count < seg_len {
            if !boxes[j].ch.is_whitespace() {
                count += 1;
            }
            j += 1;
        }
        if seg.changed {
            if let Some(r) = union_rect(boxes, start, j) {
                rects.push(r);
            }
        }
        box_idx = j;
    }
    rects
}

fn advance_to_nonspace(boxes: &[CharBox], from: usize) -> usize {
    let mut i = from;
    while i < boxes.len() && boxes[i].ch.is_whitespace() {
        i += 1;
    }
    i
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let t: String = s.chars().take(n).collect();
    format!("{t}…")
}

/// A line of a PDF with geometry: normalized text, its page, and the glyph boxes
/// that compose it (for word-level rect mapping).
#[derive(Clone)]
struct GeomLine {
    text: String,
    page: usize,
    boxes: Vec<CharBox>,
}

/// Process-wide cache of extracted lines, keyed by (path, mtime, size). pdfium
/// text extraction over a multi-thousand-page PDF costs ~15s, so caching makes
/// the initial doc_diff pay it once; later page_changes/re-diffs are instant.
static LINE_CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<GeomLine>>>>> = OnceLock::new();

fn cache_key(path: &str) -> String {
    let meta = std::fs::metadata(path).ok();
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    format!("{path}|{mtime}|{size}")
}

fn cached_lines(path: &str) -> Result<Arc<Vec<GeomLine>>, String> {
    cached_lines_progress(path, &mut |_, _| {})
}

/// cached_lines returns the extracted (geometry-bearing) lines for a document,
/// extracting + caching on first use. Holds PDFIUM_LOCK only during extraction.
fn cached_lines_progress(
    path: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Arc<Vec<GeomLine>>, String> {
    if path.is_empty() {
        return Ok(Arc::new(Vec::new()));
    }
    let key = cache_key(path);
    let cache = LINE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Ok(hit.clone());
    }
    let pdfium = pdfium()?;
    let lines = {
        let _guard = PDFIUM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Arc::new(geom_lines(pdfium, path, progress)?)
    };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, lines.clone());
    Ok(lines)
}

/// geom_lines extracts every line of a document with its glyph boxes, splitting
/// on the char stream's newlines and stripping running headers/footers. This is
/// the single expensive pass; its result is cached by `cached_lines`.
fn geom_lines(
    pdfium: &Pdfium,
    path: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Vec<GeomLine>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {}", path, e))?;
    let doc = pdfium
        .load_pdf_from_byte_slice(&bytes, None)
        .map_err(|e| format!("open pdf {}: {}", path, e))?;

    let mut per_page: Vec<Vec<GeomLine>> = Vec::new();
    let total = doc.pages().len() as usize;
    let step = (total / 100).max(1);
    for (pi, page) in doc.pages().iter().enumerate() {
        let pw = page.width().value.max(1.0);
        let ph = page.height().value.max(1.0);
        let text = page
            .text()
            .map_err(|e| format!("page {} text: {}", pi + 1, e))?;

        let mut lines: Vec<GeomLine> = Vec::new();
        let mut cur: Vec<CharBox> = Vec::new();
        for c in text.chars().iter() {
            let ch = c.unicode_char().unwrap_or('\u{fffd}');
            if ch == '\n' || ch == '\r' {
                push_geom_line(&mut lines, &mut cur, pi);
                continue;
            }
            let rect = c.loose_bounds().ok().map(|r| HiRect {
                x: r.left().value / pw,
                y: (ph - r.top().value) / ph,
                w: (r.right().value - r.left().value) / pw,
                h: (r.top().value - r.bottom().value) / ph,
            });
            cur.push(CharBox { ch, rect });
        }
        push_geom_line(&mut lines, &mut cur, pi);
        per_page.push(lines);
        if pi % step == 0 || pi + 1 == total {
            progress(pi + 1, total);
        }
    }

    // Strip running headers/footers (text-only criteria), preserving geometry.
    let page_texts: Vec<Vec<String>> = per_page
        .iter()
        .map(|ls| ls.iter().map(|l| l.text.clone()).collect())
        .collect();
    let repeated = repeated_lines(&page_texts);

    let mut out: Vec<GeomLine> = Vec::new();
    for lines in per_page {
        for l in lines {
            if repeated.contains(&mask_numbers(&l.text)) {
                continue;
            }
            out.push(l);
        }
    }
    Ok(out)
}

/// push_geom_line finalizes the current char run into a normalized GeomLine.
fn push_geom_line(lines: &mut Vec<GeomLine>, cur: &mut Vec<CharBox>, page: usize) {
    if cur.is_empty() {
        return;
    }
    let raw: String = cur.iter().map(|c| c.ch).collect();
    let text = normalize_pdf_line(&raw);
    if !text.is_empty() {
        lines.push(GeomLine { text, page, boxes: std::mem::take(cur) });
    } else {
        cur.clear();
    }
}

/// repeated_lines returns the set of masked lines that recur across most pages
/// (running headers/footers), matching strip_running_headers_footers' criteria.
fn repeated_lines(pages: &[Vec<String>]) -> HashSet<String> {
    let mut repeated = HashSet::new();
    if pages.len() < 3 {
        return repeated;
    }
    let mut page_count: HashMap<String, usize> = HashMap::new();
    let mut total_count: HashMap<String, usize> = HashMap::new();
    for lines in pages {
        let mut seen: HashSet<String> = HashSet::new();
        for ln in lines {
            let m = mask_numbers(ln);
            *total_count.entry(m.clone()).or_insert(0) += 1;
            seen.insert(m);
        }
        for k in seen {
            *page_count.entry(k).or_insert(0) += 1;
        }
    }
    let threshold = pages.len().div_ceil(2);
    for (k, &c) in &page_count {
        let avg = total_count[k] as f64 / c as f64;
        if c >= threshold && avg <= 1.5 && !strip_mask(k).trim().is_empty() {
            repeated.insert(k.clone());
        }
    }
    repeated
}

/// normalize_pdf_line trims and collapses runs of whitespace so minor extraction
/// jitter does not produce false differences, while preserving single column gaps.
fn normalize_pdf_line(s: &str) -> String {
    // Replace non-breaking space (U+00A0) with a regular space, like the Go code.
    let s = s.replace('\u{00a0}', " ");
    let s = s.trim_matches([' ', '\t']);
    // Collapse internal whitespace runs: a run of 2+ spaces/tabs longer than 4 is
    // capped at 4 spaces (kept as a column separator); shorter runs pass through.
    let mut out = String::with_capacity(s.len());
    let mut run = 0usize;
    let flush = |out: &mut String, run: usize| {
        if run == 0 {
            return;
        }
        let keep = if run > 4 { 4 } else { run };
        for _ in 0..keep {
            out.push(' ');
        }
    };
    for ch in s.chars() {
        if ch == ' ' || ch == '\t' {
            run += 1;
        } else {
            flush(&mut out, run);
            run = 0;
            out.push(ch);
        }
    }
    // A trailing run is trimmed (we already trimmed the ends, so none remains).
    out
}

fn split_non_empty(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in s.split('\n') {
        let ln = normalize_pdf_line(line);
        if !ln.is_empty() {
            out.push(ln);
        }
    }
    out
}

/// strip_running_headers_footers removes lines that repeat across most pages.
fn strip_running_headers_footers(pages: Vec<Vec<String>>) -> Vec<Vec<String>> {
    if pages.len() < 3 {
        return pages;
    }

    let mut page_count: HashMap<String, usize> = HashMap::new();
    let mut total_count: HashMap<String, usize> = HashMap::new();
    for lines in &pages {
        let mut seen: HashSet<String> = HashSet::new();
        for ln in lines {
            let m = mask_numbers(ln);
            *total_count.entry(m.clone()).or_insert(0) += 1;
            seen.insert(m);
        }
        for k in seen {
            *page_count.entry(k).or_insert(0) += 1;
        }
    }

    let threshold = pages.len().div_ceil(2);
    let mut repeated: HashSet<String> = HashSet::new();
    for (k, &c) in &page_count {
        let avg_per_page = total_count[k] as f64 / c as f64;
        if c >= threshold && avg_per_page <= 1.5 && !strip_mask(k).trim().is_empty() {
            repeated.insert(k.clone());
        }
    }
    if repeated.is_empty() {
        return pages;
    }

    pages
        .into_iter()
        .map(|lines| {
            lines
                .into_iter()
                .filter(|ln| !repeated.contains(&mask_numbers(ln)))
                .collect()
        })
        .collect()
}

const MASK: &str = "\u{0}#\u{0}";

/// mask_numbers replaces digit runs with a sentinel so "Page 3 of 50" and
/// "Page 4 of 50" are recognized as the same running footer.
fn mask_numbers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_digits = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push_str(MASK);
                in_digits = true;
            }
        } else {
            in_digits = false;
            out.push(ch);
        }
    }
    out
}

fn strip_mask(s: &str) -> String {
    s.replace(MASK, "")
}
