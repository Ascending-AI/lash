use serde_json::json;
use std::path::Path;

use crate::{ToolDefinition, ToolImage, ToolParam, ToolProvider, ToolResult};

use super::hashline;
use super::{read_to_string, require_str, run_blocking};

/// Read files with hashline-prefixed output. Supports images natively.
#[derive(Default)]
pub struct ReadFile;

impl ReadFile {
    pub fn new() -> Self {
        Self
    }
}

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_LEN: usize = 2000;
/// Max text file size we'll read (1 MB). Larger files must use offset/limit.
const MAX_TEXT_BYTES: u64 = 1_000_000;
const TRUNCATION_HINT: &str = "Pass `limit=null` for all lines.";

#[async_trait::async_trait]
impl ToolProvider for ReadFile {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "read_file".into(),
            description: format!(
                "Read a file (default: up to {} lines). Text files return hashline-prefixed content in `LINE:8HEX|text` format. PDF files are extracted to text. Image files (png, jpg, gif, webp, bmp) are read for visual inspection. Use `ls` for directories.",
                DEFAULT_LIMIT
            )
            .into(),
            params: vec![
                ToolParam::typed("path", "str"),
                ToolParam {
                    name: "offset".into(),
                    r#type: "int".into(),
                    description: "Line offset to start reading from (1-based)".into(),
                    required: false,
                },
                ToolParam {
                    name: "limit".into(),
                    r#type: "int".into(),
                    description: format!(
                        "Maximum lines to read (default: {}). Use null or \"none\" for no cap.",
                        DEFAULT_LIMIT
                    )
                    .into(),
                    required: false,
                },
            ],
            returns: "str".into(),
            examples: vec![],
                hidden: false,
                inject_into_prompt: true,
        }]
    }

    async fn execute(&self, _name: &str, args: &serde_json::Value) -> ToolResult {
        let path_str = match require_str(args, "path") {
            Ok(s) => s.to_string(),
            Err(e) => return e,
        };

        let offset = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(1)
            .max(1);

        let limit = match parse_limit(args) {
            Ok(l) => l,
            Err(e) => return e,
        };

        run_blocking(move || execute_read_file_sync(&path_str, offset, limit)).await
    }
}

fn parse_limit(args: &serde_json::Value) -> Result<Option<usize>, ToolResult> {
    match args.get("limit") {
        None => Ok(Some(DEFAULT_LIMIT)),
        Some(v) if v.is_null() => Ok(None),
        Some(v) => {
            if let Some(s) = v.as_str() {
                if s.eq_ignore_ascii_case("none") {
                    return Ok(None);
                }
                return Err(ToolResult::err_fmt(format_args!(
                    "Invalid limit: expected int, null, or \"none\""
                )));
            }

            let n = match v.as_u64() {
                Some(n) => n,
                None => {
                    return Err(ToolResult::err_fmt(format_args!(
                        "Invalid limit: expected int, null, or \"none\""
                    )));
                }
            };

            if n == 0 {
                return Err(ToolResult::err_fmt(format_args!(
                    "Invalid limit: must be >= 1, or use null/\"none\" for no cap"
                )));
            }
            Ok(Some(n as usize))
        }
    }
}

