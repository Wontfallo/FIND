//! Search execution: runs a parsed `SearchSpec` over the index in parallel,
//! scores matches, and returns the top results.

use crate::index::Index;
use crate::query::{MatchMode, SearchSpec, TypeFilter};
use crate::util::{contains_ignore_case, extension_of, Category};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug)]
pub struct Hit {
    pub idx: u32,
    pub score: u32,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: i64,
    /// First matching line for content searches: (line number, text).
    pub content_line: Option<(u64, String)>,
}

pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    /// Total matches before truncation to `max_results`.
    pub total: usize,
    pub truncated: bool,
}

/// Run the search. `generation`/`current` implement cancellation: if the shared
/// counter moves past this search's generation, we bail out early.
pub fn execute(
    index: &Index,
    spec: &SearchSpec,
    category: Category,
    max_results: usize,
    generation: u64,
    current: &AtomicU64,
) -> Option<SearchOutcome> {
    let cancelled = || current.load(Ordering::Relaxed) != generation;

    // Empty query with no category: show the index in natural order (cheap).
    if spec.is_empty() && category == Category::All {
        let hits: Vec<Hit> = index
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_deleted())
            .take(max_results)
            .map(|(i, _)| make_hit(index, i as u32, 0, None))
            .collect();
        let total = index.live_count();
        return Some(SearchOutcome {
            truncated: total > hits.len(),
            total,
            hits,
        });
    }

    let regex = if spec.mode == MatchMode::Regex && !spec.name_terms.is_empty() {
        let joined = spec.name_terms.join(" ");
        match regex::RegexBuilder::new(&joined)
            .case_insensitive(!spec.case_sensitive)
            .build()
        {
            Ok(r) => Some(r),
            Err(_) => return Some(SearchOutcome { hits: vec![], total: 0, truncated: false }),
        }
    } else {
        None
    };

    let fuzzy_pattern = if spec.mode == MatchMode::Fuzzy && !spec.name_terms.is_empty() {
        let joined = spec.name_terms.join(" ");
        let case = if spec.case_sensitive {
            CaseMatching::Respect
        } else {
            CaseMatching::Ignore
        };
        Some(Pattern::parse(&joined, case, Normalization::Smart))
    } else {
        None
    };

    let mut scored: Vec<(u32, u32)> = index
        .entries
        .par_iter()
        .enumerate()
        .map_init(
            || Matcher::new(Config::DEFAULT),
            |matcher, (i, entry)| {
                if i & 0x3FF == 0 && cancelled() {
                    return None;
                }
                if entry.is_deleted() {
                    return None;
                }
                let is_dir = entry.is_dir();
                match spec.type_filter {
                    TypeFilter::File if is_dir => return None,
                    TypeFilter::Folder if !is_dir => return None,
                    _ => {}
                }
                if !category.matches(&entry.name, is_dir) {
                    return None;
                }
                if !spec.exts.is_empty() {
                    let ext = extension_of(&entry.name).map(|e| e.to_ascii_lowercase());
                    match ext {
                        Some(e) if spec.exts.iter().any(|x| *x == e) => {}
                        _ => return None,
                    }
                }
                if let Some(min) = spec.size_min {
                    if is_dir || entry.size < min {
                        return None;
                    }
                }
                if let Some(max) = spec.size_max {
                    if is_dir || entry.size > max {
                        return None;
                    }
                }
                if let Some(min) = spec.date_min {
                    if entry.modified < min {
                        return None;
                    }
                }
                if let Some(max) = spec.date_max {
                    if entry.modified >= max {
                        return None;
                    }
                }

                let score = match spec.mode {
                    MatchMode::Substring => substring_score(&entry.name, spec)?,
                    MatchMode::Fuzzy => match &fuzzy_pattern {
                        Some(pattern) => {
                            let mut buf = Vec::new();
                            let hay = Utf32Str::new(&entry.name, &mut buf);
                            pattern.score(hay, matcher)?
                        }
                        None => 1,
                    },
                    MatchMode::Regex => match &regex {
                        Some(r) => {
                            if r.is_match(&entry.name) {
                                1000u32.saturating_sub(entry.name.len() as u32)
                            } else {
                                return None;
                            }
                        }
                        None => 1,
                    },
                };

                // Path filter: reconstruct the full path only when required.
                if spec.needs_path() {
                    let path = index.full_path_string(i as u32);
                    for term in &spec.path_terms {
                        let ok = if spec.case_sensitive {
                            path.contains(term.as_str())
                        } else {
                            contains_ignore_case(&path, term)
                        };
                        if !ok {
                            return None;
                        }
                    }
                }

                Some((i as u32, score))
            },
        )
        .flatten()
        .collect();

    if cancelled() {
        return None;
    }

    let total = scored.len();
    scored.par_sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(max_results);

    let hits: Vec<Hit> = scored
        .into_iter()
        .map(|(idx, score)| make_hit(index, idx, score, None))
        .collect();

    if cancelled() {
        return None;
    }

    Some(SearchOutcome {
        truncated: total > hits.len(),
        total,
        hits,
    })
}

