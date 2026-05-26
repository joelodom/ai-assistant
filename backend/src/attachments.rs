//! Attachment processing. Each Attachment is turned into a text blob the
//! Sanitizer can reason about. Text-ish mimes pass through. PDFs get
//! text-extracted. Images and other binaries become a content-free
//! placeholder noting that one arrived — the prototype Sanitizer is
//! text-only, so visual content can't be inspected for sensitive data
//! and therefore can't safely be stored. Vision-based sanitization is
//! future work.

use base64::Engine;
use shared::Attachment;

const MAX_EXTRACTED_CHARS: usize = 50_000;

pub fn extract_text(a: &Attachment) -> String {
    let mime = a.mime.to_ascii_lowercase();
    if is_text_mime(&mime) {
        return clip(&a.data, MAX_EXTRACTED_CHARS);
    }
    // Decode base64.
    let bytes = match base64::engine::general_purpose::STANDARD.decode(&a.data) {
        Ok(b) => b,
        Err(e) => {
            return format!("[attachment could not be decoded: {e}]");
        }
    };
    if mime == "application/pdf" {
        return extract_pdf(&bytes);
    }
    if mime.starts_with("image/") {
        return format!(
            "[Image attachment received: mime={}, bytes={}. The current Sanitizer is \
             text-only, so the pixel content was NOT inspected for sensitive data and \
             was NOT stored. Tell the user what they uploaded but do not pretend to have \
             seen its contents.]",
            mime,
            bytes.len()
        );
    }
    // Unknown binary. Same pattern as images.
    format!(
        "[Binary attachment received: mime={}, bytes={}. Not processed — only the fact \
         of upload is recorded.]",
        mime,
        bytes.len()
    )
}

fn extract_pdf(bytes: &[u8]) -> String {
    // pdf-extract is sync and can be slow; we accept that for v1 since
    // PDFs are interactively uploaded. If this becomes a problem, wrap
    // in spawn_blocking.
    match pdf_extract::extract_text_from_mem(bytes) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                format!(
                    "[PDF attachment received ({} bytes) — extraction returned no text. \
                     The PDF may be image-based / scanned; OCR is not implemented yet.]",
                    bytes.len()
                )
            } else {
                format!("[PDF text-extracted, {} chars]\n{}", trimmed.len(), clip(trimmed, MAX_EXTRACTED_CHARS))
            }
        }
        Err(e) => format!(
            "[PDF attachment received ({} bytes) — text extraction failed: {e}]",
            bytes.len()
        ),
    }
}

fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/xml"
        || mime == "application/yaml"
        || mime == "message/rfc822"
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}\n[…truncated after {max} chars]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::AttachmentKind;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn plain_text_passes_through() {
        let a = Attachment {
            kind: AttachmentKind::Document,
            data: "hello world".into(),
            mime: "text/plain".into(),
            name: Some("greeting.txt".into()),
        };
        assert_eq!(extract_text(&a), "hello world");
    }

    #[test]
    fn json_passes_through() {
        let a = Attachment {
            kind: AttachmentKind::Document,
            data: r#"{"k":1}"#.into(),
            mime: "application/json".into(),
            name: None,
        };
        assert_eq!(extract_text(&a), r#"{"k":1}"#);
    }

    #[test]
    fn image_yields_placeholder_not_bytes() {
        let bytes = vec![0u8; 256];
        let a = Attachment {
            kind: AttachmentKind::Photo,
            data: b64(&bytes),
            mime: "image/jpeg".into(),
            name: Some("vacation.jpg".into()),
        };
        let out = extract_text(&a);
        assert!(out.contains("Image attachment received"));
        assert!(out.contains("image/jpeg"));
        assert!(out.contains("256"));
        // The output is a small notice — not the entire 256-byte base64 blob.
        assert!(out.len() < 600);
    }

    #[test]
    fn unknown_binary_yields_placeholder() {
        let a = Attachment {
            kind: AttachmentKind::Document,
            data: b64(b"\x00\x01\x02"),
            mime: "application/octet-stream".into(),
            name: Some("weird.bin".into()),
        };
        let out = extract_text(&a);
        assert!(out.contains("Binary attachment received"));
    }

    #[test]
    fn long_text_gets_clipped() {
        let huge = "x".repeat(MAX_EXTRACTED_CHARS + 100);
        let a = Attachment {
            kind: AttachmentKind::Document,
            data: huge,
            mime: "text/plain".into(),
            name: None,
        };
        let out = extract_text(&a);
        assert!(out.contains("truncated"));
        assert!(out.chars().count() < MAX_EXTRACTED_CHARS + 200);
    }
}
