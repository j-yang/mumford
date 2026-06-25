//! Word (.docx) diff: paragraph-level add/delete/modify plus table change counts.
//! Reads word/document.xml from the zip and walks it with a streaming XML reader,
//! tracking paragraphs (<w:p>), tables (<w:tbl>), rows (<w:tr>), cells (<w:tc>)
//! and text runs (<w:t>). Paragraph diff delegates to [`tate::lines::diff`].

use quick_xml::events::Event;
use quick_xml::Reader;
use std::io::Read;
use tate::inline::OpType;
use tate::lines::diff;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DocxParagraph {
    pub index: usize,
    pub text: String,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "String::is_empty"))]
    pub style: String,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DocxTable {
    pub index: usize,
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DocxParaDiff {
    pub index: usize,
    pub old: String,
    #[cfg_attr(feature = "serde", serde(rename = "new"))]
    pub new: String,
}

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DocxResult {
    #[cfg_attr(feature = "serde", serde(rename = "fileType"))]
    pub file_type: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathA"))]
    pub path_a: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathB"))]
    pub path_b: String,
    pub paragraphs: Vec<DocxParagraph>,
    #[cfg_attr(feature = "serde", serde(rename = "addedParagraphs"))]
    pub added_p: Vec<DocxParagraph>,
    #[cfg_attr(feature = "serde", serde(rename = "deletedParagraphs"))]
    pub deleted_p: Vec<DocxParagraph>,
    #[cfg_attr(feature = "serde", serde(rename = "modifiedParagraphs"))]
    pub modified_p: Vec<DocxParaDiff>,
    pub tables: Vec<DocxTable>,
    #[cfg_attr(feature = "serde", serde(rename = "addedTables"))]
    pub added_t: usize,
    #[cfg_attr(feature = "serde", serde(rename = "deletedTables"))]
    pub deleted_t: usize,
    #[cfg_attr(feature = "serde", serde(rename = "modifiedTables"))]
    pub modified_t: usize,
}

pub fn docx_diff(path_a: &str, path_b: &str) -> Result<DocxResult, String> {
    let (paras_a, tables_a) = read_docx(path_a).map_err(|e| format!("read A: {e}"))?;
    let (paras_b, tables_b) = read_docx(path_b).map_err(|e| format!("read B: {e}"))?;

    let mut res = DocxResult {
        file_type: "docx".into(),
        path_a: path_a.into(),
        path_b: path_b.into(),
        paragraphs: paras_a.clone(),
        ..Default::default()
    };

    let text_a: Vec<String> = paras_a.iter().map(|p| p.text.clone()).collect();
    let text_b: Vec<String> = paras_b.iter().map(|p| p.text.clone()).collect();

    let ops = diff(&text_a, &text_b);
    for op in &ops {
        match op.typ {
            OpType::Insert => {
                if op.b < paras_b.len() {
                    res.added_p.push(paras_b[op.b].clone());
                }
            }
            OpType::Delete => {
                if op.a < paras_a.len() {
                    res.deleted_p.push(paras_a[op.a].clone());
                }
            }
            OpType::Equal => {}
            OpType::Replace => {}
        }
    }

    let limit = paras_a.len().min(paras_b.len());
    for i in 0..limit {
        if paras_a[i].text != paras_b[i].text {
            res.modified_p.push(DocxParaDiff {
                index: i,
                old: paras_a[i].text.clone(),
                new: paras_b[i].text.clone(),
            });
        }
    }

    if tables_a.len() != tables_b.len() {
        if tables_b.len() > tables_a.len() {
            res.added_t = tables_b.len() - tables_a.len();
        } else {
            res.deleted_t = tables_a.len() - tables_b.len();
        }
    }
    res.tables = if tables_a.len() < tables_b.len() { tables_b.clone() } else { tables_a.clone() };

    let tlimit = tables_a.len().min(tables_b.len());
    for i in 0..tlimit {
        if !tables_equal(&tables_a[i].rows, &tables_b[i].rows) {
            res.modified_t += 1;
        }
    }

    res.added_p.sort_by_key(|p| p.index);
    res.deleted_p.sort_by_key(|p| p.index);
    res.modified_p.sort_by_key(|p| p.index);

    Ok(res)
}

fn read_docx(path: &str) -> Result<(Vec<DocxParagraph>, Vec<DocxTable>), String> {
    if path.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut xml = String::new();
    {
        let mut entry = zip
            .by_name("word/document.xml")
            .map_err(|e| format!("word/document.xml: {e}"))?;
        entry.read_to_string(&mut xml).map_err(|e| e.to_string())?;
    }
    Ok(parse_document_xml(&xml))
}

fn parse_document_xml(xml: &str) -> (Vec<DocxParagraph>, Vec<DocxTable>) {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut paragraphs: Vec<DocxParagraph> = Vec::new();
    let mut tables: Vec<DocxTable> = Vec::new();
    let mut para_idx = 0usize;

    let mut table_depth = 0i32;
    let mut cur_table_rows: Vec<Vec<String>> = Vec::new();
    let mut cur_row: Vec<String> = Vec::new();
    let mut cur_cell = String::new();
    let mut cur_para = String::new();
    let mut in_text = false;
    let mut text_buf = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "tbl" => {
                        table_depth += 1;
                        if table_depth == 1 {
                            cur_table_rows.clear();
                        }
                    }
                    "tr" if table_depth > 0 => cur_row.clear(),
                    "tc" if table_depth > 0 => cur_cell.clear(),
                    "t" => {
                        in_text = true;
                        text_buf.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if in_text {
                    text_buf.push_str(&t.unescape().unwrap_or_default());
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "t" if in_text => {
                        if table_depth > 0 {
                            cur_cell.push_str(&text_buf);
                        } else {
                            cur_para.push_str(&text_buf);
                        }
                        in_text = false;
                    }
                    "tc" if table_depth > 0 => {
                        cur_row.push(cur_cell.trim().to_string());
                        cur_cell.clear();
                    }
                    "tr" if table_depth > 0 => {
                        cur_table_rows.push(std::mem::take(&mut cur_row));
                    }
                    "tbl" => {
                        table_depth -= 1;
                        if table_depth == 0 {
                            tables.push(DocxTable {
                                index: tables.len(),
                                rows: std::mem::take(&mut cur_table_rows),
                            });
                        }
                    }
                    "p" if table_depth == 0 => {
                        let text = cur_para.trim().to_string();
                        if !text.is_empty() {
                            paragraphs.push(DocxParagraph { index: para_idx, text, style: String::new() });
                            para_idx += 1;
                        }
                        cur_para.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    (paragraphs, tables)
}

fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.into_owned(),
    }
}

fn tables_equal(a: &[Vec<String>], b: &[Vec<String>]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paragraphs_and_tables() {
        let xml = r#"<w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:t>Hello world</w:t></w:r></w:p>
            <w:p><w:r><w:t>Second </w:t><w:t>paragraph</w:t></w:r></w:p>
            <w:tbl>
              <w:tr><w:tc><w:p><w:r><w:t>A1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B1</w:t></w:r></w:p></w:tc></w:tr>
              <w:tr><w:tc><w:p><w:r><w:t>A2</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B2</w:t></w:r></w:p></w:tc></w:tr>
            </w:tbl>
            </w:body></w:document>"#;
        let (paras, tables) = parse_document_xml(xml);
        assert_eq!(paras.len(), 2);
        assert_eq!(paras[0].text, "Hello world");
        assert_eq!(paras[1].text, "Second paragraph");
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows, vec![vec!["A1", "B1"], vec!["A2", "B2"]]);
    }
}