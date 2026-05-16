use std::io::Write;
use std::path::PathBuf;

use zip::write::SimpleFileOptions;
use zip::ZipWriter;
use zunel_core::extract_documents_with_limit;

#[test]
fn extract_documents_appends_text_files_and_keeps_images_as_media() {
    let tmp = tempfile::tempdir().unwrap();
    let doc = tmp.path().join("notes.md");
    let image = tmp.path().join("image.png");
    std::fs::write(&doc, "# Notes\n\nremember this").unwrap();
    std::fs::write(&image, b"\x89PNG\r\n\x1a\npng-bytes").unwrap();

    let (text, media) = extract_documents_with_limit(
        "hello",
        &[doc.clone(), image.clone(), tmp.path().join("missing.txt")],
        1024,
    );

    assert!(text.contains("hello"));
    assert!(text.contains("[File: notes.md]"));
    assert!(text.contains("remember this"));
    assert_eq!(media, vec![image]);
}

#[test]
fn extract_documents_skips_oversized_files() {
    let tmp = tempfile::tempdir().unwrap();
    let doc = tmp.path().join("large.txt");
    std::fs::write(&doc, "too large").unwrap();

    let (text, media) = extract_documents_with_limit("hello", &[PathBuf::from(&doc)], 1);

    assert_eq!(text, "hello");
    assert!(media.is_empty());
}

#[test]
fn extract_documents_reads_pdf_docx_xlsx_and_pptx_text() {
    let tmp = tempfile::tempdir().unwrap();
    let pdf = tmp.path().join("sample.pdf");
    let docx = tmp.path().join("sample.docx");
    let xlsx = tmp.path().join("sample.xlsx");
    let pptx = tmp.path().join("sample.pptx");

    std::fs::write(&pdf, simple_pdf("hello pdf")).unwrap();
    write_zip(
        &docx,
        &[(
            "word/document.xml",
            r#"<w:document><w:body><w:p><w:r><w:t>hello docx</w:t></w:r></w:p></w:body></w:document>"#,
        )],
    );
    write_zip(
        &xlsx,
        &[(
            "xl/sharedStrings.xml",
            r#"<sst><si><t>hello xlsx</t></si></sst>"#,
        )],
    );
    write_zip(
        &pptx,
        &[(
            "ppt/slides/slide1.xml",
            r#"<p:sld><p:cSld><p:spTree><a:t>hello pptx</a:t></p:spTree></p:cSld></p:sld>"#,
        )],
    );

    let (text, media) =
        extract_documents_with_limit("start", &[pdf, docx, xlsx, pptx], 1024 * 1024);

    assert!(media.is_empty());
    assert!(text.contains("[File: sample.pdf]"), "{text}");
    assert!(text.contains("hello pdf"), "{text}");
    assert!(text.contains("[File: sample.docx]"), "{text}");
    assert!(text.contains("hello docx"), "{text}");
    assert!(text.contains("[File: sample.xlsx]"), "{text}");
    assert!(text.contains("hello xlsx"), "{text}");
    assert!(text.contains("[File: sample.pptx]"), "{text}");
    assert!(text.contains("hello pptx"), "{text}");
}

#[test]
fn extract_documents_truncates_large_extracted_text() {
    let tmp = tempfile::tempdir().unwrap();
    let doc = tmp.path().join("large.txt");
    std::fs::write(&doc, "a".repeat(210_000)).unwrap();

    let (text, media) = extract_documents_with_limit("start", &[doc], 1024 * 1024);

    assert!(media.is_empty());
    assert!(text.contains("truncated, 210000 chars total"), "{text}");
    assert!(text.len() < 205_000, "text was not capped: {}", text.len());
}

fn write_zip(path: &std::path::Path, entries: &[(&str, &str)]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = ZipWriter::new(file);
    for (name, content) in entries {
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

fn simple_pdf(text: &str) -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> /MediaBox [0 0 612 792] /Contents 5 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!(
            "<< /Length {} >>\nstream\nBT /F1 24 Tf 100 700 Td ({}) Tj ET\nendstream",
            text.len() + 36,
            text
        ),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (idx, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", idx + 1, object).as_bytes());
    }
    let xref_start = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            objects.len() + 1,
            xref_start
        )
        .as_bytes(),
    );
    out
}
