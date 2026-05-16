//! Document extraction helpers for media-aware prompts.

use std::io::Read;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;

const DEFAULT_MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;
const MAX_TEXT_LENGTH: usize = 200_000;

pub fn extract_documents(text: &str, media_paths: &[PathBuf]) -> (String, Vec<PathBuf>) {
    extract_documents_with_limit(text, media_paths, DEFAULT_MAX_FILE_SIZE)
}

pub fn extract_documents_with_limit(
    text: &str,
    media_paths: &[PathBuf],
    max_file_size: u64,
) -> (String, Vec<PathBuf>) {
    let mut image_paths = Vec::new();
    let mut doc_texts = Vec::new();

    for path in media_paths {
        if !path.is_file() {
            continue;
        }
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if meta.len() > max_file_size {
            continue;
        }
        if is_image(path) {
            image_paths.push(path.clone());
            continue;
        }
        if let Some(extracted) = extract_text(path) {
            if !extracted.is_empty() {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
                doc_texts.push(format!("[File: {name}]\n{extracted}"));
            }
        }
    }

    if doc_texts.is_empty() {
        (text.to_string(), image_paths)
    } else {
        (format!("{text}\n\n{}", doc_texts.join("\n\n")), image_paths)
    }
}

fn read_text_lossy(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => truncate(String::from_utf8_lossy(&bytes).to_string()),
        Err(_) => String::new(),
    }
}

fn extract_text(path: &Path) -> Option<String> {
    match extension(path).as_deref()? {
        "pdf" => Some(extract_pdf(path))
            .filter(|text| !text.is_empty())
            .map(truncate),
        "docx" => extract_docx(path).ok().map(truncate),
        "xlsx" => extract_xlsx(path).ok().map(truncate),
        "pptx" => extract_pptx(path).ok().map(truncate),
        _ if is_text_extension(path) => Some(read_text_lossy(path)),
        _ => None,
    }
}

fn extract_pdf(path: &Path) -> String {
    if let Ok(text) = pdf_extract::extract_text(path) {
        if !text.trim().is_empty() {
            return text;
        }
    }
    extract_pdf_text_literals(path)
}

fn extract_pdf_text_literals(path: &Path) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let raw = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    let mut rest = raw.as_ref();
    while let Some(start) = rest.find('(') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find(") Tj") else {
            continue;
        };
        let text = rest[..end]
            .replace(r"\(", "(")
            .replace(r"\)", ")")
            .replace(r"\\", r"\");
        if !text.trim().is_empty() {
            out.push(text);
        }
        rest = &rest[end + 4..];
    }
    out.join("\n")
}

fn extract_docx(path: &Path) -> std::io::Result<String> {
    extract_zip_xml_text(path, |name| {
        name == "word/document.xml"
            || name.starts_with("word/header")
            || name.starts_with("word/footer")
    })
}

fn extract_xlsx(path: &Path) -> std::io::Result<String> {
    extract_zip_xml_text(path, |name| {
        name == "xl/sharedStrings.xml" || name.starts_with("xl/worksheets/")
    })
}

fn extract_pptx(path: &Path) -> std::io::Result<String> {
    extract_zip_xml_text(path, |name| {
        name.starts_with("ppt/slides/") && name.ends_with(".xml")
    })
}

fn extract_zip_xml_text(path: &Path, include: impl Fn(&str) -> bool) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut parts = Vec::new();
    for idx in 0..archive.len() {
        let mut entry = archive.by_index(idx)?;
        if !include(entry.name()) {
            continue;
        }
        let mut xml = String::new();
        entry.read_to_string(&mut xml)?;
        let text = xml_text(&xml);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    Ok(parts.join("\n\n"))
}

fn xml_text(xml: &str) -> String {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Text(text)) => {
                if let Ok(decoded) = text.decode() {
                    let decoded = decoded.trim();
                    if !decoded.is_empty() {
                        out.push(decoded.to_string());
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out.join("\n")
}

fn truncate(text: String) -> String {
    if text.len() <= MAX_TEXT_LENGTH {
        return text;
    }
    format!(
        "{}... (truncated, {} chars total)",
        &text[..MAX_TEXT_LENGTH],
        text.len()
    )
}

fn is_text_extension(path: &Path) -> bool {
    matches!(
        extension(path).as_deref(),
        Some(
            "txt"
                | "md"
                | "csv"
                | "json"
                | "xml"
                | "html"
                | "htm"
                | "log"
                | "yaml"
                | "yml"
                | "toml"
                | "ini"
                | "cfg"
        )
    )
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
}

fn is_image(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let header = bytes.get(..bytes.len().min(16)).unwrap_or(&bytes);
    header.starts_with(b"\x89PNG\r\n\x1a\n")
        || header.starts_with(b"\xff\xd8\xff")
        || header.starts_with(b"GIF87a")
        || header.starts_with(b"GIF89a")
        || header.starts_with(b"RIFF") && header.get(8..12) == Some(b"WEBP")
}
