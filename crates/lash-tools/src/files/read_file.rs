use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::path::Path;

use lash_core::{ToolCall, ToolDefinition, ToolOutcome, ToolRetryPolicy};

use lash_tool_support::{
    StaticToolExecute, StaticToolProvider, ToolDefinitionBindingExt, execute_typed_tool_result,
    execution_failure, invalid_request_failure, invalid_tool_args, io_failure, non_empty_string,
    resolve_under, run_blocking_value,
};

/// Read files with line-number-prefixed output. Supports images natively.
#[derive(Default)]
pub struct ReadFile;

/// Build the cached `read_file` tool provider.
pub fn read_file_provider() -> StaticToolProvider<ReadFile> {
    StaticToolProvider::new(vec![read_file_tool_definition()], ReadFile)
}

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_LEN: usize = 2000;
const MAX_OUTPUT_BYTES: usize = 50 * 1024;
const MAX_OUTPUT_BYTES_LABEL: &str = "50 KB";

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    /// File path to read.
    path: String,
    /// Line offset to start reading from (1-based).
    #[serde(default = "default_offset")]
    #[schemars(range(min = 1))]
    offset: usize,
    /// Maximum lines to read.
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1))]
    limit: usize,
    /// Attach the file natively using this MIME type instead of extracting or
    /// rejecting it. Provider support is checked when the request is built.
    #[serde(default)]
    attach_as: Option<String>,
}

fn default_offset() -> usize {
    1
}

fn default_limit() -> usize {
    DEFAULT_LIMIT
}

struct FileAttachmentData {
    data: Vec<u8>,
    media_type: lash_core::MediaType,
    type_metadata: Option<lash_core::AttachmentTypeMetadata>,
    label: String,
}

enum ReadFileBlockingResult {
    Tool(ToolOutcome),
    Attachment(FileAttachmentData),
}

impl ReadFileBlockingResult {
    fn tool(result: ToolOutcome) -> Self {
        Self::Tool(result)
    }

    async fn into_tool_result(self, context: &lash_core::AttemptContext<'_>) -> ToolOutcome {
        match self {
            Self::Tool(result) => result,
            Self::Attachment(attachment) => store_attachment(context, attachment).await,
        }
    }
}

#[async_trait::async_trait]
impl StaticToolExecute for ReadFile {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        execute_typed_tool_result::<ReadFileArgs, _, _>(call.args, |args| async move {
            if let Err(err) = non_empty_string(&args.path, "path") {
                return err;
            }
            if args.limit < 1 {
                return invalid_tool_args("Invalid limit: must be >= 1");
            }
            let path_str = args.path;
            let offset = args.offset.max(1);
            let limit = args.limit;
            let attach_as = match args.attach_as {
                Some(value) => match lash_core::MediaType::parse(&value) {
                    Ok(media_type) => Some(media_type),
                    Err(err) => return invalid_tool_args(err.to_string()),
                },
                None => None,
            };

            match run_blocking_value(move || {
                execute_read_file_sync(&path_str, offset, limit, attach_as)
            })
            .await
            {
                Ok(result) => result.into_tool_result(call.context).await,
                Err(err) => err,
            }
        })
        .await
    }
}

fn read_file_tool_definition() -> ToolDefinition {
    ToolDefinition::typed::<ReadFileArgs, String>(
                "tool:read_file",
                "read_file",
                "Read a known file or directory. Text returns lines prefixed as `LINE: text`, directories return concise paginated entry listings, PDFs return extracted text, and five common image formats return visual content. Set `attach_as` to an explicit MIME type to attach another provider-capable file natively. Default: 2000 lines. Use `files.glob` for discovery.",
            )
            .with_examples(vec![
                r#"await files.read({ path: "Cargo.toml" })?"#.into(),
                r#"await files.read({ path: "src/main.rs", offset: 1, limit: 120 })?"#.into(),
            ])
            .with_tool_binding(lash_tool_support::tool_binding(
                ["files"],
                "read",
                &["cat", "view_file"],
            ))
            .with_retry_policy(ToolRetryPolicy::safe(2, 25, 100))
}

