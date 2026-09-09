use filetwin_core::{
    Error, ErrorCode, Result, profile,
    worker_protocol::{Encoded, MAX_TEXT_BYTES, Request, encode_text},
};
use pdfium_render::prelude::*;
use quick_xml::{NsReader, events::Event, name::ResolveResult};
use serde_json::json;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};

fn decode_error(e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::DecodeFailed, "document", e.to_string())
}
fn limit() -> Error {
    Error::new(
        ErrorCode::ResourceBudgetTooSmall,
        "document",
        "Document extraction exceeds the profile's bounded reader limits",
    )
}

pub fn encode(request: &Request) -> Result<Encoded> {
    if request.profile_id != profile::document_profile().profile_id {
        return Err(Error::new(
            ErrorCode::UnsupportedFormat,
            "document",
            "This profile has no document extractor",
        ));
    }
    let (text, mut extraction) = match request.format.as_str() {
        "pdf" => pdf(request)?,
        "docx" => docx(request)?,
        _ => {
            return Err(Error::new(
                ErrorCode::UnsupportedFormat,
                "document",
                "Unsupported document format",
            ));
        }
    };
    if !text.chars().any(|c| !c.is_whitespace()) {
        return Err(Error::new(
            ErrorCode::InsufficientContent,
            "document",
            "No extractable body text; scanned PDFs require OCR, which is not enabled",
        ));
    }
    let (vector, characters) = encode_text(text.as_bytes())?;
    extraction["characters"] = json!(characters);
    extraction["utf8_bytes"] = json!(text.len());
    Ok(Encoded {
        family: "text".into(),
        format: request.format.clone(),
        vector,
        extraction,
    })
}

fn pdf(request: &Request) -> Result<(String, serde_json::Value)> {
    let path =
        request.runtime.pdfium_path.as_ref().ok_or_else(|| {
            Error::new(ErrorCode::RuntimeUnavailable, "pdf", "PDFium path missing")
        })?;
    crate::verify_runtime(path, "pdfium")?;
    let pdfium = Pdfium::new(Pdfium::bind_to_library(path).map_err(decode_error)?);
    let document = pdfium
        .load_pdf_from_file(&request.path, None)
        .map_err(decode_error)?;
    let pages = document.pages().len();
    if pages > 10000 {
        return Err(limit());
    }
    let mut body = String::new();
    let mut text_pages = 0;
    for page in document.pages().iter() {
        let text = page.text().map_err(decode_error)?;
        let count = text.len();
        if count < 0 || body.len() as u64 + (count as u64 * 4) + 1 > MAX_TEXT_BYTES {
            return Err(limit());
        }
        let contents = text.all().replace("\r\n", "\n").replace('\r', "\n");
        if contents.chars().any(|c| !c.is_whitespace()) {
            text_pages += 1;
        }
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&contents);
        if body.len() as u64 > MAX_TEXT_BYTES {
            return Err(limit());
        }
    }
    // Mixed scanned/text documents must not silently claim complete extraction.
    if text_pages != pages {
        return Err(Error::new(
            ErrorCode::InsufficientContent,
            "pdf",
            "At least one PDF page has no extractable text; OCR/blank-page classification is required for complete coverage",
        ));
    }
    Ok((
        body,
        json!({"coverage":"complete_extractable_page_text","pages":pages,"pages_with_text":text_pages,"reader":"pdfium-chromium-8044","ocr":false,"reading_order":"pdfium_content_order"}),
    ))
}

fn docx(request: &Request) -> Result<(String, serde_json::Value)> {
    let mut file = File::open(&request.path)?;
    zip_preflight(&mut file)?;
    let mut archive = zip::ZipArchive::new(file).map_err(decode_error)?;
    if archive.len() > 4096 {
        return Err(limit());
    }
    let mut names = std::collections::BTreeSet::new();
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(decode_error)?;
        if !names.insert(entry.name().to_owned()) {
            return Err(decode_error("Duplicate DOCX ZIP member"));
        }
    }
    if !names.contains("[Content_Types].xml") || !names.contains("word/document.xml") {
        return Err(Error::new(
            ErrorCode::UnsupportedFormat,
            "docx",
            "ZIP package is not a DOCX document; generic archives are not content families",
        ));
    }
    let mut xml = String::new();
    let entry = archive.by_name("word/document.xml").map_err(decode_error)?;
    if entry.size() > MAX_TEXT_BYTES {
        return Err(limit());
    }
    entry
        .take(MAX_TEXT_BYTES + 1)
        .read_to_string(&mut xml)
        .map_err(decode_error)?;
    if xml.len() as u64 > MAX_TEXT_BYTES {
        return Err(limit());
    }
    let text = docx_xml(&xml)?;
    Ok((
        text,
        json!({"coverage":"complete_main_document_body","reader":"quick-xml-0.42.0/zip-8.6.0","excluded_parts":["headers","footers","comments","footnotes","deleted_text","field_instructions","textboxes"],"ocr":false}),
    ))
}

