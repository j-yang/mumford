//! PowerPoint (.pptx) diff: extract text from each slide via zip + quick-xml,
//! align slides by content similarity, then diff slide text via
//! [`tate::lines::diff`].
//!
//! A .pptx is an OOXML zip container. Each slide lives at
//! `ppt/slides/slideN.xml`. We extract text runs (`<a:t>`) per slide,
//! producing one "line" per text block. Slides are aligned by content
//! similarity so a slide edited between versions pairs as Modified rather
//! than Remove+Add.

use std::io::Read;

use quick_xml::events::Event;
use quick_xml::Reader;
use tate::lines::diff;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// One slide's content: its 1-based slide number and the text lines it contains.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SlideContent {
    /// 1-based slide number.
    pub slide: usize,
    /// Text blocks extracted from this slide (one per `<a:t>` run).
    pub lines: Vec<String>,
}

/// One aligned slide pair in the diff output.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SlideDiff {
    /// "equal" | "modified" | "added" | "removed".
    pub status: String,
    /// 1-based slide number in A (0 = absent).
    #[cfg_attr(feature = "serde", serde(rename = "slideA"))]
    pub slide_a: usize,
    /// 1-based slide number in B (0 = absent).
    #[cfg_attr(feature = "serde", serde(rename = "slideB"))]
    pub slide_b: usize,
    /// Line-level ops for this slide pair (only populated for Modified).
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
    pub ops: Vec<tate::inline::Op>,
}

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PptxResult {
    #[cfg_attr(feature = "serde", serde(rename = "fileType"))]
    pub file_type: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathA"))]
    pub path_a: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathB"))]
    pub path_b: String,
    pub slides: Vec<SlideDiff>,
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
}

pub fn pptx_diff(path_a: &str, path_b: &str) -> Result<PptxResult, String> {
    let slides_a = read_slides(path_a)?;
    let slides_b = read_slides(path_b)?;

    let mut res = PptxResult {
        file_type: "pptx".into(),
        path_a: path_a.into(),
        path_b: path_b.into(),
        ..Default::default()
    };

    let pairs = align_slides(&slides_a, &slides_b);
    for p in &pairs {
        let mut sd = SlideDiff {
            status: String::new(),
            slide_a: 0,
            slide_b: 0,
            ops: Vec::new(),
        };
        match (p.a, p.b) {
            (Some(ai), Some(bi)) => {
                sd.slide_a = ai + 1;
                sd.slide_b = bi + 1;
                let a_text = &slides_a[ai].lines;
                let b_text = &slides_b[bi].lines;
                if a_text == b_text {
                    sd.status = "equal".into();
                } else {
                    sd.status = "modified".into();
                    let raw = diff(a_text, b_text);
                    sd.ops = tate::inline::pair_replacements(raw, tate::inline::DEFAULT_SIMILARITY);
                    res.modified += 1;
                }
            }
            (Some(ai), None) => {
                sd.slide_a = ai + 1;
                sd.status = "removed".into();
                res.removed += 1;
            }
            (None, Some(bi)) => {
                sd.slide_b = bi + 1;
                sd.status = "added".into();
                res.added += 1;
            }
            (None, None) => {}
        }
        res.slides.push(sd);
    }

    Ok(res)
}

#[derive(Clone, Copy)]
struct SlidePair {
    a: Option<usize>,
    b: Option<usize>,
}

/// Align slides by content similarity. Identical slides pair as Equal; similar
/// slides (≥ 0.4 text overlap) pair as Modified candidates; the rest are
/// positional, with leftovers as pure add/remove.
fn align_slides(a: &[SlideContent], b: &[SlideContent]) -> Vec<SlidePair> {
    let n = a.len().max(b.len());
    let mut pairs = Vec::with_capacity(n);

    // First pass: match by position when content is identical or similar.
    let mut used_b = vec![false; b.len()];
    for (i, sa) in a.iter().enumerate() {
        if i < b.len() && !used_b[i] {
            let sim = slide_similarity(&sa.lines, &b[i].lines);
            if sim >= 0.4 {
                used_b[i] = true;
                pairs.push(SlidePair { a: Some(i), b: Some(i) });
                continue;
            }
        }
        // Try to find a similar B slide (for reordered slides).
        let mut best_j: Option<usize> = None;
        let mut best_sim = 0.0f64;
        for (j, sb) in b.iter().enumerate() {
            if used_b[j] {
                continue;
            }
            let sim = slide_similarity(&sa.lines, &sb.lines);
            if sim > best_sim {
                best_sim = sim;
                best_j = Some(j);
            }
        }
        if best_j.is_some() && best_sim >= 0.5 {
            let j = best_j.unwrap();
            used_b[j] = true;
            pairs.push(SlidePair { a: Some(i), b: Some(j) });
        } else {
            pairs.push(SlidePair { a: Some(i), b: None });
        }
    }

    // Leftover B slides are additions.
    for (j, _) in b.iter().enumerate() {
        if !used_b[j] {
            pairs.push(SlidePair { a: None, b: Some(j) });
        }
    }

    pairs
}

