//! Files attached to prompts.
//!
//! Importing copies the file into `~/.serechat/attachments/` so a session
//! stays complete when the original moves. Images are sent as images, PDFs
//! as documents, and anything that reads as UTF-8 text is inlined into the
//! prompt, which every model understands.
//!
//! ponytail: no thumbnails; images show as chips until an image decoder is
//! worth a dependency.

use std::fs;
use std::io::Read;
use std::path::Path;

use serechat::{Attachment, Part, data_url, new_session_id};

/// Largest image accepted.
const MAX_IMAGE: u64 = 20 << 20;
/// Largest PDF accepted.
const MAX_DOCUMENT: u64 = 32 << 20;
/// Largest text file inlined into a prompt.
const MAX_TEXT: u64 = 1 << 20;

/// MIME type by file extension, for the types handled specially.
fn mime_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => return None,
    })
}

/// Whether the file starts like UTF-8 text (no NULs, valid encoding).
fn looks_like_text(path: &Path) -> std::io::Result<bool> {
    let mut head = Vec::with_capacity(8192);
    fs::File::open(path)?.take(8192).read_to_end(&mut head)?;
    if head.contains(&0) {
        return Ok(false);
    }
    // A multi-byte character may be cut at the end of the sample.
    Ok(match std::str::from_utf8(&head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && e.valid_up_to() + 4 > head.len(),
    })
}

/// Copies `path` into `store` and describes it, or explains why it can't be attached.
///
/// # Errors
/// A human-readable reason: a folder, too large, unreadable or binary.
pub fn import(path: &Path, store: &Path) -> Result<Attachment, String> {
    let name = path.file_name().map_or_else(|| path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
    let meta = fs::metadata(path).map_err(|e| format!("{name}: {e}"))?;
    if meta.is_dir() {
        return Err(format!("{name} is a folder. Open it as a project instead."));
    }
    let size = meta.len();
    let mime = match mime_for(path) {
        Some(mime) => mime,
        None if looks_like_text(path).map_err(|e| format!("{name}: {e}"))? => "text/plain",
        None => return Err(format!("{name} can't be attached: only images, PDFs and text files are supported.")),
    };
    let limit = match mime {
        "text/plain" => MAX_TEXT,
        "application/pdf" => MAX_DOCUMENT,
        _ => MAX_IMAGE,
    };
    if size > limit {
        return Err(format!("{name} is {}; the limit for this kind of file is {}.", human_size(size), human_size(limit)));
    }
    fs::create_dir_all(store).map_err(|e| format!("{name}: {e}"))?;
    // Keep the name recognisable but safe as a file name everywhere.
    let safe: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).take(80).collect();
    let copy = store.join(format!("{}-{safe}", new_session_id()));
    fs::copy(path, &copy).map_err(|e| format!("{name}: {e}"))?;
    Ok(Attachment { name, mime: mime.to_owned(), size, path: copy.to_string_lossy().into_owned() })
}

/// The API parts of a prompt: its text, then each attachment. Reads the
/// attachment copies, so call it off the UI thread.
///
/// # Errors
/// An attachment copy could not be read.
pub fn parts(text: &str, attachments: &[Attachment]) -> Result<Vec<Part>, String> {
    let mut parts = Vec::with_capacity(attachments.len() + 1);
    if !text.is_empty() || attachments.is_empty() {
        parts.push(Part::Text(text.to_owned()));
    }
    for attachment in attachments {
        let bytes = fs::read(&attachment.path).map_err(|e| format!("The attachment {} could not be read: {e}", attachment.name))?;
        parts.push(if attachment.is_image() {
            Part::Image(data_url(&attachment.mime, &bytes))
        } else if attachment.mime == "application/pdf" {
            Part::File { name: attachment.name.clone(), data_url: data_url(&attachment.mime, &bytes) }
        } else {
            Part::Text(format!("<file name=\"{}\">\n{}\n</file>", attachment.name, String::from_utf8_lossy(&bytes)))
        });
    }
    Ok(parts)
}

/// `1536` -> `1.5 KB`.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 10.0 { format!("{value:.0} {}", UNITS[unit]) } else { format!("{value:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_classifies_and_copies() {
        let dir = std::env::temp_dir().join(format!("serechat-attach-{}", std::process::id()));
        let store = dir.join("store");
        fs::create_dir_all(&dir).unwrap();
        let text = dir.join("notes.md");
        fs::write(&text, "héllo").unwrap();
        let binary = dir.join("blob.bin");
        fs::write(&binary, [0u8, 1, 2]).unwrap();
        let image = dir.join("pic.PNG");
        fs::write(&image, [0x89, b'P', b'N', b'G']).unwrap();

        let a = import(&text, &store).unwrap();
        assert_eq!((a.name.as_str(), a.mime.as_str(), a.size), ("notes.md", "text/plain", 6));
        assert!(Path::new(&a.path).starts_with(&store));
        assert!(import(&binary, &store).unwrap_err().contains("only images"));
        assert!(import(&dir, &store).unwrap_err().contains("folder"));
        let img = import(&image, &store).unwrap();
        assert!(img.is_image());

        let parts = parts("look", &[a, img]).unwrap();
        assert_eq!(parts[0], Part::Text("look".into()));
        assert_eq!(parts[1], Part::Text("<file name=\"notes.md\">\nhéllo\n</file>".into()));
        assert!(matches!(&parts[2], Part::Image(url) if url.starts_with("data:image/png;base64,")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(20 << 20), "20 MB");
    }
}