fn execute_read_file_sync(path_str: &str, offset: usize, limit: Option<usize>) -> ToolResult {
    let path = Path::new(path_str);
    if !path.exists() {
        return ToolResult::err_fmt(format_args!(
            "Path does not exist: {path_str}. Use `ls` or `glob` to locate the correct path."
        ));
    }

    // Directory — still works but nudges toward ls
    if path.is_dir() {
        let mut result = list_directory(path);
        if result.success
            && let serde_json::Value::String(ref mut s) = result.result
        {
            s.insert_str(0, "(Hint: use `ls` for directory listings.)\n");
        }
        return result;
    }

    // Image files — return as visual attachment
    if let Some(mime) = image_mime(path) {
        return read_image(path, path_str, mime);
    }

    // PDF files — extract text via pdf-extract (pure Rust)
    if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
    {
        return read_pdf(path, path_str, offset, limit);
    }

    // Binary detection
    if is_likely_binary(path) {
        return ToolResult::err_fmt(format_args!(
            "Binary file detected: {path_str}. Use `read_image` for images, or `shell` (`file`, `strings`, `xxd`) for binary inspection."
        ));
    }

    // Check file size before reading
    let file_size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) => return ToolResult::err_fmt(format_args!("Failed to stat file: {e}")),
    };
    if file_size > MAX_TEXT_BYTES {
        return ToolResult::err_fmt(format_args!(
            "File too large ({file_size} bytes, max {MAX_TEXT_BYTES}). Use offset and limit parameters to read in chunks."
        ));
    }

    let content = match read_to_string(path) {
        Ok(c) => c,
        Err(e) => return e,
    };

    let all_lines: Vec<&str> = content.lines().collect();
    let total_lines = all_lines.len();

    // offset is 1-based
    let start_idx = (offset - 1).min(total_lines);
    let end_idx = match limit {
        Some(limit) => (start_idx + limit).min(total_lines),
        None => total_lines,
    };
    let selected: Vec<&str> = all_lines[start_idx..end_idx].to_vec();

    // Truncate long lines and format with hashlines
    let truncated_content: String = selected
        .iter()
        .map(|line| {
            if line.len() > MAX_LINE_LEN {
                format!("{}...", &line[..MAX_LINE_LEN])
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut formatted = hashline::format_hashlines(&truncated_content, offset);

    append_truncation_notice(&mut formatted, start_idx, end_idx, total_lines);

    ToolResult::ok(json!(formatted))
}

fn append_truncation_notice(
    formatted: &mut String,
    start_idx: usize,
    end_idx: usize,
    total_lines: usize,
) {
    if end_idx >= total_lines {
        return;
    }
    let shown_start = start_idx + 1;
    let shown_end = end_idx;
    let omitted = total_lines - end_idx;
    let next_offset = end_idx + 1;
    formatted.push_str(&format!(
        "\n[results truncated: showing lines {}-{} of {} ({} more lines). Use offset={} to continue. {}]",
        shown_start, shown_end, total_lines, omitted, next_offset, TRUNCATION_HINT
    ));
}

fn list_directory(path: &Path) -> ToolResult {
    match std::fs::read_dir(path) {
        Ok(entries) => {
            let mut items: Vec<String> = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    items.push(format!("{}/", name));
                } else {
                    items.push(name);
                }
            }
            items.sort();
            ToolResult::ok(json!(items.join("\n")))
        }
        Err(e) => ToolResult::err_fmt(format_args!("Failed to read directory: {e}")),
    }
}

/// Simple binary detection: check first 8KB for null bytes.
fn is_likely_binary(path: &Path) -> bool {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; 8192];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    buf[..n].contains(&0)
}

/// Return the MIME type for supported image extensions.
fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// Read an image file, extract dimensions from the header, and return as a ToolImage.
fn read_image(path: &Path, path_str: &str, mime: &str) -> ToolResult {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return ToolResult::err_fmt(format_args!("Failed to read image: {e}")),
    };

    let size_kb = data.len() / 1024;
    let dims = image_dimensions(&data, mime);
    let label = match dims {
        Some((w, h)) => format!("{} ({}KB {}x{})", path_str, size_kb, w, h),
        None => format!("{} ({}KB)", path_str, size_kb),
    };

    let image = ToolImage {
        mime: mime.to_string(),
        data,
        label: label.clone(),
    };

    ToolResult::with_images(true, json!(format!("[Image: {}]", label)), vec![image])
}