fn docx_xml(xml: &str) -> Result<String> {
    let mut reader = NsReader::from_str(xml);
    let mut depth = 0usize;
    let mut body_depth = None;
    let mut skip_depth = None;
    let mut text_depth = None;
    let mut body_seen = false;
    let mut document_seen = false;
    let mut result = String::new();
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(decode_error)?;
        let word = matches!(ns, ResolveResult::Bound(n) if n.as_ref() == "http://schemas.openxmlformats.org/wordprocessingml/2006/main" || n.as_ref() == "http://purl.oclc.org/ooxml/wordprocessingml/main");
        match event {
            Event::Start(e) => {
                depth += 1;
                if depth > 256 {
                    return Err(limit());
                }
                if depth == 1 {
                    if !word || e.local_name().as_ref() != "document" || document_seen {
                        return Err(decode_error("Invalid DOCX document root"));
                    }
                    document_seen = true;
                }
                if word {
                    match e.local_name().as_ref() {
                        "body" if depth == 2 && !body_seen => {
                            body_depth = Some(depth);
                            body_seen = true;
                        }
                        "del" | "txbxContent" if skip_depth.is_none() => skip_depth = Some(depth),
                        "t" if body_depth.is_some() && skip_depth.is_none() => {
                            text_depth = Some(depth)
                        }
                        "tab" if body_depth.is_some() && skip_depth.is_none() => result.push('\t'),
                        "br" | "cr" if body_depth.is_some() && skip_depth.is_none() => {
                            result.push('\n')
                        }
                        _ => (),
                    }
                }
            }
            Event::Empty(e) if word && body_depth.is_some() && skip_depth.is_none() => {
                match e.local_name().as_ref() {
                    "tab" => result.push('\t'),
                    "br" | "cr" | "p" => result.push('\n'),
                    _ => (),
                }
            }
            Event::End(e) => {
                if word
                    && e.local_name().as_ref() == "p"
                    && body_depth.is_some()
                    && skip_depth.is_none()
                {
                    result.push('\n');
                }
                if text_depth == Some(depth) {
                    text_depth = None;
                }
                if skip_depth == Some(depth) {
                    skip_depth = None;
                }
                if body_depth == Some(depth) {
                    body_depth = None;
                }
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| decode_error("Invalid XML nesting"))?;
            }
            Event::Text(e) if text_depth.is_some() => result.push_str(&e.xml10_content()),
            Event::CData(e) if text_depth.is_some() => result.push_str(&e.xml10_content()),
            Event::GeneralRef(e) => {
                // Resolve only numeric and XML's five predefined references.
                // Reject undefined entities even outside body text.
                let ch = e
                    .resolve_char_ref()
                    .map_err(decode_error)?
                    .or_else(|| match e.as_ref() {
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "amp" => Some('&'),
                        "apos" => Some('\''),
                        "quot" => Some('"'),
                        _ => None,
                    })
                    .ok_or_else(|| decode_error("Undefined XML entity"))?;
                if text_depth.is_some() {
                    result.push(ch);
                }
            }
            Event::DocType(_) => {
                return Err(decode_error(
                    "DOCX DTDs and external entities are forbidden",
                ));
            }
            Event::Eof => break,
            _ => (),
        }
        if result.len() as u64 > MAX_TEXT_BYTES {
            return Err(limit());
        }
    }
    if !document_seen || !body_seen || depth != 0 {
        return Err(decode_error("Incomplete DOCX XML"));
    }
    Ok(result)
}

fn zip_preflight(file: &mut File) -> Result<()> {
    let size = file.metadata()?.len();
    let tail_size = size.min(65557);
    file.seek(SeekFrom::Start(size - tail_size))?;
    let mut tail = Vec::new();
    file.take(tail_size).read_to_end(&mut tail)?;
    let eocd = tail
        .windows(22)
        .enumerate()
        .rev()
        .find_map(|(i, h)| {
            (h.starts_with(b"PK\x05\x06")
                && i + 22 + u16::from_le_bytes([h[20], h[21]]) as usize == tail.len())
            .then_some(h)
        })
        .ok_or_else(|| decode_error("Missing ZIP end-of-directory record"))?;
    let entries = u16::from_le_bytes([eocd[10], eocd[11]]);
    let directory_size = u32::from_le_bytes(eocd[12..16].try_into().expect("4 bytes"));
    if eocd[4..8] != [0, 0, 0, 0] || entries == u16::MAX {
        return Err(Error::new(
            ErrorCode::UnsupportedFormat,
            "docx",
            "Multi-volume and ZIP64 DOCX packages are unsupported",
        ));
    }
    if entries > 4096 || directory_size > 8 * 1024 * 1024 {
        return Err(limit());
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn docx_namespaces_entities_structure_and_exclusions() {
        let xml = r#"<d:document xmlns:d="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><d:body><d:p><d:r><d:t>A &amp; B&#33;</d:t><d:tab/><d:t>next</d:t></d:r><d:del><d:r><d:t>deleted</d:t></d:r></d:del></d:p></d:body></d:document>"#;
        assert_eq!(docx_xml(xml).unwrap(), "A & B!\tnext\n");
        assert!(docx_xml("<!DOCTYPE x [<!ENTITY e SYSTEM 'file:///tmp/x'>]><x/>").is_err());
        assert!(docx_xml("<x>").is_err());
    }
}
