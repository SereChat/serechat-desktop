//! Files attached to prompts, and the files generations make.
//!
//! Importing copies the file into `~/.serechat/attachments/` so a session
//! stays complete when the original moves. Generated images, videos and
//! audio are downloaded there too. Images are sent as images, PDFs
//! as documents, and anything that reads as UTF-8 text is inlined into the
//! prompt, which every model understands. PNG and JPEG images show as
//! thumbnails (`image.rs`).
//!
//! ponytail: GIF and WebP images show as chips; add their decoders if they
//! turn out to be common attachments.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serechat::{Attachment, Client, MediaKind, MediaStatus, Part, data_url, new_session_id};

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
    Ok(Attachment { name, mime: mime.to_owned(), size, path: copy.to_string_lossy().into_owned(), dimensions: None })
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

/// Longest the app waits for one generation.
const MAX_WAIT: Duration = Duration::from_secs(30 * 60);
/// Failed status checks in a row before a generation is given up on.
const MAX_FAILED_CHECKS: u32 = 20;

/// Waits for generation `job` of `kind` and downloads its file into `store`
/// as an attachment named after `prompt`. Blocks for as long as the job
/// takes, so run it on a worker thread. `Ok(None)` when `cancel` was raised.
///
/// # Errors
/// The generation failed (the server refunds it), took too long, or its
/// file could not be downloaded or saved.
pub fn fetch_generated(client: &Client, kind: MediaKind, job: &str, prompt: &str, store: &Path, cancel: &AtomicBool) -> Result<Option<Attachment>, String> {
    let started = Instant::now();
    let mut failed_checks = 0;
    let (url, mime) = loop {
        // Quick ones (most images) are noticed fast; long ones polled gently.
        let pause = if started.elapsed() < Duration::from_secs(60) { 2 } else { 5 };
        for _ in 0..pause * 10 {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        match client.media_status(kind, job) {
            Ok(MediaStatus::Pending) => failed_checks = 0,
            Ok(MediaStatus::Succeeded { url, mime }) => break (url, mime),
            Ok(MediaStatus::Failed(reason)) => return Err(reason),
            Err(e) if e.is_retryable() && failed_checks < MAX_FAILED_CHECKS => failed_checks += 1,
            Err(e) => return Err(e.to_string()),
        }
        if started.elapsed() > MAX_WAIT {
            return Err(format!("it was still not done after {} minutes", MAX_WAIT.as_secs() / 60));
        }
    };

    fs::create_dir_all(store).map_err(|e| e.to_string())?;
    let limit = match kind {
        MediaKind::Image => 64 << 20,
        MediaKind::Audio => 256 << 20,
        MediaKind::Video => 1 << 30,
    };
    // The extension follows the type; the type is checked once the file is in.
    let path = store.join(format!("{}-{}", new_session_id(), slug(prompt, kind.noun())));
    let saved = fs::File::create(&path).map_err(|e| e.to_string()).and_then(|mut file| {
        let header = client.download(&url, limit, &mut file).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        Ok(header)
    });
    let header = match saved {
        Ok(header) => header,
        Err(e) => {
            let _ = fs::remove_file(&path);
            return Err(format!("the file could not be saved: {e}"));
        }
    };
    let mime = mime.or(header).map_or_else(|| default_mime(kind).to_owned(), |m| m.split(';').next().unwrap_or_default().trim().to_ascii_lowercase());
    let named = path.with_extension(extension(&mime));
    fs::rename(&path, &named).map_err(|e| e.to_string())?;
    let size = fs::metadata(&named).map_err(|e| e.to_string())?.len();
    let name = named.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // The id prefix keeps copies apart on disk; the name shown leaves it out.
    let name = name.split_once('-').map_or(name.clone(), |(_, rest)| rest.to_owned());
    let dimensions = if crate::image::supported(&mime) { crate::image::dimensions(&named) } else { None };
    Ok(Some(Attachment { name, mime, size, path: named.to_string_lossy().into_owned(), dimensions }))
}

/// A file name (no extension) from the first words of `prompt`, or `fallback`.
fn slug(prompt: &str, fallback: &str) -> String {
    let words: Vec<String> = prompt
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .scan(0, |len, word| {
            *len += word.len() + 1;
            (*len <= 40).then_some(word)
        })
        .collect();
    if words.is_empty() { fallback.to_owned() } else { words.join("-") }
}

/// The type assumed when the server names none.
fn default_mime(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image/png",
        MediaKind::Video => "video/mp4",
        MediaKind::Audio => "audio/mpeg",
    }
}

/// File extension for a MIME type.
fn extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/ogg" => "ogg",
        "audio/flac" => "flac",
        "audio/mp4" | "audio/aac" | "audio/x-m4a" => "m4a",
        _ => "bin",
    }
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
    fn generated_file_names() {
        assert_eq!(slug("A red fox, in the snow!", "image"), "a-red-fox-in-the-snow");
        assert_eq!(slug("   ...", "video"), "video");
        assert!(slug(&"word ".repeat(50), "x").len() <= 40);
        assert_eq!(slug("café ünïcode", "x"), "café-ünïcode");
        assert_eq!((extension("image/svg+xml"), extension("audio/x-wav"), extension("text/html")), ("svg", "wav", "bin"));
        let cancelled = AtomicBool::new(true);
        let store = std::env::temp_dir();
        assert_eq!(fetch_generated(&Client::new(None), MediaKind::Image, "j", "p", &store, &cancelled), Ok(None), "a cancelled wait ends at once");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(20 << 20), "20 MB");
    }
}
