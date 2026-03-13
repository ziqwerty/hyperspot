use std::fmt;

use modkit_macros::domain_model;

use crate::domain::error::DomainError;

/// Classification of attachment content (domain-layer enum, no ORM deps).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Document,
    Image,
}

impl fmt::Display for AttachmentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Document => write!(f, "document"),
            Self::Image => write!(f, "image"),
        }
    }
}

/// Validated MIME result: the canonical MIME type string and the attachment kind.
#[domain_model]
pub struct ValidatedMime {
    pub mime: &'static str,
    pub kind: AttachmentKind,
}

/// MIME allowlist: 20 types (19 from spec + image/gif per spec:64).
///
/// Strips charset parameters (e.g., `text/plain; charset=utf-8` → `text/plain`).
/// Rejects `application/octet-stream` and any unlisted types.
///
/// Returns the canonical MIME string and the attachment kind (Document or Image).
pub fn validate_mime(content_type: &str) -> Result<ValidatedMime, DomainError> {
    // Strip charset and other parameters: take only the media type before `;`
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();

    match mime.as_str() {
        // Document types (16)
        "application/pdf" => Ok(ValidatedMime {
            mime: "application/pdf",
            kind: AttachmentKind::Document,
        }),
        "text/plain" => Ok(ValidatedMime {
            mime: "text/plain",
            kind: AttachmentKind::Document,
        }),
        "text/markdown" => Ok(ValidatedMime {
            mime: "text/markdown",
            kind: AttachmentKind::Document,
        }),
        "text/html" => Ok(ValidatedMime {
            mime: "text/html",
            kind: AttachmentKind::Document,
        }),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            Ok(ValidatedMime {
                mime: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                kind: AttachmentKind::Document,
            })
        }
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            Ok(ValidatedMime {
                mime: "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                kind: AttachmentKind::Document,
            })
        }
        "application/json" => Ok(ValidatedMime {
            mime: "application/json",
            kind: AttachmentKind::Document,
        }),
        "text/x-python" => Ok(ValidatedMime {
            mime: "text/x-python",
            kind: AttachmentKind::Document,
        }),
        "text/x-java" => Ok(ValidatedMime {
            mime: "text/x-java",
            kind: AttachmentKind::Document,
        }),
        "text/javascript" => Ok(ValidatedMime {
            mime: "text/javascript",
            kind: AttachmentKind::Document,
        }),
        "text/typescript" => Ok(ValidatedMime {
            mime: "text/typescript",
            kind: AttachmentKind::Document,
        }),
        "text/x-rust" => Ok(ValidatedMime {
            mime: "text/x-rust",
            kind: AttachmentKind::Document,
        }),
        "text/x-go" => Ok(ValidatedMime {
            mime: "text/x-go",
            kind: AttachmentKind::Document,
        }),
        "text/x-csharp" => Ok(ValidatedMime {
            mime: "text/x-csharp",
            kind: AttachmentKind::Document,
        }),
        "text/x-ruby" => Ok(ValidatedMime {
            mime: "text/x-ruby",
            kind: AttachmentKind::Document,
        }),
        "text/x-sql" => Ok(ValidatedMime {
            mime: "text/x-sql",
            kind: AttachmentKind::Document,
        }),
        // Image types (4)
        "image/png" => Ok(ValidatedMime {
            mime: "image/png",
            kind: AttachmentKind::Image,
        }),
        "image/jpeg" => Ok(ValidatedMime {
            mime: "image/jpeg",
            kind: AttachmentKind::Image,
        }),
        "image/webp" => Ok(ValidatedMime {
            mime: "image/webp",
            kind: AttachmentKind::Image,
        }),
        "image/gif" => Ok(ValidatedMime {
            mime: "image/gif",
            kind: AttachmentKind::Image,
        }),
        _ => Err(DomainError::UnsupportedFileType { mime: mime.clone() }),
    }
}

/// Build a structured filename for provider upload: `{chat_id}_{attachment_id}.{ext}`.
///
/// The extension is derived from the validated MIME type. All accepted MIME
/// types have a known extension — unsupported types are rejected before
/// reaching this point.
#[must_use]
pub fn structured_filename(chat_id: uuid::Uuid, attachment_id: uuid::Uuid, mime: &str) -> String {
    let ext = mime_to_extension(mime);
    format!("{chat_id}_{attachment_id}.{ext}")
}