/// Extract text from a PDF file using the pdf-extract crate (pure Rust).
fn read_pdf(path: &Path, path_str: &str, offset: usize, limit: Option<usize>) -> ToolResult {
    let pdf_bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return ToolResult::err_fmt(format_args!("Failed to read PDF: {e}")),
    };

    let file_size_kb = pdf_bytes.len() / 1024;

    let text = match pdf_extract::extract_text_from_mem(&pdf_bytes) {
        Ok(t) => t,
        Err(e) => {
            return ToolResult::err_fmt(format_args!(
                "Failed to extract text from PDF {path_str}: {e}"
            ));
        }
    };

    let all_lines: Vec<&str> = text.lines().collect();
    let total_lines = all_lines.len();
    let start_idx = (offset - 1).min(total_lines);
    let end_idx = match limit {
        Some(limit) => (start_idx + limit).min(total_lines),
        None => total_lines,
    };
    let selected = &all_lines[start_idx..end_idx];

    let truncated: String = selected
        .iter()
        .map(|line| {
            if line.len() > MAX_LINE_LEN {
                format!("{}...", &line[..MAX_LINE_LEN])
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut formatted = hashline::format_hashlines(&truncated, offset);

    let header = format!(
        "[PDF: {} ({}KB, {} lines extracted)]\n",
        path_str, file_size_kb, total_lines
    );
    formatted.insert_str(0, &header);

    append_truncation_notice(&mut formatted, start_idx, end_idx, total_lines);

    ToolResult::ok(json!(formatted))
}

/// Extract width x height from image headers (zero deps).
fn image_dimensions(data: &[u8], mime: &str) -> Option<(u32, u32)> {
    match mime {
        "image/png" => png_dimensions(data),
        "image/jpeg" => jpeg_dimensions(data),
        "image/gif" => gif_dimensions(data),
        _ => None,
    }
}

/// PNG: width at bytes 16-19, height at bytes 20-23 (IHDR chunk, big-endian).
fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 {
        return None;
    }
    // Verify PNG signature
    if &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    Some((w, h))
}

/// JPEG: scan for SOF0/SOF2 marker (0xFF 0xC0 or 0xFF 0xC2), height then width.
fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        // SOF0 (0xC0) or SOF2 (0xC2) — baseline or progressive
        if marker == 0xC0 || marker == 0xC2 {
            if i + 9 >= data.len() {
                return None;
            }
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return Some((w, h));
        }
        // Skip non-SOF markers
        if marker == 0xD8 || marker == 0xD9 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
        } else if i + 3 < data.len() {
            let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
            i += 2 + len;
        } else {
            break;
        }
    }
    None
}