/// Fraction of shared text lines between two slides (Jaccard over line sets).
fn slide_similarity(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let set_a: std::collections::HashSet<&str> = a.iter().map(|s| s.as_str()).collect();
    let set_b: std::collections::HashSet<&str> = b.iter().map(|s| s.as_str()).collect();
    let shared = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    if union == 0 {
        0.0
    } else {
        shared as f64 / union as f64
    }
}

/// Read all slides from a .pptx file, sorted by slide number.
fn read_slides(path: &str) -> Result<Vec<SlideContent>, String> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("unzip {path}: {e}"))?;

    // Collect slide entry names: ppt/slides/slide1.xml, slide2.xml, …
    let mut slide_names: Vec<(usize, String)> = Vec::new();
    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().to_string();
        if let Some(num_str) = name
            .strip_prefix("ppt/slides/slide")
            .and_then(|s| s.strip_suffix(".xml"))
        {
            if let Ok(num) = num_str.parse::<usize>() {
                slide_names.push((num, name));
            }
        }
    }
    slide_names.sort_by_key(|(n, _)| *n);

    let mut slides = Vec::new();
    for (num, name) in &slide_names {
        let mut xml = String::new();
        {
            let mut entry = zip.by_name(name).map_err(|e| format!("{name}: {e}"))?;
            entry.read_to_string(&mut xml).map_err(|e| e.to_string())?;
        }
        let lines = extract_slide_text(&xml);
        slides.push(SlideContent { slide: *num, lines });
    }

    Ok(slides)
}

/// Extract all text runs (`<a:t>...</a:t>`) from a slide XML, in document order.
fn extract_slide_text(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut lines = Vec::new();
    let mut in_text = false;
    let mut buf = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if local_name(e.name().as_ref()) == "t" {
                    in_text = true;
                    buf.clear();
                }
            }
            Ok(Event::Text(t)) => {
                if in_text {
                    buf.push_str(&t.unescape().unwrap_or_default());
                }
            }
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == "t" && in_text {
                    let text = buf.trim().to_string();
                    if !text.is_empty() {
                        lines.push(text);
                    }
                    in_text = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    lines
}

/// Strip namespace prefix ("a:t" -> "t").
fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_text_from_slide_xml() {
        let xml = r#"<xml xmlns:a="x">
            <a:p><a:r><a:t>Title Here</a:t></a:r></a:p>
            <a:p><a:r><a:t>Bullet 1</a:t></a:r></a:p>
            <a:p><a:r><a:t>Bullet 2</a:t></a:r></a:p>
        </xml>"#;
        let lines = extract_slide_text(xml);
        assert_eq!(lines, vec!["Title Here", "Bullet 1", "Bullet 2"]);
    }

    #[test]
    fn empty_slide_produces_no_lines() {
        let xml = r#"<xml xmlns:a="x"><a:p><a:r><a:t>  </a:t></a:r></a:p></xml>"#;
        let lines = extract_slide_text(xml);
        assert!(lines.is_empty());
    }

    #[test]
    fn slide_similarity_identical() {
        let a = vec!["Hello".into(), "World".into()];
        assert!((slide_similarity(&a, &a) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn slide_similarity_half_match() {
        let a = vec!["A".into(), "B".into(), "C".into(), "D".into()];
        let b = vec!["A".into(), "B".into(), "X".into(), "Y".into()];
        let sim = slide_similarity(&a, &b);
        // Jaccard: {A,B} ∩ {A,B} / {A,B,C,D,X,Y} = 2/6 ≈ 0.33
        assert!(sim < 0.5);
    }

    #[test]
    fn align_identical_slides() {
        let a = vec![
            SlideContent { slide: 1, lines: vec!["A".into(), "B".into()] },
            SlideContent { slide: 2, lines: vec!["C".into()] },
        ];
        let b = a.clone();
        let pairs = align_slides(&a, &b);
        assert_eq!(pairs.len(), 2);
        assert!(pairs.iter().all(|p| p.a.is_some() && p.b.is_some()));
    }

    #[test]
    fn align_with_added_slide() {
        let a = vec![SlideContent { slide: 1, lines: vec!["A".into()] }];
        let b = vec![
            SlideContent { slide: 1, lines: vec!["A".into()] },
            SlideContent { slide: 2, lines: vec!["NEW".into()] },
        ];
        let pairs = align_slides(&a, &b);
        assert_eq!(pairs.len(), 2);
        assert!(pairs[1].a.is_none() && pairs[1].b.is_some());
    }

    #[test]
    fn align_with_removed_slide() {
        let a = vec![
            SlideContent { slide: 1, lines: vec!["A".into()] },
            SlideContent { slide: 2, lines: vec!["GONE".into()] },
        ];
        let b = vec![SlideContent { slide: 1, lines: vec!["A".into()] }];
        let pairs = align_slides(&a, &b);
        assert_eq!(pairs.len(), 2);
        assert!(pairs[1].a.is_some() && pairs[1].b.is_none());
    }

    #[test]
    fn diff_identical_pptx_content() {
        let a_lines = vec!["Title".into(), "Bullet 1".into(), "Bullet 2".into()];
        let b_lines = a_lines.clone();
        let raw = diff(&a_lines, &b_lines);
        assert!(raw.iter().all(|o| o.typ == OpType::Equal));
    }
}