fn execute_read_file_sync(
    path_str: &str,
    offset: usize,
    limit: usize,
    attach_as: Option<lash_core::MediaType>,
) -> ReadFileBlockingResult {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => {
            return ReadFileBlockingResult::tool(io_failure(
                "current_dir_failed",
                format!("Failed to determine cwd: {err}"),
            ));
        }
    };
    let path = resolve_under(&cwd, Path::new(path_str));
    let path_str = path.to_string_lossy();
    if !path.exists() {
        return ReadFileBlockingResult::tool(io_failure(
            "path_not_found",
            format!(
                "Path does not exist: {path_str}. Use `files.glob` to locate the correct path."
            ),
        ));
    }

    // Directory reads are intentionally exact: use glob to discover paths,
    // then read a known directory for an immediate paginated entry list.
    if path.is_dir() {
        let output = match read_directory(&path, offset, limit).into_done_output() {
            Ok(output) => output,
            Err(_) => {
                return ReadFileBlockingResult::tool(execution_failure(
                    "directory_listing_pending",
                    "directory listing unexpectedly returned pending output",
                ));
            }
        };
        return ReadFileBlockingResult::tool(ToolOutcome::from_output(output));
    }

    if let Some(media_type) = attach_as {
        return read_native_attachment(&path, &path_str, media_type);
    }

    // Image files — return as visual attachment
    if let Some(mime) = image_mime(&path) {
        return read_image(&path, &path_str, mime);
    }

    // PDF files — extract text via pdf-extract (pure Rust)
    if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
    {
        return ReadFileBlockingResult::tool(read_pdf(&path, &path_str, offset, limit));
    }

    // Binary detection
    if is_likely_binary(&path) {
        return ReadFileBlockingResult::tool(invalid_request_failure(
            "binary_file_requires_explicit_handling",
            format!(
                "Binary file detected: {path_str}. Use image-aware reads for images, or `shell.exec` for binary inspection."
            ),
        ));
    }

    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) => {
            return ReadFileBlockingResult::tool(io_failure(
                "open_file_failed",
                format!("Failed to open file {path_str}: {e}"),
            ));
        }
    };
    let reader = BufReader::new(file);
    let slice = match collect_window(
        reader.lines(),
        offset,
        limit,
        |line_no, line| format!("{line_no}: {line}"),
        "file",
    ) {
        Ok(slice) => slice,
        Err(err) => return ReadFileBlockingResult::tool(err),
    };

    ReadFileBlockingResult::tool(ToolOutcome::ok(json!(render_window(
        &slice,
        WindowKind::Lines
    ))))
}

fn read_directory(path: &Path, offset: usize, limit: usize) -> ToolOutcome {
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
            let slice = match collect_window(
                items.into_iter().map(Ok::<String, std::io::Error>),
                offset,
                limit,
                |_index, entry| entry.to_string(),
                "directory",
            ) {
                Ok(slice) => slice,
                Err(err) => return err,
            };
            ToolOutcome::ok(json!(render_window(&slice, WindowKind::Entries)))
        }
        Err(err) => io_failure(
            "read_directory_failed",
            format!("Failed to read directory {}: {err}", path.display()),
        ),
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

/// Read image metadata. Image bytes must be attached through ToolContext by
/// callers that need model-visible attachments.
fn read_image(path: &Path, path_str: &str, mime: &str) -> ReadFileBlockingResult {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            return ReadFileBlockingResult::tool(io_failure(
                "read_image_failed",
                format!("Failed to read image {path_str}: {e}"),
            ));
        }
    };

    let size_kb = data.len() / 1024;
    let dims = image_dimensions(&data, mime);
    let label = match dims {
        Some((w, h)) => format!("{} ({}KB {}x{})", path_str, size_kb, w, h),
        None => format!("{} ({}KB)", path_str, size_kb),
    };

    let media_type = lash_core::MediaType::parse(mime)
        .expect("built-in image MIME types must be syntactically valid");
    ReadFileBlockingResult::Attachment(FileAttachmentData {
        data,
        media_type,
        type_metadata: Some(lash_core::AttachmentTypeMetadata::image(
            dims.map(|(width, _)| width),
            dims.map(|(_, height)| height),
        )),
        label,
    })
}

fn read_native_attachment(
    path: &Path,
    path_str: &str,
    media_type: lash_core::MediaType,
) -> ReadFileBlockingResult {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) => {
            return ReadFileBlockingResult::tool(io_failure(
                "read_attachment_failed",
                format!("Failed to read native attachment {path_str}: {err}"),
            ));
        }
    };
    let type_metadata = if media_type.is_image() {
        let dimensions = image_dimensions(&data, media_type.as_str());
        Some(lash_core::AttachmentTypeMetadata::image(
            dimensions.map(|(width, _)| width),
            dimensions.map(|(_, height)| height),
        ))
    } else {
        None
    };
    let label = format!("{} ({}KB)", path_str, data.len() / 1024);
    ReadFileBlockingResult::Attachment(FileAttachmentData {
        data,
        media_type,
        type_metadata,
        label,
    })
}