/// GIF: width at bytes 6-7, height at bytes 8-9 (little-endian).
fn gif_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 10 {
        return None;
    }
    // Verify GIF signature
    if &data[..3] != b"GIF" {
        return None;
    }
    let w = u16::from_le_bytes([data[6], data[7]]) as u32;
    let h = u16::from_le_bytes([data[8], data[9]]) as u32;
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolProvider;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_read_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3").unwrap();
        let tool = ReadFile;
        let result = tool
            .execute("read_file", &json!({"path": path.to_str().unwrap()}))
            .await;
        assert!(result.success);
        let text = result.result.as_str().unwrap();
        assert!(text.contains("|line1"));
        assert!(text.contains("|line2"));
        assert!(text.contains("|line3"));
    }

    #[tokio::test]
    async fn test_read_with_offset_and_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3\nline4\nline5").unwrap();
        let tool = ReadFile;
        let result = tool
            .execute(
                "read_file",
                &json!({"path": path.to_str().unwrap(), "offset": 2, "limit": 2}),
            )
            .await;
        assert!(result.success);
        let text = result.result.as_str().unwrap();
        assert!(text.contains("|line2"));
        assert!(text.contains("|line3"));
        assert!(!text.contains("|line1"));
        assert!(!text.contains("|line4"));
        assert!(text.contains("results truncated"));
        assert!(text.contains("2 more lines"));
        assert!(text.contains("offset=4"));
    }

    #[tokio::test]
    async fn test_read_limit_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3").unwrap();
        let tool = ReadFile;
        let result = tool
            .execute(
                "read_file",
                &json!({"path": path.to_str().unwrap(), "limit": null}),
            )
            .await;
        assert!(result.success);
        let text = result.result.as_str().unwrap();
        assert!(text.contains("|line1"));
        assert!(text.contains("|line2"));
        assert!(text.contains("|line3"));
        assert!(!text.contains("results truncated"));
    }

    #[tokio::test]
    async fn test_read_nonexistent() {
        let tool = ReadFile;
        let result = tool
            .execute(
                "read_file",
                &json!({"path": "/nonexistent/path/to/file.txt"}),
            )
            .await;
        assert!(!result.success);
    }

    // ── PNG dimensions ──

    #[test]
    fn test_png_dimensions_valid() {
        // Minimal valid PNG header (first 24 bytes)
        let mut data = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        // IHDR chunk length (4 bytes)
        data.extend_from_slice(&[0, 0, 0, 13]);
        // IHDR tag
        data.extend_from_slice(b"IHDR");
        // Width: 640 (big-endian)
        data.extend_from_slice(&640u32.to_be_bytes());
        // Height: 480 (big-endian)
        data.extend_from_slice(&480u32.to_be_bytes());
        let (w, h) = png_dimensions(&data).unwrap();
        assert_eq!((w, h), (640, 480));
    }

    #[test]
    fn test_png_dimensions_truncated() {
        assert!(png_dimensions(&[0x89, b'P', b'N', b'G']).is_none());
    }

    #[test]
    fn test_png_dimensions_wrong_sig() {
        let data = vec![0; 24];
        assert!(png_dimensions(&data).is_none());
    }

    // ── JPEG dimensions ──

    #[test]
    fn test_jpeg_dimensions_valid() {
        // Minimal JPEG with SOI + SOF0
        let mut data = vec![0xFF, 0xD8]; // SOI
        // SOF0 marker
        data.extend_from_slice(&[0xFF, 0xC0]);
        // Length (including these 2 bytes)
        data.extend_from_slice(&[0x00, 0x11]);
        // Precision
        data.push(8);
        // Height: 480 (big-endian u16)
        data.extend_from_slice(&480u16.to_be_bytes());
        // Width: 640 (big-endian u16)
        data.extend_from_slice(&640u16.to_be_bytes());
        // Padding to satisfy i+9 < len bounds check
        data.push(0);
        let (w, h) = jpeg_dimensions(&data).unwrap();
        assert_eq!((w, h), (640, 480));
    }

    #[test]
    fn test_jpeg_dimensions_truncated() {
        assert!(jpeg_dimensions(&[0xFF, 0xD8, 0xFF, 0xC0]).is_none());
    }

    // ── GIF dimensions ──

    #[test]
    fn test_gif87a_dimensions() {
        let mut data = b"GIF87a".to_vec();
        // Width: 320 (little-endian u16)
        data.extend_from_slice(&320u16.to_le_bytes());
        // Height: 200 (little-endian u16)
        data.extend_from_slice(&200u16.to_le_bytes());
        let (w, h) = gif_dimensions(&data).unwrap();
        assert_eq!((w, h), (320, 200));
    }

    #[test]
    fn test_gif89a_dimensions() {
        let mut data = b"GIF89a".to_vec();
        data.extend_from_slice(&100u16.to_le_bytes());
        data.extend_from_slice(&50u16.to_le_bytes());
        let (w, h) = gif_dimensions(&data).unwrap();
        assert_eq!((w, h), (100, 50));
    }

    #[test]
    fn test_gif_bad_signature() {
        let data = b"NOT_GIF___".to_vec();
        assert!(gif_dimensions(&data).is_none());
    }

    // ── image_mime ──

    #[test]
    fn test_image_mime() {
        assert_eq!(image_mime(Path::new("photo.png")), Some("image/png"));
        assert_eq!(image_mime(Path::new("photo.jpg")), Some("image/jpeg"));
        assert_eq!(image_mime(Path::new("photo.jpeg")), Some("image/jpeg"));
        assert_eq!(image_mime(Path::new("anim.gif")), Some("image/gif"));
        assert_eq!(image_mime(Path::new("photo.webp")), Some("image/webp"));
        assert_eq!(image_mime(Path::new("photo.bmp")), Some("image/bmp"));
        assert_eq!(image_mime(Path::new("file.txt")), None);
        assert_eq!(image_mime(Path::new("noext")), None);
    }
}