fn make_hit(index: &Index, idx: u32, score: u32, content_line: Option<(u64, String)>) -> Hit {
    let entry = &index.entries[idx as usize];
    let path = index.full_path_string(idx);
    Hit {
        idx,
        score,
        name: entry.name.to_string(),
        path,
        is_dir: entry.is_dir(),
        size: entry.size,
        modified: entry.modified,
        content_line,
    }
}

/// All terms must appear in the name. Better scores for prefix matches and
/// shorter names, so exact-ish hits float to the top.
fn substring_score(name: &str, spec: &SearchSpec) -> Option<u32> {
    if spec.name_terms.is_empty() {
        return Some(1);
    }
    let mut score = 0u32;
    for term in &spec.name_terms {
        let matched = if spec.case_sensitive {
            name.contains(term.as_str())
        } else {
            contains_ignore_case(name, term)
        };
        if !matched {
            return None;
        }
        let prefix = if spec.case_sensitive {
            name.starts_with(term.as_str())
        } else {
            name.len() >= term.len()
                && name.as_bytes()[..term.len()]
                    .iter()
                    .zip(term.as_bytes())
                    .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
        };
        score += if prefix { 500 } else { 100 };
        // Whole-name match is best.
        if name.len() == term.len() && matched {
            score += 1000;
        }
    }
    score += 200u32.saturating_sub(name.len().min(200) as u32);
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    fn test_index() -> (Index, PathBuf) {
        let tmp = std::env::temp_dir().join(format!("find_search_test_{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("docs")).unwrap();
        std::fs::write(tmp.join("report_2024.pdf"), vec![0u8; 100]).unwrap();
        std::fs::write(tmp.join("notes.txt"), b"hello").unwrap();
        std::fs::write(tmp.join("docs").join("summary_report.txt"), b"x").unwrap();
        let progress = AtomicUsize::new(0);
        let cancel = AtomicBool::new(false);
        let index = crate::index::scan(&[tmp.clone()], &[], &progress, &cancel);
        (index, tmp)
    }

    fn run(index: &Index, query: &str, mode: MatchMode) -> SearchOutcome {
        let spec = parse(query, mode, false);
        let gen = AtomicU64::new(7);
        execute(index, &spec, Category::All, 100, 7, &gen).unwrap()
    }

    #[test]
    fn test_substring_search() {
        let (index, tmp) = test_index();
        let out = run(&index, "report", MatchMode::Substring);
        assert_eq!(out.total, 2);
        assert!(out.hits.iter().any(|h| h.name == "report_2024.pdf"));
        assert!(out.hits.iter().any(|h| h.name == "summary_report.txt"));

        let out = run(&index, "report ext:pdf", MatchMode::Substring);
        assert_eq!(out.total, 1);
        assert_eq!(out.hits[0].name, "report_2024.pdf");

        let out = run(&index, "report path:docs", MatchMode::Substring);
        assert_eq!(out.total, 1);
        assert_eq!(out.hits[0].name, "summary_report.txt");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_fuzzy_and_regex() {
        let (index, tmp) = test_index();
        let out = run(&index, "rpt2024", MatchMode::Fuzzy);
        assert!(out.hits.iter().any(|h| h.name == "report_2024.pdf"));

        let out = run(&index, r"^notes\.(txt|md)$", MatchMode::Regex);
        assert_eq!(out.total, 1);
        assert_eq!(out.hits[0].name, "notes.txt");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_type_filter() {
        let (index, tmp) = test_index();
        let out = run(&index, "docs type:folder", MatchMode::Substring);
        assert!(out.hits.iter().all(|h| h.is_dir));
        assert!(out.hits.iter().any(|h| h.name == "docs"));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
