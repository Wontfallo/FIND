//! Query parsing. Supports Everything-style filter tokens mixed with plain
//! search terms:
//!
//! ```text
//! report 2024 ext:pdf,docx path:projects size:>10mb date:>2024-01-01 type:file content:"todo"
//! ```

use crate::util::{parse_date, parse_size};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum MatchMode {
    #[default]
    Substring,
    Fuzzy,
    Regex,
}

impl MatchMode {
    pub fn label(self) -> &'static str {
        match self {
            MatchMode::Substring => "Substring",
            MatchMode::Fuzzy => "Fuzzy",
            MatchMode::Regex => "Regex",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TypeFilter {
    #[default]
    Any,
    File,
    Folder,
}

#[derive(Clone, Debug, Default)]
pub struct SearchSpec {
    pub mode: MatchMode,
    pub case_sensitive: bool,
    /// Plain name terms (all must match the file name).
    pub name_terms: Vec<String>,
    /// Extension whitelist from `ext:`.
    pub exts: Vec<String>,
    /// Substrings that must appear in the full path, from `path:`.
    pub path_terms: Vec<String>,
    pub size_min: Option<u64>,
    pub size_max: Option<u64>,
    pub date_min: Option<i64>,
    pub date_max: Option<i64>,
    pub type_filter: TypeFilter,
    /// Text to grep for inside matching files, from `content:`.
    pub content: Option<String>,
}

impl SearchSpec {
    pub fn is_empty(&self) -> bool {
        self.name_terms.is_empty()
            && self.exts.is_empty()
            && self.path_terms.is_empty()
            && self.size_min.is_none()
            && self.size_max.is_none()
            && self.date_min.is_none()
            && self.date_max.is_none()
            && self.type_filter == TypeFilter::Any
            && self.content.is_none()
    }

    pub fn needs_path(&self) -> bool {
        !self.path_terms.is_empty()
    }
}

/// Split on whitespace, honoring double quotes: `content:"hello world"`.
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in input.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

pub fn parse(input: &str, mode: MatchMode, case_sensitive: bool) -> SearchSpec {
    let mut spec = SearchSpec {
        mode,
        case_sensitive,
        ..Default::default()
    };

    for token in tokenize(input) {
        let lower = token.to_ascii_lowercase();
        if let Some(rest) = strip_any(&token, &lower, &["ext:", "e:"]) {
            spec.exts.extend(
                rest.split(',')
                    .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|s| !s.is_empty()),
            );
        } else if let Some(rest) = strip_any(&token, &lower, &["path:", "p:", "in:"]) {
            if !rest.is_empty() {
                spec.path_terms.push(rest.to_string());
            }
        } else if let Some(rest) = strip_any(&token, &lower, &["size:", "s:"]) {
            parse_size_filter(rest, &mut spec);
        } else if let Some(rest) = strip_any(&token, &lower, &["date:", "dm:", "modified:"]) {
            parse_date_filter(rest, &mut spec);
        } else if let Some(rest) = strip_any(&token, &lower, &["type:", "t:"]) {
            match rest.to_ascii_lowercase().as_str() {
                "file" | "files" | "f" => spec.type_filter = TypeFilter::File,
                "folder" | "folders" | "dir" | "dirs" | "d" => {
                    spec.type_filter = TypeFilter::Folder
                }
                _ => {}
            }
        } else if let Some(rest) = strip_any(&token, &lower, &["content:", "c:", "grep:"]) {
            if !rest.is_empty() {
                spec.content = Some(rest.to_string());
            }
        } else if !token.is_empty() {
            spec.name_terms.push(token);
        }
    }
    spec
}

/// If `lower` starts with one of the prefixes, return the corresponding suffix
/// of the original (case-preserved) token.
fn strip_any<'a>(original: &'a str, lower: &str, prefixes: &[&str]) -> Option<&'a str> {
    for p in prefixes {
        if lower.starts_with(p) {
            return Some(&original[p.len()..]);
        }
    }
    None
}

fn parse_size_filter(rest: &str, spec: &mut SearchSpec) {
    if let Some((lo, hi)) = rest.split_once("..") {
        spec.size_min = parse_size(lo);
        spec.size_max = parse_size(hi);
    } else if let Some(v) = rest.strip_prefix(">=").or_else(|| rest.strip_prefix('>')) {
        spec.size_min = parse_size(v);
    } else if let Some(v) = rest.strip_prefix("<=").or_else(|| rest.strip_prefix('<')) {
        spec.size_max = parse_size(v);
    } else if let Some(exact) = parse_size(rest) {
        // Bare size: within ~10% either way.
        spec.size_min = Some(exact - exact / 10);
        spec.size_max = Some(exact + exact / 10);
    }
}

fn parse_date_filter(rest: &str, spec: &mut SearchSpec) {
    const DAY: i64 = 86_400;
    if let Some((lo, hi)) = rest.split_once("..") {
        spec.date_min = parse_date(lo);
        spec.date_max = parse_date(hi).map(|t| t + DAY);
    } else if let Some(v) = rest.strip_prefix(">=").or_else(|| rest.strip_prefix('>')) {
        spec.date_min = parse_date(v);
    } else if let Some(v) = rest.strip_prefix("<=").or_else(|| rest.strip_prefix('<')) {
        spec.date_max = parse_date(v);
    } else if let Some(day) = parse_date(rest) {
        spec.date_min = Some(day);
        spec.date_max = Some(day + DAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_terms() {
        let spec = parse("hello world", MatchMode::Substring, false);
        assert_eq!(spec.name_terms, vec!["hello", "world"]);
        assert!(spec.content.is_none());
    }

    #[test]
    fn test_filters() {
        let spec = parse(
            "report ext:pdf,docx path:projects size:>10mb type:file",
            MatchMode::Substring,
            false,
        );
        assert_eq!(spec.name_terms, vec!["report"]);
        assert_eq!(spec.exts, vec!["pdf", "docx"]);
        assert_eq!(spec.path_terms, vec!["projects"]);
        assert_eq!(spec.size_min, Some(10 * 1024 * 1024));
        assert_eq!(spec.type_filter, TypeFilter::File);
    }

    #[test]
    fn test_quoted_content() {
        let spec = parse("content:\"hello world\" ext:rs", MatchMode::Substring, false);
        assert_eq!(spec.content.as_deref(), Some("hello world"));
        assert_eq!(spec.exts, vec!["rs"]);
    }

    #[test]
    fn test_date_range() {
        let spec = parse("date:2024-01-01..2024-12-31", MatchMode::Substring, false);
        assert!(spec.date_min.is_some());
        assert!(spec.date_max.is_some());
        assert!(spec.date_max.unwrap() > spec.date_min.unwrap());
    }

    #[test]
    fn test_size_range() {
        let spec = parse("size:1mb..10mb", MatchMode::Substring, false);
        assert_eq!(spec.size_min, Some(1024 * 1024));
        assert_eq!(spec.size_max, Some(10 * 1024 * 1024));
    }

    #[test]
    fn test_case_preserved_in_terms() {
        let spec = parse("Path:MyDocs README", MatchMode::Substring, false);
        assert_eq!(spec.path_terms, vec!["MyDocs"]);
        assert_eq!(spec.name_terms, vec!["README"]);
    }
}
