//! Preview pane content loading: text head, images, or plain metadata.

use find_core::search::Hit;
use find_core::util::{human_date, human_size, is_image_ext, is_texty};
use std::io::Read;

const TEXT_PREVIEW_BYTES: usize = 128 * 1024;
const IMAGE_PREVIEW_MAX: u64 = 30 * 1024 * 1024;
const DOC_PREVIEW_MAX: u64 = 8 * 1024 * 1024;

pub enum PreviewContent {
    Empty,
    Text { text: String, truncated: bool },
    Markdown { text: String, truncated: bool },
    /// Image bytes loaded by us: the egui bytes-loader decodes them. (The
    /// file:// URI loader proved unreliable, so we read the file directly.)
    Image {
        uri: String,
        bytes: std::sync::Arc<[u8]>,
    },
    /// Decoded pixels from the OS thumbnail provider (video frames etc).
    Thumbnail {
        uri: String,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        caption: String,
    },
    Info(String),
}

pub fn load(hit: &Hit) -> PreviewContent {
    if hit.is_dir {
        return PreviewContent::Info(format!(
            "Folder\n\n{}\nModified: {}",
            hit.path,
            human_date(hit.modified)
        ));
    }
    if is_image_ext(&hit.name) && hit.size <= IMAGE_PREVIEW_MAX {
        match std::fs::read(&hit.path) {
            Ok(bytes) => {
                return PreviewContent::Image {
                    uri: format!("bytes://preview/{}", hit.path),
                    bytes: std::sync::Arc::from(bytes.into_boxed_slice()),
                }
            }
            Err(_) => return info_for(hit),
        }
    }
    // Audio/video: a real frame from the shell's thumbnail provider (the
    // same one Explorer uses) plus duration/resolution read from the file.
    if find_core::media::is_media(&hit.name) {
        let info = find_core::media::probe(std::path::Path::new(&hit.path));
        let mut caption = String::new();
        if !info.is_empty() {
            caption.push_str(&info.summary());
            caption.push('\n');
        }
        caption.push_str(&format!(
            "{}\n{} • {}\n\nDouble-click to play in your default player.",
            hit.path,
            human_size(hit.size),
            human_date(hit.modified)
        ));
        #[cfg(target_os = "windows")]
        if let Some(thumb) =
            find_core::thumbnail::shell_thumbnail(std::path::Path::new(&hit.path), 512)
        {
            return PreviewContent::Thumbnail {
                uri: format!("thumb://{}", hit.path),
                width: thumb.width,
                height: thumb.height,
                rgba: thumb.rgba,
                caption,
            };
        }
        return PreviewContent::Info(caption);
    }
    // Documents (PDF, DOCX, PPTX, spreadsheets, ODF): preview extracted text.
    if find_core::doctext::is_document(&hit.name) && hit.size <= DOC_PREVIEW_MAX {
        if let Some(mut text) = find_core::doctext::extract_text(std::path::Path::new(&hit.path)) {
            let truncated = text.len() > TEXT_PREVIEW_BYTES;
            if truncated {
                let mut cut = TEXT_PREVIEW_BYTES;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
            }
            return PreviewContent::Text { text, truncated };
        }
    }
    if is_texty(&hit.name) {
        match read_head(&hit.path) {
            Some((bytes, truncated)) => {
                // Refuse binary-looking data even if the extension said text.
                if bytes.iter().take(4096).any(|&b| b == 0) {
                    return info_for(hit);
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if is_markdown(&hit.name) {
                    return PreviewContent::Markdown { text, truncated };
                }
                return PreviewContent::Text { text, truncated };
            }
            None => return info_for(hit),
        }
    }
    info_for(hit)
}

fn is_markdown(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

fn info_for(hit: &Hit) -> PreviewContent {
    // Last resort: the shell may still render this type (PSD, AI, 3D
    // models, CAD, fonts...) via an installed thumbnail handler.
    #[cfg(target_os = "windows")]
    if !hit.is_dir && hit.size > 0 {
        if let Some(thumb) =
            find_core::thumbnail::shell_thumbnail(std::path::Path::new(&hit.path), 512)
        {
            return PreviewContent::Thumbnail {
                uri: format!("thumb://{}", hit.path),
                width: thumb.width,
                height: thumb.height,
                rgba: thumb.rgba,
                caption: format!(
                    "{}\n{} • {}",
                    hit.path,
                    human_size(hit.size),
                    human_date(hit.modified)
                ),
            };
        }
    }
    PreviewContent::Info(format!(
        "{}\n\nSize: {}\nModified: {}\n\nNo preview available for this file type.",
        hit.path,
        human_size(hit.size),
        human_date(hit.modified)
    ))
}

fn read_head(path: &str) -> Option<(Vec<u8>, bool)> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; TEXT_PREVIEW_BYTES + 1];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    let truncated = filled > TEXT_PREVIEW_BYTES;
    buf.truncate(filled.min(TEXT_PREVIEW_BYTES));
    Some((buf, truncated))
}
