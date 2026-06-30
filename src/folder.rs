//! Folder comparison with sha256 content hashing and rename detection.
//!
//! Hashing a file requires reading every byte, so we never hash up front. The
//! walk collects only metadata (size/mtime), and `compare` hashes the few files
//! that actually need it: same-name files whose size matches (size mismatch is
//! an instant "modified"), and rename candidates whose size matches across
//! sides. Those hashes run in parallel (rayon). Added/removed files with a size
//! found nowhere on the other side are never read at all.

use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
struct FileMeta {
    rel: String,
    abs: PathBuf,
    size: u64,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Rename {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Summary {
    #[cfg_attr(feature = "serde", serde(rename = "totalA"))]
    pub total_a: usize,
    #[cfg_attr(feature = "serde", serde(rename = "totalB"))]
    pub total_b: usize,
    pub unchanged: usize,
    pub modified: usize,
    pub added: usize,
    pub removed: usize,
    pub renamed: usize,
}

#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Comparison {
    #[cfg_attr(feature = "serde", serde(rename = "pathA"))]
    pub path_a: String,
    #[cfg_attr(feature = "serde", serde(rename = "pathB"))]
    pub path_b: String,
    pub unchanged: Vec<String>,
    pub modified: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub renamed: Vec<Rename>,
    pub summary: Summary,
}

pub fn compare(path_a: &str, path_b: &str) -> io::Result<Comparison> {
    let files_a = walk(path_a)?;
    let files_b = walk(path_b)?;

    let mut cmp = Comparison {
        path_a: path_a.into(),
        path_b: path_b.into(),
        ..Default::default()
    };

    let a_set: HashMap<&str, &FileMeta> = files_a.iter().map(|f| (f.rel.as_str(), f)).collect();
    let b_set: HashMap<&str, &FileMeta> = files_b.iter().map(|f| (f.rel.as_str(), f)).collect();

    let common: Vec<&str> = a_set.keys().copied().filter(|p| b_set.contains_key(p)).collect();
    let common_set: HashSet<&str> = common.iter().copied().collect();

    let mut to_hash: Vec<&Path> = Vec::new();
    let mut zip_common: Vec<&str> = Vec::new();
    for &p in &common {
        if needs_content_fingerprint(p) {
            zip_common.push(p);
        } else if a_set[p].size != b_set[p].size {
            cmp.modified.push(p.to_string());
        } else {
            to_hash.push(a_set[p].abs.as_path());
            to_hash.push(b_set[p].abs.as_path());
        }
    }

    let a_orphans: Vec<&FileMeta> =
        files_a.iter().filter(|f| !common_set.contains(f.rel.as_str())).collect();
    let b_orphans: Vec<&FileMeta> =
        files_b.iter().filter(|f| !common_set.contains(f.rel.as_str())).collect();
    let mut b_sizes: HashMap<u64, usize> = HashMap::new();
    for f in &b_orphans {
        *b_sizes.entry(f.size).or_insert(0) += 1;
    }
    let mut a_sizes: HashMap<u64, usize> = HashMap::new();
    for f in &a_orphans {
        *a_sizes.entry(f.size).or_insert(0) += 1;
    }
    for f in &a_orphans {
        if b_sizes.contains_key(&f.size) {
            to_hash.push(f.abs.as_path());
        }
    }
    for f in &b_orphans {
        if a_sizes.contains_key(&f.size) {
            to_hash.push(f.abs.as_path());
        }
    }

    to_hash.sort_unstable();
    to_hash.dedup();
    let hashes = hash_many(&to_hash);
    let hash_of = |m: &FileMeta| -> Option<&String> { hashes.get(m.abs.as_path()) };

    for &p in &common {
        if a_set[p].size != b_set[p].size {
            continue;
        }
        match (hash_of(a_set[p]), hash_of(b_set[p])) {
            (Some(ha), Some(hb)) if ha == hb => cmp.unchanged.push(p.to_string()),
            _ => cmp.modified.push(p.to_string()),
        }
    }

    let zip_verdicts: Vec<(&str, bool)> = zip_common
        .par_iter()
        .map(|&p| {
            let fa = content_fingerprint(a_set[p].abs.to_string_lossy().as_ref());
            let fb = content_fingerprint(b_set[p].abs.to_string_lossy().as_ref());
            let same = match (fa, fb) {
                (Some(ha), Some(hb)) => ha == hb,
                _ => hash_file(&a_set[p].abs)
                    .ok()
                    .zip(hash_file(&b_set[p].abs).ok())
                    .map(|(ha, hb)| ha == hb)
                    .unwrap_or(false),
            };
            (p, same)
        })
        .collect();
    for (p, same) in zip_verdicts {
        if same {
            cmp.unchanged.push(p.to_string());
        } else {
            cmp.modified.push(p.to_string());
        }
    }

    let mut b_by_hash: HashMap<&String, &str> = HashMap::new();
    for f in &b_orphans {
        if let Some(h) = hash_of(f) {
            b_by_hash.insert(h, f.rel.as_str());
        }
    }
    let mut renamed_from: HashSet<&str> = HashSet::new();
    let mut renamed_to: HashSet<&str> = HashSet::new();
    for f in &a_orphans {
        if let Some(h) = hash_of(f) {
            if let Some(&b_path) = b_by_hash.get(h) {
                if renamed_to.contains(b_path) {
                    continue;
                }
                cmp.renamed.push(Rename { from: f.rel.clone(), to: b_path.to_string() });
                renamed_from.insert(f.rel.as_str());
                renamed_to.insert(b_path);
            }
        }
    }

    for f in &a_orphans {
        if !renamed_from.contains(f.rel.as_str()) {
            cmp.removed.push(f.rel.clone());
        }
    }
    for f in &b_orphans {
        if !renamed_to.contains(f.rel.as_str()) {
            cmp.added.push(f.rel.clone());
        }
    }

    cmp.unchanged.sort();
    cmp.modified.sort();
    cmp.added.sort();
    cmp.removed.sort();

    cmp.summary = Summary {
        total_a: files_a.len(),
        total_b: files_b.len(),
        unchanged: cmp.unchanged.len(),
        modified: cmp.modified.len(),
        added: cmp.added.len(),
        removed: cmp.removed.len(),
        renamed: cmp.renamed.len(),
    };

    Ok(cmp)
}

fn hash_many(paths: &[&Path]) -> HashMap<PathBuf, String> {
    paths
        .par_iter()
        .filter_map(|p| hash_file(p).ok().map(|h| (p.to_path_buf(), h)))
        .collect()
}

fn walk(root: &str) -> io::Result<Vec<FileMeta>> {
    let mut files = Vec::new();
    let root_path = Path::new(root);
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|e| io::Error::other(e.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root_path)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.to_string_lossy().to_string());
        let meta = entry.metadata().map_err(|e| io::Error::other(e.to_string()))?;
        files.push(FileMeta { rel, abs: path.to_path_buf(), size: meta.len() });
    }
    Ok(files)
}