async fn store_attachment(
    context: &lash_core::AttemptContext<'_>,
    attachment: FileAttachmentData,
) -> ToolOutcome {
    let reference = match context
        .attachments()
        .put(
            attachment.data,
            lash_core::AttachmentCreateMeta::new(
                attachment.media_type,
                attachment.type_metadata,
                Some(attachment.label),
            ),
        )
        .await
    {
        Ok(reference) => reference,
        Err(err) => {
            return io_failure(
                "store_attachment_failed",
                format!("Failed to store attachment: {err}"),
            );
        }
    };
    ToolOutcome::from_output(lash_core::ToolCallOutput::success(
        lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(reference)),
    ))
}

/// Extract text from a PDF file using the pdf-extract crate (pure Rust).
fn read_pdf(path: &Path, path_str: &str, offset: usize, limit: usize) -> ToolOutcome {
    let pdf_bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(err) => {
            return io_failure(
                "read_pdf_failed",
                format!("Failed to read PDF {path_str}: {err}"),
            );
        }
    };

    let file_size_kb = pdf_bytes.len() / 1024;

    let text = match pdf_extract::extract_text_from_mem(&pdf_bytes) {
        Ok(t) => t,
        Err(e) => {
            return execution_failure(
                "pdf_extraction_failed",
                format!("Failed to extract text from PDF {path_str}: {e}"),
            );
        }
    };

    let slice = match collect_window(
        text.lines()
            .map(|line| Ok::<String, std::io::Error>(line.to_string())),
        offset,
        limit,
        |line_no, line| format!("{line_no}: {line}"),
        "PDF",
    ) {
        Ok(slice) => slice,
        Err(err) => return err,
    };

    let mut formatted = render_window(&slice, WindowKind::Lines);

    let header = format!(
        "[PDF: {} ({}KB, {} lines extracted)]\n",
        path_str, file_size_kb, slice.total_items
    );
    formatted.insert_str(0, &header);

    ToolOutcome::ok(json!(formatted))
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

struct WindowSlice {
    rendered: Vec<String>,
    total_items: usize,
    shown_start: Option<usize>,
    shown_end: Option<usize>,
    has_more_items: bool,
    truncated_by_bytes: bool,
}

enum WindowKind {
    Lines,
    Entries,
}

fn collect_window<I, E, F>(
    items: I,
    offset: usize,
    limit: usize,
    mut format_item: F,
    item_label: &str,
) -> Result<WindowSlice, ToolOutcome>
where
    I: IntoIterator<Item = Result<String, E>>,
    E: std::fmt::Display,
    F: FnMut(usize, &str) -> String,
{
    let mut total_items = 0usize;
    let mut bytes = 0usize;
    let mut rendered = Vec::new();
    let mut has_more_items = false;
    let mut truncated_by_bytes = false;

    for item in items {
        let item = item.map_err(|err| {
            io_failure(
                "read_item_failed",
                format!("Failed to read {item_label}: {err}"),
            )
        })?;
        total_items += 1;
        if total_items < offset {
            continue;
        }
        if rendered.len() >= limit {
            has_more_items = true;
            continue;
        }

        let item = truncate_line(&item);
        let rendered_item = format_item(total_items, &item);
        let size = rendered_item.len() + usize::from(!rendered.is_empty());
        if bytes + size > MAX_OUTPUT_BYTES {
            truncated_by_bytes = true;
            has_more_items = true;
            break;
        }
        bytes += size;
        rendered.push(rendered_item);
    }

    if total_items < offset && !(total_items == 0 && offset == 1) {
        return Err(invalid_request_failure(
            "offset_out_of_range",
            format!("Offset {offset} is out of range for this {item_label} ({total_items} items)"),
        ));
    }

    let shown_start = (!rendered.is_empty()).then_some(offset);
    let shown_end = shown_start.map(|start| start + rendered.len().saturating_sub(1));

    Ok(WindowSlice {
        rendered,
        total_items,
        shown_start,
        shown_end,
        has_more_items,
        truncated_by_bytes,
    })
}

