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

    // Empty query with no category: show the index in natural order (cheap:
    // no scoring, no sort — just the first `max_results` live entries).
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
    // Only the top `max_results` are ever shown, so avoid sorting millions of
    // matches on every keystroke: partition around the cutoff (linear), then
    // sort just that slice. A broad query used to spend hundreds of ms in a
    // full sort — this is what made typing feel laggy.
    let by_rank = |a: &(u32, u32), b: &(u32, u32)| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0));
    if scored.len() > max_results {
        scored.select_nth_unstable_by(max_results, by_rank);
        scored.truncate(max_results);
    }
    scored.sort_unstable_by(by_rank);

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

/// All terms must appear in the name. Ranking (searching "dog"):
///   exact name          "dog"                  best
///   exact stem          "dog.png"
///   word at the start   "dog park.jpg"
///   word anywhere       "my dog photos.png"
///   prefix of a name    "dogecoin.pdf"
///   buried substring    "hotdog_recipes.txt"   worst
/// Shorter names get a small boost as a tiebreaker.
fn substring_score(name: &str, spec: &SearchSpec) -> Option<u32> {
    if spec.name_terms.is_empty() {
        return Some(1);
    }
    let is_boundary = |b: u8| !b.is_ascii_alphanumeric();
    let stem_len = crate::util::extension_of(name)
        .map(|e| name.len() - e.len() - 1)
        .unwrap_or(name.len());

    let mut score = 0u32;
    for term in &spec.name_terms {
        let pos = if spec.case_sensitive {
            name.find(term.as_str())
        } else {
            crate::util::find_ignore_case(name, term)
        };
        let Some(pos) = pos else { return None };
        let end = pos + term.len();
        let start_ok = pos == 0 || is_boundary(name.as_bytes()[pos - 1]);
        let end_ok = end == name.len() || is_boundary(name.as_bytes()[end]);

        score += if pos == 0 && end == name.len() {
            5000 // exact whole name
        } else if pos == 0 && end == stem_len {
            4000 // exact stem: "dog" matches "dog.png"
        } else if pos == 0 && end_ok {
            1500 // whole word at the start
        } else if start_ok && end_ok {
            700 // whole word somewhere inside
        } else if pos == 0 {
            400 // prefix of a longer word
        } else {
            100 // buried substring
        };
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
        // Unique per call: these tests run in parallel in one process, so a
        // pid-keyed directory would be shared and torn down under each other.
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!(
            "find_search_test_{}_{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(tmp.join("docs")).unwrap();
        std::fs::write(tmp.join("report_2024.pdf"), vec![0u8; 100]).unwrap();
        std::fs::write(tmp.join("notes.txt"), b"hello").unwrap();
        std::fs::write(tmp.join("docs").join("summary_report.txt"), b"x").unwrap();
        let progress = AtomicUsize::new(0);
        let cancel = AtomicBool::new(false);
        let completed = std::sync::Mutex::new(std::collections::HashSet::new());
        let index = crate::index::scan(&[tmp.clone()], &[], &progress, &cancel, &completed);
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
    fn test_top_results_correct_when_truncated() {
        // The partial-sort path must still return the best-ranked hits, in
        // order, when there are more matches than max_results.
        let tmp = std::env::temp_dir().join(format!(
            "find_trunc_test_{}_{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("dog.txt"), b"x").unwrap(); // best: exact stem
        std::fs::write(tmp.join("dog run.txt"), b"x").unwrap(); // word at start
        for i in 0..30 {
            std::fs::write(tmp.join(format!("a_hotdog_{i}.txt")), b"x").unwrap();
        }
        let progress = AtomicUsize::new(0);
        let cancel = AtomicBool::new(false);
        let completed = std::sync::Mutex::new(std::collections::HashSet::new());
        let index = crate::index::scan(&[tmp.clone()], &[], &progress, &cancel, &completed);

        let spec = parse("dog", MatchMode::Substring, false);
        let gen = AtomicU64::new(3);
        let out = execute(&index, &spec, Category::All, 2, 3, &gen).unwrap();
        assert_eq!(out.hits.len(), 2);
        assert!(out.truncated);
        assert_eq!(out.total, 32);
        assert_eq!(out.hits[0].name, "dog.txt");
        assert_eq!(out.hits[1].name, "dog run.txt");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_relevance_ranking() {
        let tmp = std::env::temp_dir().join(format!(
            "find_rank_test_{}_{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        for name in [
            "dog.png",
            "dog park.jpg",
            "my dog photos.png",
            "dogecoin_whitepaper.pdf",
            "hotdog_recipes.txt",
        ] {
            std::fs::write(tmp.join(name), b"x").unwrap();
        }
        let progress = AtomicUsize::new(0);
        let cancel = AtomicBool::new(false);
        let completed = std::sync::Mutex::new(std::collections::HashSet::new());
        let index = crate::index::scan(&[tmp.clone()], &[], &progress, &cancel, &completed);
        let out = run(&index, "dog", MatchMode::Substring);
        let names: Vec<&str> = out.hits.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names[0], "dog.png", "exact stem must rank first: {names:?}");
        assert_eq!(names[1], "dog park.jpg", "word-at-start second: {names:?}");
        let buried = names.iter().position(|n| *n == "hotdog_recipes.txt").unwrap();
        let word = names.iter().position(|n| *n == "my dog photos.png").unwrap();
        assert!(word < buried, "word match must beat buried substring: {names:?}");
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
