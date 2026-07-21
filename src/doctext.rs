//! Text extraction from binary document formats (PDF, DOCX, PPTX, XLSX, and
//! OpenDocument files), so `content:` searches can look inside them — ported
//! from the author's RipGrep-File-Finder Python app.

use std::io::Read;
use std::path::Path;

/// Extensions we can extract text from (beyond plain text files).
pub fn is_document(name: &str) -> bool {
    matches!(
        crate::util::extension_of(name)
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("pdf" | "docx" | "pptx" | "xlsx" | "odt" | "odp" | "ods")
    )
}

/// Extract readable text from a document. Returns None for unsupported or
/// unparseable files.
pub fn extract_text(path: &Path) -> Option<String> {
    let ext = path
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => extract_pdf(path),
        "docx" => extract_zip_xml(path, ZipSelect::Exact("word/document.xml"), "</w:p>"),
        "pptx" => extract_zip_xml(path, ZipSelect::Prefix("ppt/slides/slide"), "</a:p>"),
        "xlsx" => extract_zip_xml(path, ZipSelect::Exact("xl/sharedStrings.xml"), "</si>"),
        "odt" | "odp" | "ods" => {
            extract_zip_xml(path, ZipSelect::Exact("content.xml"), "</text:p>")
        }
        _ => None,
    }
}

fn extract_pdf(path: &Path) -> Option<String> {
    // pdf-extract can panic on malformed files; contain that.
    let path = path.to_path_buf();
    std::panic::catch_unwind(move || pdf_extract::extract_text(&path).ok())
        .ok()
        .flatten()
}

enum ZipSelect {
    Exact(&'static str),
    Prefix(&'static str),
}

/// Pull the selected XML member(s) out of a zip container and strip markup.
fn extract_zip_xml(path: &Path, select: ZipSelect, para_tag: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;

    let names: Vec<String> = match &select {
        ZipSelect::Exact(name) => vec![(*name).to_string()],
        ZipSelect::Prefix(prefix) => {
            let mut names: Vec<String> = archive
                .file_names()
                .filter(|n| n.starts_with(prefix) && n.ends_with(".xml"))
                .map(String::from)
                .collect();
            // slide1, slide2, ... slide10 — sort numerically where possible.
            names.sort_by_key(|n| {
                n.trim_start_matches(|c: char| !c.is_ascii_digit())
                    .trim_end_matches(".xml")
                    .parse::<u32>()
                    .unwrap_or(u32::MAX)
            });
            names
        }
    };

    let mut out = String::new();
    for name in names {
        let Ok(mut member) = archive.by_name(&name) else {
            continue;
        };
        let mut xml = String::new();
        if member.read_to_string(&mut xml).is_err() {
            continue;
        }
        strip_xml_into(&xml, para_tag, &mut out);
        out.push('\n');
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Remove tags from XML, turning each paragraph-closing tag into a newline
/// and decoding the common entities.
fn strip_xml_into(xml: &str, para_tag: &str, out: &mut String) {
    let with_breaks = xml.replace(para_tag, "\n");
    let mut in_tag = false;
    let mut text = String::with_capacity(with_breaks.len() / 4);
    for c in with_breaks.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    let decoded = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");
    for line in decoded.lines() {
        let line = line.trim();
        if !line.is_empty() {
            out.push_str(line);
            out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_zip(path: &Path, members: &[(&str, &str)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for (name, content) in members {
            zip.start_file(*name, options).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn test_docx_extraction() {
        let tmp = std::env::temp_dir().join(format!("find_doc_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let docx = tmp.join("report.docx");
        make_zip(
            &docx,
            &[(
                "word/document.xml",
                r#"<w:document><w:body><w:p><w:r><w:t>Quarterly Report</w:t></w:r></w:p><w:p><w:r><w:t>Revenue &amp; growth were strong.</w:t></w:r></w:p></w:body></w:document>"#,
            )],
        );
        let text = extract_text(&docx).unwrap();
        assert!(text.contains("Quarterly Report"));
        assert!(text.contains("Revenue & growth"));
        // Paragraphs became separate lines.
        assert!(text.lines().count() >= 2);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_pptx_extraction() {
        let tmp = std::env::temp_dir().join(format!("find_ppt_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let pptx = tmp.join("deck.pptx");
        make_zip(
            &pptx,
            &[
                (
                    "ppt/slides/slide2.xml",
                    r#"<p:sld><a:p><a:r><a:t>Second slide</a:t></a:r></a:p></p:sld>"#,
                ),
                (
                    "ppt/slides/slide1.xml",
                    r#"<p:sld><a:p><a:r><a:t>Title slide</a:t></a:r></a:p></p:sld>"#,
                ),
            ],
        );
        let text = extract_text(&pptx).unwrap();
        let title_pos = text.find("Title slide").unwrap();
        let second_pos = text.find("Second slide").unwrap();
        assert!(title_pos < second_pos, "slides out of order: {text}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_xlsx_extraction() {
        let tmp = std::env::temp_dir().join(format!("find_xls_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let xlsx = tmp.join("data.xlsx");
        make_zip(
            &xlsx,
            &[(
                "xl/sharedStrings.xml",
                r#"<sst><si><t>Customer Name</t></si><si><t>Total Owed</t></si></sst>"#,
            )],
        );
        let text = extract_text(&xlsx).unwrap();
        assert!(text.contains("Customer Name"));
        assert!(text.contains("Total Owed"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_is_document() {
        assert!(is_document("report.PDF"));
        assert!(is_document("deck.pptx"));
        assert!(!is_document("notes.txt"));
        assert!(!is_document("archive.zip"));
    }
}
