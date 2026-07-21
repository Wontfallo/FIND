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
    Image { uri: String },
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
        return PreviewContent::Image {
            uri: format!("file://{}", hit.path),
        };
    }
    // Documents (PDF, DOCX, PPTX, XLSX, ODF): preview their extracted text.
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
                return PreviewContent::Text { text, truncated };
            }
            None => return info_for(hit),
        }
    }
    info_for(hit)
}

fn info_for(hit: &Hit) -> PreviewContent {
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
