//! Small helpers: humanized sizes/dates, case-insensitive matching, file categories.

use std::path::Path;

/// Case-insensitive (ASCII) substring test without allocating.
pub fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    let first = n[0].to_ascii_lowercase();
    for start in 0..=(h.len() - n.len()) {
        if h[start].to_ascii_lowercase() != first {
            continue;
        }
        if h[start..start + n.len()]
            .iter()
            .zip(n)
            .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
        {
            return true;
        }
    }
    false
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Parse a human size like "10mb", "1.5g", "500k", "1024" (bytes).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.trim().parse().ok()?;
    let mult: u64 = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1u64 << 40,
        _ => return None,
    };
    Some((num * mult as f64) as u64)
}

pub fn human_date(unix_secs: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix_secs, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => String::new(),
    }
}

/// Parse "2024-01-31" (or "2024-01" / "2024") into a unix timestamp at local midnight.
pub fn parse_date(s: &str) -> Option<i64> {
    use chrono::{Local, NaiveDate, TimeZone};
    let parts: Vec<&str> = s.split('-').collect();
    let (y, m, d) = match parts.as_slice() {
        [y] => (y.parse().ok()?, 1, 1),
        [y, m] => (y.parse().ok()?, m.parse().ok()?, 1),
        [y, m, d] => (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?),
        _ => return None,
    };
    let date = NaiveDate::from_ymd_opt(y, m, d)?;
    let dt = date.and_hms_opt(0, 0, 0)?;
    Local
        .from_local_datetime(&dt)
        .earliest()
        .map(|t| t.timestamp())
}

pub fn extension_of(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    if dot == 0 || dot + 1 == name.len() {
        return None;
    }
    Some(&name[dot + 1..])
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Category {
    #[default]
    All,
    Folders,
    Files,
    Documents,
    Images,
    Audio,
    Video,
    Archives,
    Code,
    Executables,
}

impl Category {
    pub const ALL: [Category; 10] = [
        Category::All,
        Category::Folders,
        Category::Files,
        Category::Documents,
        Category::Images,
        Category::Audio,
        Category::Video,
        Category::Archives,
        Category::Code,
        Category::Executables,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::All => "All",
            Category::Folders => "Folders",
            Category::Files => "Files",
            Category::Documents => "Documents",
            Category::Images => "Images",
            Category::Audio => "Audio",
            Category::Video => "Video",
            Category::Archives => "Archives",
            Category::Code => "Code",
            Category::Executables => "Executables",
        }
    }

    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Category::Documents => &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "txt",
                "rtf", "md", "epub", "mobi", "csv", "tex", "pages", "numbers", "key",
            ],
            Category::Images => &[
                "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "ico", "svg", "heic",
                "raw", "cr2", "nef", "arw", "psd", "ai", "xcf",
            ],
            Category::Audio => &[
                "mp3", "wav", "flac", "aac", "ogg", "wma", "m4a", "opus", "aiff", "mid", "midi",
            ],
            Category::Video => &[
                "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "3gp",
                "ts", "vob",
            ],
            Category::Archives => &[
                "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "iso", "cab", "arj", "lz",
            ],
            Category::Code => &[
                "rs", "py", "js", "ts", "jsx", "tsx", "c", "h", "cpp", "hpp", "cc", "cs", "java",
                "kt", "go", "rb", "php", "swift", "sh", "bat", "ps1", "cmd", "html", "htm", "css",
                "scss", "json", "xml", "yml", "yaml", "toml", "ini", "sql", "lua", "pl", "r",
                "dart", "scala", "zig", "vue", "svelte",
            ],
            Category::Executables => &[
                "exe", "msi", "dll", "sys", "com", "scr", "app", "deb", "rpm", "appimage", "apk",
                "jar", "bin", "so", "dylib",
            ],
            _ => &[],
        }
    }

    pub fn matches(self, name: &str, is_dir: bool) -> bool {
        match self {
            Category::All => true,
            Category::Folders => is_dir,
            Category::Files => !is_dir,
            other => {
                if is_dir {
                    return false;
                }
                match extension_of(name) {
                    Some(ext) => {
                        let ext = ext.to_ascii_lowercase();
                        other.extensions().iter().any(|e| *e == ext)
                    }
                    None => false,
                }
            }
        }
    }
}

/// Extensions we're willing to preview / grep as text.
pub fn is_texty(name: &str) -> bool {
    match extension_of(name).map(|e| e.to_ascii_lowercase()) {
        Some(ext) => {
            Category::Code.extensions().contains(&ext.as_str())
                || matches!(
                    ext.as_str(),
                    "txt" | "md" | "log" | "csv" | "tsv" | "cfg" | "conf" | "env" | "rst"
                        | "properties" | "gitignore" | "editorconfig" | "dockerfile" | "makefile"
                        | "srt" | "sub" | "vtt" | "nfo" | "reg" | "inf" | "diff" | "patch"
                )
        }
        // Many text files have no extension (Makefile, LICENSE, README).
        None => true,
    }
}

pub fn is_image_ext(name: &str) -> bool {
    matches!(
        extension_of(name)
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "ico" | "tif" | "tiff")
    )
}

pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Default index roots: every drive letter on Windows, home directory elsewhere.
pub fn default_roots() -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        ('A'..='Z')
            .filter_map(|c| {
                let p = std::path::PathBuf::from(format!("{c}:\\"));
                p.exists().then_some(p)
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        directories::UserDirs::new()
            .map(|u| vec![u.home_dir().to_path_buf()])
            .unwrap_or_else(|| vec![std::path::PathBuf::from("/")])
    }
}

pub fn default_exclusions() -> Vec<String> {
    #[cfg(windows)]
    {
        vec![
            "\\$Recycle.Bin".into(),
            "\\System Volume Information".into(),
            "\\Windows\\WinSxS".into(),
            "\\Windows\\servicing".into(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            "/proc".into(),
            "/sys".into(),
            "/dev".into(),
            "/run".into(),
            "/.cache".into(),
        ]
    }
}

pub fn is_excluded(path: &Path, exclusions: &[String]) -> bool {
    if exclusions.is_empty() {
        return false;
    }
    let p = path.to_string_lossy();
    exclusions
        .iter()
        .any(|ex| !ex.is_empty() && contains_ignore_case(&p, ex))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_ignore_case() {
        assert!(contains_ignore_case("Hello World.TXT", "world"));
        assert!(contains_ignore_case("abc", ""));
        assert!(!contains_ignore_case("abc", "abcd"));
        assert!(contains_ignore_case("ABC", "abc"));
    }

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("10mb"), Some(10 * 1024 * 1024));
        assert_eq!(parse_size("1.5kb"), Some(1536));
        assert_eq!(parse_size("100"), Some(100));
        assert_eq!(parse_size("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("abc"), None);
    }

    #[test]
    fn test_extension() {
        assert_eq!(extension_of("foo.txt"), Some("txt"));
        assert_eq!(extension_of("foo"), None);
        assert_eq!(extension_of(".hidden"), None);
        assert_eq!(extension_of("a.tar.gz"), Some("gz"));
    }

    #[test]
    fn test_category() {
        assert!(Category::Images.matches("photo.JPG", false));
        assert!(!Category::Images.matches("photo.jpg", true));
        assert!(Category::Folders.matches("stuff", true));
        assert!(Category::All.matches("anything", false));
    }

    #[test]
    fn test_thousands() {
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(12), "12");
    }
}
