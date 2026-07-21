//! Content search: greps inside candidate files using the ripgrep engine
//! (grep-searcher), with binary detection and size caps so it stays fast.

use crate::search::Hit;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// Max file size we're willing to grep.
const MAX_GREP_SIZE: u64 = 16 * 1024 * 1024;
/// Max number of candidate files to grep per search.
pub const MAX_GREP_FILES: usize = 50_000;

/// Filter `hits` down to files whose content matches `pattern` (treated as a
/// literal unless `as_regex`). Each surviving hit gains its first matching line.
pub fn filter_by_content(
    hits: Vec<Hit>,
    pattern: &str,
    as_regex: bool,
    case_sensitive: bool,
    generation: u64,
    current: &AtomicU64,
) -> Option<Vec<Hit>> {
    let pattern_src = if as_regex {
        pattern.to_string()
    } else {
        regex::escape(pattern)
    };
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!case_sensitive)
        .build(&pattern_src)
        .ok()?;
    // Plain regex for searching text extracted from documents (PDF/Office).
    let doc_regex = regex::RegexBuilder::new(&pattern_src)
        .case_insensitive(!case_sensitive)
        .build()
        .ok()?;

    let cancelled = || current.load(Ordering::Relaxed) != generation;

    let results: Vec<Option<Hit>> = hits
        .into_par_iter()
        .map(|hit| {
            if cancelled() {
                return None;
            }
            if hit.is_dir || hit.size > MAX_GREP_SIZE {
                return None;
            }

            // Documents (PDF, DOCX, PPTX, XLSX, ODF): search extracted text.
            if crate::doctext::is_document(&hit.name) {
                let text = crate::doctext::extract_text(std::path::Path::new(&hit.path))?;
                let found = text.lines().enumerate().find_map(|(i, line)| {
                    doc_regex.is_match(line).then(|| {
                        (i as u64 + 1, line.trim().chars().take(300).collect::<String>())
                    })
                });
                return found.map(|line| Hit {
                    content_line: Some(line),
                    ..hit
                });
            }

            // Everything else: grep the raw file (skips binaries).
            let mut searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .line_number(true)
                .build();
            let mut found: Option<(u64, String)> = None;
            let sink = UTF8(|line_num, line| {
                found = Some((line_num, line.trim_end().chars().take(300).collect()));
                Ok(false) // stop after the first match
            });
            if searcher.search_path(&matcher, &hit.path, sink).is_err() {
                return None;
            }
            found.map(|line| Hit {
                content_line: Some(line),
                ..hit
            })
        })
        .collect();

    if cancelled() {
        return None;
    }
    Some(results.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit_for(path: &std::path::Path, size: u64) -> Hit {
        Hit {
            idx: 0,
            score: 0,
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            path: path.to_string_lossy().into_owned(),
            is_dir: false,
            size,
            modified: 0,
            content_line: None,
        }
    }

    #[test]
    fn test_content_filter() {
        let tmp = std::env::temp_dir().join(format!("find_grep_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let a = tmp.join("a.txt");
        let b = tmp.join("b.txt");
        std::fs::write(&a, "hello world\nTODO: fix this\n").unwrap();
        std::fs::write(&b, "nothing here\n").unwrap();

        let hits = vec![hit_for(&a, 30), hit_for(&b, 15)];
        let gen = AtomicU64::new(1);
        let out = filter_by_content(hits, "todo", false, false, 1, &gen).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "a.txt");
        let (line_num, line) = out[0].content_line.clone().unwrap();
        assert_eq!(line_num, 2);
        assert!(line.contains("TODO"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_content_filter_inside_docx() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("find_grepdoc_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let docx = tmp.join("memo.docx");
        let file = std::fs::File::create(&docx).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("word/document.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(
            br#"<w:document><w:p><w:r><w:t>The secret launch date is Friday.</w:t></w:r></w:p></w:document>"#,
        )
        .unwrap();
        zip.finish().unwrap();
        let size = std::fs::metadata(&docx).unwrap().len();

        let hits = vec![hit_for(&docx, size)];
        let gen = AtomicU64::new(2);
        let out = filter_by_content(hits, "launch date", false, false, 2, &gen).unwrap();
        assert_eq!(out.len(), 1);
        let (_, line) = out[0].content_line.clone().unwrap();
        assert!(line.contains("secret launch date"));

        // A term that isn't present finds nothing.
        let hits = vec![hit_for(&docx, size)];
        let out = filter_by_content(hits, "absent-term", false, false, 2, &gen).unwrap();
        assert!(out.is_empty());

        std::fs::remove_dir_all(&tmp).ok();
    }
}