fn render_window(slice: &WindowSlice, kind: WindowKind) -> String {
    let mut output = slice.rendered.join("\n");
    let Some(shown_start) = slice.shown_start else {
        return output;
    };
    let Some(shown_end) = slice.shown_end else {
        return output;
    };

    let next_offset = shown_end + 1;
    match kind {
        WindowKind::Lines => {
            if slice.truncated_by_bytes {
                output.push_str(&format!(
                    "\n[output capped at {}. Showing lines {}-{}. Use offset={} to continue.]",
                    MAX_OUTPUT_BYTES_LABEL, shown_start, shown_end, next_offset
                ));
            } else if slice.has_more_items {
                output.push_str(&format!(
                    "\n[results truncated: showing lines {}-{} of {}. Use offset={} to continue.]",
                    shown_start, shown_end, slice.total_items, next_offset
                ));
            }
        }
        WindowKind::Entries => {
            if slice.truncated_by_bytes {
                output.push_str(&format!(
                    "\n[output capped at {}. Showing entries {}-{}. Use offset={} to continue.]",
                    MAX_OUTPUT_BYTES_LABEL, shown_start, shown_end, next_offset
                ));
            } else if slice.has_more_items {
                output.push_str(&format!(
                    "\n[results truncated: showing entries {}-{} of {}. Use offset={} to continue.]",
                    shown_start, shown_end, slice.total_items, next_offset
                ));
            }
        }
    }
    output
}