fn mime_to_extension(mime: &str) -> &'static str {
    match mime {
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/html" => "html",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/json" => "json",
        "text/x-python" => "py",
        "text/x-java" => "java",
        "text/javascript" => "js",
        "text/typescript" => "ts",
        "text/x-rust" => "rs",
        "text/x-go" => "go",
        "text/x-csharp" => "cs",
        "text/x-ruby" => "rb",
        "text/x-sql" => "sql",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        // Defensive fallback — should never be reached since we validate MIME types first
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_all_document_types() {
        let doc_types = [
            "application/pdf",
            "text/plain",
            "text/markdown",
            "text/html",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "application/json",
            "text/x-python",
            "text/x-java",
            "text/javascript",
            "text/typescript",
            "text/x-rust",
            "text/x-go",
            "text/x-csharp",
            "text/x-ruby",
            "text/x-sql",
        ];
        for mime in doc_types {
            let result = validate_mime(mime).unwrap_or_else(|_| panic!("should accept {mime}"));
            assert_eq!(result.mime, mime);
            assert!(
                matches!(result.kind, AttachmentKind::Document),
                "{mime} should be Document"
            );
        }
    }

    #[test]
    fn accepts_all_image_types() {
        let img_types = ["image/png", "image/jpeg", "image/webp", "image/gif"];
        for mime in img_types {
            let result = validate_mime(mime).unwrap_or_else(|_| panic!("should accept {mime}"));
            assert_eq!(result.mime, mime);
            assert!(
                matches!(result.kind, AttachmentKind::Image),
                "{mime} should be Image"
            );
        }
    }

    #[test]
    fn total_accepted_types_is_20() {
        let all_types = [
            "application/pdf",
            "text/plain",
            "text/markdown",
            "text/html",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "application/json",
            "text/x-python",
            "text/x-java",
            "text/javascript",
            "text/typescript",
            "text/x-rust",
            "text/x-go",
            "text/x-csharp",
            "text/x-ruby",
            "text/x-sql",
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
        ];
        assert_eq!(all_types.len(), 20);
        for mime in all_types {
            assert!(validate_mime(mime).is_ok(), "should accept {mime}");
        }
    }

    #[test]
    fn strips_charset_parameter() {
        let result = validate_mime("text/plain; charset=utf-8").unwrap();
        assert_eq!(result.mime, "text/plain");
        assert!(matches!(result.kind, AttachmentKind::Document));
    }

    #[test]
    fn strips_multiple_parameters() {
        let result = validate_mime("text/html; charset=utf-8; boundary=something").unwrap();
        assert_eq!(result.mime, "text/html");
    }

    #[test]
    fn case_insensitive() {
        let result = validate_mime("Application/PDF").unwrap();
        assert_eq!(result.mime, "application/pdf");

        let result = validate_mime("IMAGE/PNG").unwrap();
        assert_eq!(result.mime, "image/png");
    }

    #[test]
    fn rejects_octet_stream() {
        assert!(validate_mime("application/octet-stream").is_err());
    }

    #[test]
    fn rejects_unknown_types() {
        assert!(validate_mime("application/xml").is_err());
        assert!(validate_mime("video/mp4").is_err());
        assert!(validate_mime("audio/mpeg").is_err());
        assert!(validate_mime("application/zip").is_err());
        assert!(validate_mime("text/csv").is_err());
    }

    #[test]
    fn rejects_empty_string() {
        assert!(validate_mime("").is_err());
    }

    #[test]
    fn handles_whitespace() {
        let result = validate_mime("  text/plain  ").unwrap();
        assert_eq!(result.mime, "text/plain");
    }

    #[test]
    fn structured_filename_format() {
        let chat = uuid::Uuid::nil();
        let att = uuid::Uuid::nil();
        let name = structured_filename(chat, att, "application/pdf");
        assert!(
            std::path::Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
        );
        assert!(name.contains('_'));
    }

    #[test]
    fn all_mimes_have_extensions() {
        let mimes = [
            "application/pdf",
            "text/plain",
            "text/markdown",
            "text/html",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "application/json",
            "text/x-python",
            "text/x-java",
            "text/javascript",
            "text/typescript",
            "text/x-rust",
            "text/x-go",
            "text/x-csharp",
            "text/x-ruby",
            "text/x-sql",
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
        ];
        for mime in mimes {
            let ext = mime_to_extension(mime);
            assert_ne!(ext, "bin", "MIME {mime} should not fall back to .bin");
        }
    }
}