/// Whether a file is a zip-container format whose raw bytes may change even
/// when content is identical (due to metadata timestamps in docProps/).
fn needs_content_fingerprint(rel: &str) -> bool {
    let lower = rel.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(lower.as_str(), "xlsx" | "xlsm" | "xls" | "docx" | "pptx")
}

/// Content-aware fingerprint for zip-container formats. For Excel, uses
/// cell-value hashing via calamine. For docx/pptx, hashes all zip entries
/// except docProps/ metadata.
fn content_fingerprint(path: &str) -> Option<String> {
    let ext = Path::new(path).extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        "xlsx" | "xlsm" | "xls" => crate::excel::content_fingerprint(path),
        "docx" | "pptx" => zip_content_fingerprint(path),
        _ => None,
    }
}

/// Hash all zip entries except docProps/ metadata. Two files with identical
/// content but different timestamps will produce the same fingerprint.
fn zip_content_fingerprint(path: &str) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).ok()?;
        let name = entry.name().to_string();
        if name.starts_with("docProps/") {
            continue;
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).ok()?;
        entries.push((name, buf));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (name, data) in &entries {
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
        hasher.update(data);
        hasher.update([0x1f]);
    }
    Some(crate::hex(&hasher.finalize()))
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(crate::hex(&hasher.finalize()))
}