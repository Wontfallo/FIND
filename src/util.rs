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

/// Development/cache noise nobody wants cluttering their search results.
/// A pattern without a path separator matches a whole path component exactly
/// (case-insensitive); a pattern with separators is a substring match on the
/// full path. All of these are editable in Settings.
pub fn default_exclusions() -> Vec<String> {
    let mut list: Vec<String> = [
        // Version control internals
        ".git", ".hg", ".svn",
        // Node / JS
        "node_modules", "bower_components", ".npm", ".yarn", ".pnpm-store",
        ".next", ".nuxt", ".angular", ".parcel-cache", ".turbo",
        // Python
        ".venv", "venv", "virtualenv", "__pycache__", ".tox", ".nox",
        ".mypy_cache", ".pytest_cache", ".ruff_cache", ".ipynb_checkpoints",
        "site-packages", "__pypackages__", ".eggs",
        // Conda (any env or package cache, wherever the install lives)
        ".conda", "anaconda3/envs", "anaconda3/pkgs", "miniconda3/envs",
        "miniconda3/pkgs", "mambaforge/envs", "mambaforge/pkgs",
        // Docker
        ".docker",
        // Other build/dependency caches
        ".gradle", ".m2", ".nuget", ".terraform", ".bundle", ".ccache",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    #[cfg(windows)]
    list.extend(
        [
            "$Recycle.Bin",
            "System Volume Information",
            "\\Windows\\WinSxS",
            "\\Windows\\servicing",
            "\\Windows\\SoftwareDistribution",
            "\\AppData\\Local\\Temp",
            "\\AppData\\Local\\Microsoft\\Windows\\INetCache",
            "\\AppData\\Local\\npm-cache",
            "\\AppData\\Local\\pip",
            "\\.cargo\\registry",
            "\\.rustup\\toolchains",
        ]
        .into_iter()
        .map(String::from),
    );

    #[cfg(not(windows))]
    list.extend(
        [
            "/proc",
            "/sys",
            "/dev",
            "/run",
            "/snap",
            ".cache",
            "/.cargo/registry",
            "/.rustup/toolchains",
            ".local/share/Trash",
        ]
        .into_iter()
        .map(String::from),
    );

    list
}

/// Pre-split exclusion patterns for fast repeated matching during scans.
pub struct ExclusionMatcher {
    /// Patterns matched against whole path components (no separators).
    components: Vec<String>,
    /// Patterns matched as substrings of the full path (contain separators).
    substrings: Vec<String>,
}

impl ExclusionMatcher {
    pub fn new(exclusions: &[String]) -> Self {
        let mut components = Vec::new();
        let mut substrings = Vec::new();
        for ex in exclusions {
            let ex = ex.trim();
            if ex.is_empty() {
                continue;
            }
            if ex.contains('/') || ex.contains('\\') {
                substrings.push(ex.to_string());
            } else {
                components.push(ex.to_ascii_lowercase());
            }
        }
        ExclusionMatcher {
            components,
            substrings,
        }
    }

    pub fn matches(&self, path: &Path) -> bool {
        if !self.components.is_empty() {
            for comp in path.components() {
                if let std::path::Component::Normal(os) = comp {
                    let name = os.to_string_lossy();
                    if self
                        .components
                        .iter()
                        .any(|c| name.eq_ignore_ascii_case(c))
                    {
                        return true;
                    }
                }
            }
        }
        if !self.substrings.is_empty() {
            let p = path.to_string_lossy();
            if self
                .substrings
                .iter()
                .any(|s| path_contains_segment(&p, s))
            {
                return true;
            }
        }
        false
    }
}

/// Case-insensitive substring match that only matches at path-component
/// boundaries ("/proc" matches "/proc/cpuinfo" but not "/process.txt"), with
/// `/` and `\` treated as interchangeable so patterns work cross-platform.
fn path_contains_segment(haystack: &str, pattern: &str) -> bool {
    let h = haystack.as_bytes();
    let n = pattern.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return false;
    }
    let is_sep = |b: u8| b == b'/' || b == b'\\';
    let eq = |a: u8, b: u8| {
        a.to_ascii_lowercase() == b.to_ascii_lowercase() || (is_sep(a) && is_sep(b))
    };
    for start in 0..=(h.len() - n.len()) {
        if h[start..start + n.len()].iter().zip(n).all(|(&a, &b)| eq(a, b)) {
            let end = start + n.len();
            if end == h.len() || is_sep(h[end]) {
                return true;
            }
        }
    }
    false
}

pub fn is_excluded(path: &Path, exclusions: &[String]) -> bool {
    if exclusions.is_empty() {
        return false;
    }
    ExclusionMatcher::new(exclusions).matches(path)
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

    #[test]
    fn test_exclusion_matcher() {
        let patterns: Vec<String> = vec![
            ".git".into(),
            "node_modules".into(),
            "/proc".into(),
        ];
        let m = ExclusionMatcher::new(&patterns);
        // Component patterns match whole components only.
        assert!(m.matches(Path::new("/home/me/proj/.git/config")));
        assert!(m.matches(Path::new("/home/me/app/node_modules")));
        assert!(m.matches(Path::new("/home/me/app/NODE_MODULES/x.js")));
        assert!(!m.matches(Path::new("/home/me/proj/.github/workflows/ci.yml")));
        assert!(!m.matches(Path::new("/home/me/proj/.gitignore")));
        // Separator patterns are path substrings.
        assert!(m.matches(Path::new("/proc/cpuinfo")));
        assert!(!m.matches(Path::new("/home/me/process.txt")));
    }
}