fn truncate_line(line: &str) -> String {
    if line.len() > MAX_LINE_LEN {
        // Slice on a char boundary at or below MAX_LINE_LEN so a multi-byte
        // char straddling the limit doesn't panic ("not a char boundary").
        let end = floor_char_boundary(line, MAX_LINE_LEN);
        format!("{}...", &line[..end])
    } else {
        line.to_string()
    }
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut idx = index;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn read_file_contract_guides_directory_discovery_to_glob() {
        let definition = read_file_tool_definition();
        assert!(definition.manifest.description.contains("files.glob"));
        assert!(!definition.manifest.description.contains("`ls`"));
    }

    #[tokio::test]
    async fn read_file_collapses_dot_segments_end_to_end() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "via dot segments").unwrap();
        let dotted = dir.path().join("subdir/../file.txt");
        let result = lash_core::testing::run_tool(
            &read_file_provider(),
            "read_file",
            &json!({"path": dotted.to_str().unwrap()}),
        )
        .await;
        assert!(result.is_success());
        let value = result.value_for_projection();
        assert!(value.as_str().unwrap().contains("via dot segments"));
    }

    #[tokio::test]
    async fn test_read_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3").unwrap();
        let result = lash_core::testing::run_tool(
            &read_file_provider(),
            "read_file",
            &json!({"path": path.to_str().unwrap()}),
        )
        .await;
        assert!(result.is_success());
        let value = result.value_for_projection();
        let text = value.as_str().unwrap();
        assert!(text.contains("1: line1"));
        assert!(text.contains("2: line2"));
        assert!(text.contains("3: line3"));
        assert!(!text.contains('|'));
    }

    #[tokio::test]
    async fn test_read_with_offset_and_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3\nline4\nline5").unwrap();
        let result = lash_core::testing::run_tool(
            &read_file_provider(),
            "read_file",
            &json!({"path": path.to_str().unwrap(), "offset": 2, "limit": 2}),
        )
        .await;
        assert!(result.is_success());
        let value = result.value_for_projection();
        let text = value.as_str().unwrap();
        assert!(text.contains("2: line2"));
        assert!(text.contains("3: line3"));
        assert!(!text.contains("1: line1"));
        assert!(!text.contains("4: line4"));
        assert!(text.contains("results truncated"));
        assert!(text.contains("offset=4"));
    }

    #[tokio::test]
    async fn test_read_caps_large_output_by_bytes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        let content = (0..200)
            .map(|idx| format!("{idx}: {}", "x".repeat(400)))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, content).unwrap();
        let result = lash_core::testing::run_tool(
            &read_file_provider(),
            "read_file",
            &json!({"path": path.to_str().unwrap(), "limit": 200}),
        )
        .await;
        assert!(result.is_success());
        let value = result.value_for_projection();
        let text = value.as_str().unwrap();
        assert!(text.contains("output capped at 50 KB"));
        assert!(text.contains("Use offset="));
    }

    #[tokio::test]
    async fn test_read_nonexistent() {
        let missing = "/nonexistent/path/to/file.txt";
        let result = lash_core::testing::run_tool(
            &read_file_provider(),
            "read_file",
            &json!({"path": missing}),
        )
        .await;
        let lash_core::ToolCallOutcome::Failure(failure) = &result.as_output().outcome else {
            panic!("missing file must fail");
        };
        assert_eq!(failure.class, lash_core::ToolFailureClass::Io);
        assert_eq!(failure.code, "path_not_found");
        assert!(failure.message.contains(missing));
        assert_eq!(failure.source, lash_core::ToolFailureSource::Tool);
        assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    }

    #[tokio::test]
    async fn explicit_attach_as_enables_native_binary_attachment() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sample.bin");
        std::fs::write(&path, [0, 1, 2, 3]).unwrap();

        let default_result = lash_core::testing::run_tool(
            &read_file_provider(),
            "read_file",
            &json!({"path": path.to_str().unwrap()}),
        )
        .await;
        assert!(
            !default_result.is_success(),
            "default binary behavior stays reject"
        );

        let attached = lash_core::testing::run_tool(
            &read_file_provider(),
            "read_file",
            &json!({
                "path": path.to_str().unwrap(),
                "attach_as": "application/octet-stream"
            }),
        )
        .await;
        assert!(attached.is_success());
        let attachments = attached.as_output().attachments();
        assert_eq!(attachments.len(), 1);
        assert_eq!(
            attachments[0].media_type().unwrap().as_str(),
            "application/octet-stream"
        );
    }

    #[tokio::test]
    async fn test_read_directory_returns_paginated_listing_without_ls_hint() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();
        let result = lash_core::testing::run_tool(
            &read_file_provider(),
            "read_file",
            &json!({"path": dir.path().to_str().unwrap(), "limit": 2}),
        )
        .await;
        assert!(result.is_success());
        let value = result.value_for_projection();
        let text = value.as_str().unwrap();
        assert!(text.contains("a.txt"));
        assert!(text.contains("b.txt"));
        assert!(!text.contains("c.txt"));
        assert!(text.contains("results truncated"));
        assert!(text.contains("offset=3"));
        assert!(!text.contains("ls"));
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

    #[tokio::test]
    async fn test_read_image_returns_attachment_value() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tiny.png");
        let mut data = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        data.extend_from_slice(&[0, 0, 0, 13]);
        data.extend_from_slice(b"IHDR");
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes());
        std::fs::write(&path, &data).unwrap();

        let store = Arc::new(lash_core::facade_support::SessionAttachmentStore::in_memory());
        let host = Arc::new(lash_core::testing::MockSessionManager::default());
        let context = lash_core::ToolContext::__for_testing(
            "test-session".into(),
            host.clone(),
            host.clone(),
            host,
            Arc::new(lash_core::test_support::UnavailableProcessService),
            store.clone(),
            lash_core::facade_support::DirectCompletionClient::from_fn(|_, _| {
                Err(lash_core::PluginError::Session(
                    "direct completions are unavailable in read_file tests".to_string(),
                ))
            }),
            None,
        );
        let result = ReadFile
            .execute(lash_core::ToolCall {
                name: "read_file",
                args: &json!({"path": path.to_str().unwrap()}),
                context: &lash_core::testing::mock_attempt_context_from(&context),
            })
            .await;

        let lash_core::ToolCallOutcome::Success(lash_core::ToolValue::Attachment(
            lash_core::AttachmentSource::Stored { attachment_ref },
        )) = result.into_done_output().expect("read_file result").outcome
        else {
            panic!("expected attachment result");
        };
        assert_eq!(attachment_ref.byte_len, data.len() as u64);
        assert_eq!(
            attachment_ref.type_metadata,
            Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1)))
        );
        assert_eq!(store.get(&attachment_ref.id).await.unwrap().bytes, data);
    }

    #[test]
    fn truncate_line_does_not_split_multibyte_char() {
        // Pad with ASCII so a 3-byte char ('€') straddles the MAX_LINE_LEN
        // byte boundary: bytes [MAX_LINE_LEN - 1 .. MAX_LINE_LEN + 2). A naive
        // `&line[..MAX_LINE_LEN]` slice would panic on a non-char boundary.
        let mut line = "a".repeat(MAX_LINE_LEN - 1);
        line.push('€');
        line.push_str(&"b".repeat(50));
        assert!(line.len() > MAX_LINE_LEN);

        let truncated = truncate_line(&line);

        // Truncation succeeded (no panic) and kept the char-boundary prefix.
        assert!(truncated.ends_with("..."));
        let body = truncated.strip_suffix("...").unwrap();
        // The straddling '€' is dropped entirely, leaving only the padding.
        assert_eq!(body, "a".repeat(MAX_LINE_LEN - 1));
        assert!(body.is_char_boundary(body.len()));
    }
}
