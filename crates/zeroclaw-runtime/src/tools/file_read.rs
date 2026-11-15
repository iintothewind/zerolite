use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult, with_ephemeral_workspace_warning};

/// Whole-file cap for `encoding="base64"`, the one mode that needs every byte.
const MAX_BASE64_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Text files up to this size are scanned to EOF so the footer carries an exact line total.
const EXACT_TOTAL_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Ceiling on the numbered-line body of one text read.
const MAX_OUTPUT_BYTES: usize = 48 * 1024;
/// Floor for the body budget, so a tiny runtime budget still yields a usable window.
const MIN_OUTPUT_BYTES: usize = 1024;
/// Kept free under the runtime budget for the footer (at most 139 bytes) and the
/// ephemeral-workspace banner (385 bytes).
const OUTPUT_RESERVE_BYTES: usize = 1024;
/// Prefix inspected for binary content.
const SNIFF_BYTES: usize = 8192;
const READ_CHUNK_BYTES: usize = 64 * 1024;
const LINE_TRUNCATED_MARKER: &str = " …[truncated]";

/// Read file contents with workspace sandboxing.
pub struct FileReadTool {
    security: Arc<SecurityPolicy>,
    persistent_writes: bool,
    max_output_bytes: usize,
}

impl FileReadTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self {
            security,
            persistent_writes: true,
            max_output_bytes: MAX_OUTPUT_BYTES,
        }
    }

    /// Construct with an explicit persistence flag derived from the active
    /// runtime adapter's `has_filesystem_access()`. Mirrors
    /// [`super::FileWriteTool::new_with_persistence`].
    pub fn new_with_persistence(security: Arc<SecurityPolicy>, persistent_writes: bool) -> Self {
        Self {
            security,
            persistent_writes,
            max_output_bytes: MAX_OUTPUT_BYTES,
        }
    }

    /// Keep each call's output under the runtime's tool-result budget
    /// (`max_tool_result_chars`), so the runtime's head/tail truncation never
    /// removes lines from the middle of a window. `0` means the runtime does not
    /// truncate; the tool's own ceiling still applies.
    pub fn with_runtime_result_budget(mut self, max_tool_result_chars: usize) -> Self {
        self.max_output_bytes = match max_tool_result_chars {
            0 => MAX_OUTPUT_BYTES,
            budget => budget
                .saturating_sub(OUTPUT_RESERVE_BYTES)
                .clamp(MIN_OUTPUT_BYTES, MAX_OUTPUT_BYTES),
        };
        self
    }

    fn resolve_candidate(&self, path: &str) -> anyhow::Result<std::path::PathBuf> {
        if path.contains('\0') {
            anyhow::bail!("Path not allowed: contains null byte");
        }
        if std::path::Path::new(path)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            anyhow::bail!("Path not allowed by security policy: {path}");
        }

        Ok(self.security.resolve_tool_path(path))
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read a text file as numbered lines (\"N: text\"). offset (1-based) and limit select a window of lines, so a file of any size can be read piece by piece. One call returns at most about 48 KB, less under a smaller tool-result budget; when that limit cuts the output, the trailing note gives the offset to continue from. A single line longer than the limit is cut and marked \"…[truncated]\". For large files, search first (content_search) and read only the lines you need. Binary and image files are rejected (use the image_info tool for images). Set encoding=\"base64\" to return raw bytes base64-encoded (for binary files such as .pdf/.xlsx/.docx, up to 10 MB); offset/limit are ignored in that mode."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file. Relative paths resolve from workspace root; absolute paths must be within the workspace."
                },
                "offset": {
                    "type": "integer",
                    "description": "Starting line number (1-based, default: 1). Ignored when encoding is 'base64'."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return (default: as many as fit the output limit). Ignored when encoding is 'base64'."
                },
                "encoding": {
                    "type": "string",
                    "enum": ["utf8", "base64"],
                    "description": "Output encoding (default: 'utf8'). Use 'base64' to read binary files as base64-encoded bytes."
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Base64 reads return a verbatim payload the caller decodes, so they
        // must NOT be annotated — a prepended banner would corrupt decoding.
        // Text reads on an ephemeral runtime may return stale/empty data, so
        // they carry the loud warning instead
        let is_base64 = args.get("encoding").and_then(|v| v.as_str()) == Some("base64");
        let mut result = self.read_path(args).await?;
        if !self.persistent_writes && result.success && !is_base64 {
            result.output.map_text(with_ephemeral_workspace_warning);
        }
        Ok(result)
    }
}

impl FileReadTool {
    /// Resolve, sandbox-check, and read the requested path. The ephemeral
    /// workspace warning is applied by the `Tool::execute` wrapper above.
    async fn read_path(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"param": "path"})),
                "tool argument validation failed"
            );

            anyhow::Error::msg("Missing 'path' parameter")
        })?;

        // Validate and build candidate path using workspace_dir directly.
        let full_path = match self.resolve_candidate(path) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(e.to_string()),
                });
            }
        };

        // Canonicalize to resolve symlinks, then enforce workspace boundary.
        let resolved_path = match tokio::fs::canonicalize(&full_path).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("Failed to resolve file path: {e}")),
                });
            }
        };

        // Read access: workspace + read-write allowlist + read-only allowlist
        // + universal POSIX device files (/dev/null, etc.).
        if !self.security.is_resolved_path_readable(&resolved_path) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Path escapes workspace directory: {path}")),
            });
        }

        // Stat AFTER canonicalization to prevent a TOCTOU symlink bypass. Only base64 rejects on this size; text reads are bounded by what they return.
        let file_len = match tokio::fs::metadata(&resolved_path).await {
            Ok(meta) => meta.len(),
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("Failed to read file metadata: {e}")),
                });
            }
        };

        let encoding = args
            .get("encoding")
            .and_then(|v| v.as_str())
            .unwrap_or("utf8");

        if encoding == "base64" {
            // Binary read: return raw bytes base64-encoded. Line numbering and
            // offset/limit are text concepts and do not apply here.
            if file_len > MAX_BASE64_FILE_BYTES {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "File too large: {file_len} bytes (limit: {MAX_BASE64_FILE_BYTES} bytes)"
                    )),
                });
            }
            // `stat` is not authoritative (devices, files still growing), so the
            // cap is enforced on the bytes actually read.
            let mut bytes = Vec::new();
            let read = match tokio::fs::File::open(&resolved_path).await {
                Ok(file) => {
                    file.take(MAX_BASE64_FILE_BYTES + 1)
                        .read_to_end(&mut bytes)
                        .await
                }
                Err(e) => Err(e),
            };
            if let Err(e) = read {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("Failed to read file: {e}")),
                });
            }
            if bytes.len() as u64 > MAX_BASE64_FILE_BYTES {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "File too large: {}+ bytes (limit: {MAX_BASE64_FILE_BYTES} bytes)",
                        bytes.len()
                    )),
                });
            }
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Ok(ToolResult {
                success: true,
                output: encoded.into(),
                error: None,
            });
        } else if encoding != "utf8" {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Unsupported encoding '{encoding}' (expected 'utf8' or 'base64')"
                )),
            });
        }

        self.read_text_window(&resolved_path, file_len, &args).await
    }

    /// Stream the file and return only the requested line window, bounded by
    /// `max_output_bytes`. Binary content is rejected from the first
    /// [`SNIFF_BYTES`].
    async fn read_text_window(
        &self,
        resolved_path: &Path,
        file_len: u64,
        args: &serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        let first = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .map_or(1, |v| usize::try_from(v.max(1)).unwrap_or(usize::MAX));
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| usize::try_from(v).unwrap_or(usize::MAX));

        let read_failed = |e: std::io::Error| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "path": resolved_path.display().to_string(),
                        "error": format!("{}", e),
                    })),
                "file_read: read failed"
            );
            anyhow::Error::msg(format!("Failed to read file: {e}"))
        };

        let mut file = tokio::fs::File::open(resolved_path)
            .await
            .map_err(read_failed)?;
        let mut sniff = Vec::with_capacity(SNIFF_BYTES);
        (&mut file)
            .take(SNIFF_BYTES as u64)
            .read_to_end(&mut sniff)
            .await
            .map_err(read_failed)?;

        // Reject confident binary instead of returning lossy garbage. An
        // incomplete sequence at the sniff cut is not invalid UTF-8.
        let invalid_utf8 = matches!(std::str::from_utf8(&sniff), Err(e) if e.error_len().is_some());
        let binary = looks_binary(&sniff);
        // Known image formats: point the agent at the image_info tool.
        if (invalid_utf8 || binary)
            && let Some(kind) = detect_image_format(&sniff, file_len)
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Binary image file detected ({kind}): {}. Use the image_info \
                     tool for images, or encoding=\"base64\" to read the raw bytes.",
                    resolved_path.display()
                )),
            });
        }
        // Other confident binary (NUL byte or a glut of control bytes).
        if binary {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Binary file detected: {}. Use encoding=\"base64\" to read the \
                     raw bytes.",
                    resolved_path.display()
                )),
            });
        }

        // Anything else is text; non-UTF-8 encodings (e.g. Windows-1251,
        // Latin-1) are decoded leniently, line by line.
        let mut window = LineWindow::new(
            first,
            limit,
            self.max_output_bytes,
            file_len <= EXACT_TOTAL_MAX_FILE_BYTES,
        );
        let mut stop = window.feed(&sniff);
        let mut eof = sniff.len() < SNIFF_BYTES;
        let mut chunk = vec![0u8; READ_CHUNK_BYTES];
        while !stop && !eof {
            let n = file.read(&mut chunk).await.map_err(read_failed)?;
            if n == 0 {
                eof = true;
            } else {
                stop = window.feed(&chunk[..n]);
            }
        }

        Ok(ToolResult {
            success: true,
            output: window.finish(eof && !stop).render().into(),
            error: None,
        })
    }
}

/// Streams a file's bytes and keeps only the requested line window. Line
/// semantics match `str::lines()`: split on `\n`, strip a `\r` only before a
/// `\n`, and count an unterminated last line.
struct LineWindow {
    /// 1-based first wanted line.
    first: usize,
    /// Last wanted line, inclusive; `first - 1` for an empty window.
    last: usize,
    /// Max bytes of `body`.
    budget: usize,
    /// Keep counting lines after the window closes.
    count_to_eof: bool,
    /// Number of the line the next byte belongs to.
    line_no: usize,
    /// The current line has at least one byte.
    line_has_bytes: bool,
    /// Retained bytes of the current in-window line (at most `budget + 1`).
    cur: Vec<u8>,
    /// Rendered `N: text` lines joined by `\n`.
    body: String,
    emitted: usize,
    budget_hit: bool,
    /// No further line will be emitted.
    closed: bool,
}

struct WindowRead {
    body: String,
    first: usize,
    emitted: usize,
    total: Option<usize>,
    budget_hit: bool,
}

impl LineWindow {
    fn new(first: usize, limit: Option<usize>, budget: usize, count_to_eof: bool) -> Self {
        let last = match limit {
            Some(limit) => first.saturating_add(limit).saturating_sub(1),
            None => usize::MAX,
        };
        let empty = last < first; // limit == 0
        Self {
            first,
            last,
            budget,
            count_to_eof: count_to_eof || empty,
            line_no: 1,
            line_has_bytes: false,
            cur: Vec::new(),
            body: String::new(),
            emitted: 0,
            budget_hit: false,
            closed: empty,
        }
    }

    fn in_window(&self) -> bool {
        !self.closed && self.line_no >= self.first
    }

    /// Consume the next bytes of the file. Returns `true` once reading can stop.
    fn feed(&mut self, mut bytes: &[u8]) -> bool {
        while !bytes.is_empty() {
            if self.closed && !self.count_to_eof {
                return true; // bytes follow the closed window: more lines exist
            }
            let (segment, terminated, rest) = match bytes.iter().position(|&b| b == b'\n') {
                Some(i) => (&bytes[..i], true, &bytes[i + 1..]),
                None => (bytes, false, &bytes[bytes.len()..]),
            };
            if !segment.is_empty() {
                self.line_has_bytes = true;
                if self.in_window() {
                    self.retain(segment);
                }
            }
            if terminated {
                self.end_line();
            }
            bytes = rest;
        }
        self.closed && !self.count_to_eof && self.budget_hit
    }

    fn retain(&mut self, segment: &[u8]) {
        // Nothing longer than the whole budget can be emitted intact, so keep at most
        // one byte past it: enough to know the line overflowed.
        let room = (self.budget + 1).saturating_sub(self.cur.len());
        self.cur
            .extend_from_slice(&segment[..segment.len().min(room)]);
        if self.cur.len() > self.budget {
            // Decide now instead of scanning to a newline that may be gigabytes away.
            self.emit();
            self.cur = Vec::new();
        }
    }

    fn end_line(&mut self) {
        if self.in_window() {
            if self.cur.last() == Some(&b'\r') {
                self.cur.pop();
            }
            self.emit();
        }
        self.cur.clear();
        self.line_no += 1;
        self.line_has_bytes = false;
    }

    fn emit(&mut self) {
        let prefix = format!("{}: ", self.line_no);
        let separator = usize::from(!self.body.is_empty());
        let text = String::from_utf8_lossy(&self.cur);
        if self.body.len() + separator + prefix.len() + text.len() <= self.budget {
            if separator == 1 {
                self.body.push('\n');
            }
            self.body.push_str(&prefix);
            self.body.push_str(&text);
            self.emitted += 1;
            self.closed = self.line_no == self.last;
            return;
        }
        self.budget_hit = true;
        self.closed = true;
        if self.emitted == 0 {
            // A single line larger than the whole budget: return its head, marked.
            let room = self
                .budget
                .saturating_sub(prefix.len() + LINE_TRUNCATED_MARKER.len());
            self.body.push_str(&prefix);
            self.body.push_str(&text[..text.floor_char_boundary(room)]);
            self.body.push_str(LINE_TRUNCATED_MARKER);
            self.emitted = 1;
        }
    }

    fn finish(mut self, reached_eof: bool) -> WindowRead {
        let total = reached_eof.then(|| {
            if self.line_has_bytes {
                // An unterminated last line still counts, and keeps a bare trailing '\r'.
                if self.in_window() {
                    self.emit();
                }
                self.line_no
            } else {
                self.line_no - 1
            }
        });
        WindowRead {
            body: self.body,
            first: self.first,
            emitted: self.emitted,
            total,
            budget_hit: self.budget_hit,
        }
    }
}

impl WindowRead {
    fn render(self) -> String {
        let Self {
            body,
            first,
            emitted,
            total,
            budget_hit,
        } = self;
        if emitted == 0 {
            return match total {
                Some(0) => String::new(),
                Some(t) => format!("[No lines in range, file has {t} lines]"),
                None => "[No lines in range]".to_string(),
            };
        }
        let last = first + emitted - 1;
        let next = last + 1;
        match total {
            Some(t) if budget_hit && last < t => format!(
                "{body}\n[Lines {first}-{last} of {t}; output limit reached, continue with offset={next}]"
            ),
            Some(t) if first == 1 && last == t => format!("{body}\n[{t} lines total]"),
            Some(t) => format!("{body}\n[Lines {first}-{last} of {t}]"),
            None if budget_hit => format!(
                "{body}\n[Lines {first}-{last}; output limit reached, continue with offset={next}]"
            ),
            None => format!("{body}\n[Lines {first}-{last}; more lines follow]"),
        }
    }
}

fn detect_image_format(bytes: &[u8], file_len: u64) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if is_bmp_header(bytes, file_len) {
        Some("bmp")
    } else {
        None
    }
}

fn is_bmp_header(bytes: &[u8], file_len: u64) -> bool {
    if bytes.len() < 14 || !bytes.starts_with(b"BM") {
        return false;
    }
    if bytes[6..10] != [0, 0, 0, 0] {
        return false;
    }
    let off_bits = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]);
    (14..=file_len).contains(&u64::from(off_bits))
}

fn looks_binary(bytes: &[u8]) -> bool {
    // Sample a prefix so very large files stay cheap.
    let sample = &bytes[..bytes.len().min(SNIFF_BYTES)];
    if sample.is_empty() {
        return false;
    }
    if sample.contains(&0) {
        return true;
    }
    let is_control = |b: u8| b < 0x20 && b != b'\t' && b != b'\n' && b != b'\r';
    let controls = sample.iter().filter(|&&b| is_control(b)).count();
    controls * 100 / sample.len() > 30
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use zeroclaw_tools::wrappers::{PathGuardedTool, RateLimitedTool};

    fn test_tool(workspace: std::path::PathBuf) -> FileReadTool {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        });
        FileReadTool::new(security)
    }

    fn test_tool_with(
        workspace: std::path::PathBuf,
        autonomy: AutonomyLevel,
        max_actions_per_hour: u32,
    ) -> FileReadTool {
        let security = Arc::new(SecurityPolicy {
            autonomy,
            workspace_dir: workspace,
            max_actions_per_hour,
            ..SecurityPolicy::default()
        });
        FileReadTool::new(security)
    }

    fn ephemeral_tool(workspace: std::path::PathBuf) -> FileReadTool {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        });
        FileReadTool::new_with_persistence(security, false)
    }

    fn workspace_prefixed_relative_path_for_test(
        workspace: &std::path::Path,
    ) -> std::path::PathBuf {
        let mut relative = std::path::PathBuf::new();
        for component in workspace.components() {
            match component {
                std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    panic!("test workspace path must not contain parent components")
                }
                std::path::Component::Normal(part) => relative.push(part),
            }
        }
        relative
    }

    #[test]
    fn file_read_name() {
        let tool = test_tool(std::env::temp_dir());
        assert_eq!(tool.name(), "file_read");
    }

    #[test]
    fn file_read_schema_has_path() {
        let tool = test_tool(std::env::temp_dir());
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["offset"].is_object());
        assert!(schema["properties"]["limit"].is_object());
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("path"))
        );
        // offset and limit are optional
        assert!(
            !schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("offset"))
        );
    }

    #[tokio::test]
    async fn file_read_existing_file() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("test.txt"), "hello world")
            .await
            .unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "test.txt"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("1: hello world"));
        assert!(result.output.contains("[1 lines total]"));
        assert!(result.error.is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_nonexistent_file() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_missing");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "nope.txt"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("Failed to resolve"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_blocks_path_traversal() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_traversal");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({"path": "../../../etc/passwd"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("not allowed"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_blocks_absolute_path() {
        let tool = test_tool(std::env::temp_dir());

        #[cfg(unix)]
        let target = "/etc/passwd";
        #[cfg(windows)]
        let target = {
            let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
            std::path::PathBuf::from(sysroot).join(r"System32\drivers\etc\hosts")
        };

        let result = tool.execute(json!({"path": target})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("escapes workspace"));
    }

    #[tokio::test]
    async fn file_read_allows_readonly_mode() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_readonly");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("test.txt"), "readonly ok")
            .await
            .unwrap();

        let tool = test_tool_with(dir.clone(), AutonomyLevel::ReadOnly, 20);
        let result = tool.execute(json!({"path": "test.txt"})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("1: readonly ok"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_missing_path_param() {
        let tool = test_tool(std::env::temp_dir());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn file_read_schema_has_encoding() {
        let tool = test_tool(std::env::temp_dir());
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["encoding"].is_object());
    }

    #[tokio::test]
    async fn file_read_base64_returns_encoded_bytes() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_base64");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // Non-UTF-8 bytes — proves we return raw bytes, not lossy text.
        let raw: Vec<u8> = vec![0x00, 0x80, 0xFF, 0xFE, b'P', b'K', 0x03, 0x04];
        tokio::fs::write(dir.join("data.bin"), &raw).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({"path": "data.bin", "encoding": "base64"}))
            .await
            .unwrap();
        assert!(result.success, "error: {:?}", result.error);

        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(result.output.trim())
            .expect("output must be valid base64");
        assert_eq!(decoded, raw, "base64 read must round-trip exact bytes");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── Ephemeral-workspace warning────────────────

    #[tokio::test]
    async fn file_read_warns_on_ephemeral_workspace() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_ephemeral");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("notes.txt"), "host content?")
            .await
            .unwrap();

        let tool = ephemeral_tool(dir.clone());
        let result = tool.execute(json!({"path": "notes.txt"})).await.unwrap();
        assert!(result.success);
        assert!(
            result.output.contains("EPHEMERAL WORKSPACE"),
            "ephemeral warning must be present, got: {}",
            result.output
        );
        assert!(result.output.contains("mount_workspace"));
        assert!(
            result.output.contains("host content?"),
            "original read content must be preserved, got: {}",
            result.output
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_base64_not_warned_on_ephemeral_workspace() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_ephemeral_b64");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let raw: Vec<u8> = vec![0x00, 0x80, 0xFF, 0xFE, b'P', b'K'];
        tokio::fs::write(dir.join("data.bin"), &raw).await.unwrap();

        let tool = ephemeral_tool(dir.clone());
        let result = tool
            .execute(json!({"path": "data.bin", "encoding": "base64"}))
            .await
            .unwrap();
        assert!(result.success, "error: {:?}", result.error);
        assert!(
            !result.output.contains("EPHEMERAL WORKSPACE"),
            "base64 payload must not be annotated, got: {}",
            result.output
        );
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(result.output.trim())
            .expect("base64 output must still decode");
        assert_eq!(decoded, raw, "base64 read must round-trip exact bytes");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_failure_not_warned_on_ephemeral_workspace() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_ephemeral_fail");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = ephemeral_tool(dir.clone());
        let result = tool.execute(json!({"path": "missing.txt"})).await.unwrap();
        assert!(!result.success);
        assert!(!result.output.contains("EPHEMERAL WORKSPACE"));
        assert!(
            !result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("EPHEMERAL WORKSPACE")
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_no_warning_when_persistent() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_persistent");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("notes.txt"), "ok").await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "notes.txt"})).await.unwrap();
        assert!(result.success);
        assert!(
            !result.output.contains("EPHEMERAL WORKSPACE"),
            "no ephemeral warning expected on a persistent runtime, got: {}",
            result.output
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_unsupported_encoding_errors() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_bad_encoding");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("f.txt"), "hi").await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({"path": "f.txt", "encoding": "hex"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Unsupported encoding")
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_empty_file() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_empty");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("empty.txt"), "").await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "empty.txt"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_nested_path() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_nested");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(dir.join("sub/dir"))
            .await
            .unwrap();
        tokio::fs::write(dir.join("sub/dir/deep.txt"), "deep content")
            .await
            .unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({"path": "sub/dir/deep.txt"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("1: deep content"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_normalizes_workspace_prefixed_relative_path() {
        let root = std::env::temp_dir().join("zeroclaw_test_file_read_workspace_prefixed");
        let workspace = root.join("workspace");
        let nested = workspace.join("nested");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&nested).await.unwrap();
        tokio::fs::write(nested.join("notes.txt"), "prefixed content")
            .await
            .unwrap();

        let tool = test_tool(workspace.clone());
        let workspace_prefixed =
            workspace_prefixed_relative_path_for_test(&workspace).join("nested/notes.txt");
        let result = tool
            .execute(json!({"path": workspace_prefixed.to_string_lossy()}))
            .await
            .unwrap();

        assert!(
            result.success,
            "workspace-prefixed file_read path should resolve, error: {:?}",
            result.error
        );
        assert!(result.output.contains("1: prefixed content"));

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_read_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join("zeroclaw_test_file_read_symlink_escape");
        let workspace = root.join("workspace");
        let outside = root.join("outside");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();

        tokio::fs::write(outside.join("secret.txt"), "outside workspace")
            .await
            .unwrap();

        symlink(outside.join("secret.txt"), workspace.join("escape.txt")).unwrap();

        let tool = test_tool(workspace.clone());
        let result = tool.execute(json!({"path": "escape.txt"})).await.unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("escapes workspace")
        );

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn file_read_blocks_outside_workspace_regardless_of_policy() {
        let root = std::env::temp_dir().join("zeroclaw_test_file_read_blocks_outside");
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        let outside_file = outside.join("notes.txt");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();
        tokio::fs::write(&outside_file, "outside").await.unwrap();

        let tool = test_tool(workspace.clone());

        let result = tool
            .execute(json!({"path": outside_file.to_string_lossy().to_string()}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("escapes workspace"));

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn file_read_admits_absolute_path_under_read_only_root() {
        let root =
            std::env::temp_dir().join("zeroclaw_test_file_read_admits_absolute_path_under_ro_root");
        let workspace = root.join("workspace");
        let ro_root = root.join("shared");
        let ro_file = ro_root.join("notes.txt");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&ro_root).await.unwrap();
        tokio::fs::write(&ro_file, "cross-agent read")
            .await
            .unwrap();

        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace,
            allowed_roots_read_only: vec![ro_root.clone()],
            ..SecurityPolicy::default()
        });
        let tool = FileReadTool::new(security);

        let result = tool
            .execute(json!({"path": ro_file.to_string_lossy().to_string()}))
            .await
            .unwrap();

        assert!(
            result.success,
            "absolute path under read-only root must read: {result:?}"
        );
        assert!(result.output.contains("cross-agent read"));

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn file_read_with_offset_and_limit() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_offset");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("lines.txt"), "aaa\nbbb\nccc\nddd\neee")
            .await
            .unwrap();

        let tool = test_tool(dir.clone());

        // Read lines 2-3
        let result = tool
            .execute(json!({"path": "lines.txt", "offset": 2, "limit": 2}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("2: bbb"));
        assert!(result.output.contains("3: ccc"));
        assert!(!result.output.contains("1: aaa"));
        assert!(!result.output.contains("4: ddd"));
        assert!(result.output.contains("[Lines 2-3 of 5]"));

        // Read from offset 4 to end
        let result = tool
            .execute(json!({"path": "lines.txt", "offset": 4}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("4: ddd"));
        assert!(result.output.contains("5: eee"));
        assert!(result.output.contains("[Lines 4-5 of 5]"));

        // Limit only (first 2 lines)
        let result = tool
            .execute(json!({"path": "lines.txt", "limit": 2}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("1: aaa"));
        assert!(result.output.contains("2: bbb"));
        assert!(!result.output.contains("3: ccc"));
        assert!(result.output.contains("[Lines 1-2 of 5]"));

        // Full read (no offset/limit) shows all lines
        let result = tool.execute(json!({"path": "lines.txt"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("1: aaa"));
        assert!(result.output.contains("5: eee"));
        assert!(result.output.contains("[5 lines total]"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_offset_beyond_end() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_offset_end");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("short.txt"), "one\ntwo")
            .await
            .unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({"path": "short.txt", "offset": 100}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(
            result
                .output
                .contains("[No lines in range, file has 2 lines]")
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_base64_rejects_oversized_file() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_large");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // Create a file just over 10 MB
        let big = vec![b'x'; 10 * 1024 * 1024 + 1];
        tokio::fs::write(dir.join("huge.bin"), &big).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({"path": "huge.bin", "encoding": "base64"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("File too large"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_rejects_binary_file() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_reject_binary");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // Non-UTF-8 bytes containing a NUL — the classic binary signal.
        let binary_data: Vec<u8> = vec![0x00, 0x80, 0xFF, 0xFE, b'h', b'i', 0x80];
        tokio::fs::write(dir.join("data.bin"), &binary_data)
            .await
            .unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "data.bin"})).await.unwrap();

        assert!(
            !result.success,
            "binary read must fail, got output: {:?}",
            result.output
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Binary file detected"),
            "error must indicate binary rejection, got: {:?}",
            result.error
        );
        assert!(
            !result.output.contains('\u{FFFD}'),
            "must not return lossy replacement output, got: {:?}",
            result.output
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_rejects_pdf_without_rag_pdf() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_reject_pdf");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // Minimal PDF-looking bytes: valid header followed by invalid UTF-8 and
        // a NUL byte so the non-UTF-8 path triggers the binary heuristic now
        // that pdf-extract is gone.
        let pdf: Vec<u8> =
            b"%PDF-1.4\n\x80\x00\xFF\xFE\n1 0 obj\n<< /Type /Catalog >>\nendobj\n".to_vec();
        tokio::fs::write(dir.join("doc.pdf"), &pdf).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "doc.pdf"})).await.unwrap();

        assert!(
            !result.success,
            "pdf read must fail without rag-pdf, got output: {:?}",
            result.output
        );
        let err = result.error.as_deref().unwrap_or_default();
        assert!(
            err.contains("Binary file detected"),
            "error must indicate binary rejection, got: {:?}",
            result.error
        );

        // Base64 path still works.
        let result = tool
            .execute(json!({"path": "doc.pdf", "encoding": "base64"}))
            .await
            .unwrap();
        assert!(
            result.success,
            "base64 pdf read must succeed: {:?}",
            result.error
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_rejects_png_image() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_reject_png");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // PNG magic (0x89 makes it invalid UTF-8) + a few header bytes.
        let png: Vec<u8> = vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D,
        ];
        tokio::fs::write(dir.join("pic.png"), &png).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "pic.png"})).await.unwrap();

        assert!(
            !result.success,
            "image read must fail, got output: {:?}",
            result.output
        );
        let err = result.error.as_deref().unwrap_or_default();
        assert!(
            err.contains("image") && err.contains("image_info"),
            "error must point at image_info, got: {:?}",
            result.error
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_rejects_jpeg_image() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_reject_jpeg");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let jpeg: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
        tokio::fs::write(dir.join("pic.jpg"), &jpeg).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "pic.jpg"})).await.unwrap();

        assert!(!result.success, "jpeg read must fail");
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("image"),
            "error must indicate image rejection, got: {:?}",
            result.error
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_rejects_bmp_image() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_reject_bmp");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // BMP: "BM", bfSize, reserved=0, bfOffBits=54, a 40-byte DIB header,
        // then pixel bytes. Image detection runs only when the sniffed prefix is
        // invalid UTF-8 or looks binary; the NUL bytes of the header make it
        // look binary, so detect_image_format runs and names the image.
        let mut bmp: Vec<u8> = vec![
            b'B', b'M', // magic
            0x3A, 0x00, 0x00, 0x00, // bfSize = 58
            0x00, 0x00, 0x00, 0x00, // reserved
            0x36, 0x00, 0x00, 0x00, // bfOffBits = 54
        ];
        bmp.resize(54, 0); // DIB header
        bmp.extend_from_slice(&[0xFF, 0x00, 0xFF, 0x00]); // pixel array
        tokio::fs::write(dir.join("pic.bmp"), &bmp).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "pic.bmp"})).await.unwrap();

        assert!(!result.success, "bmp read must fail");
        let err = result.error.as_deref().unwrap_or_default();
        assert!(
            err.contains("image") && err.contains("image_info"),
            "error must point at image_info, got: {:?}",
            result.error
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_reads_non_utf8_bm_text_lossy() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_bm_text");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // "BM" followed by cp1251 high bytes (a Cyrillic phrase): >14 bytes, the
        // reserved field (offset 6..10) holds printable text, no NUL / control.
        let mut data: Vec<u8> = vec![b'B', b'M'];
        data.extend_from_slice(&[
            0xCF, 0xF0, 0xE0, 0xE9, 0xF1, 0x20, 0xEA, 0xEE, 0xEC, 0xEF, 0xE0, 0xED, 0xE8, 0xE8,
        ]);
        tokio::fs::write(dir.join("note.txt"), &data).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "note.txt"})).await.unwrap();

        assert!(
            result.success,
            "non-UTF-8 text starting with BM must not be rejected as an image, error: {:?}",
            result.error
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_reads_non_utf8_text_lossy() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_legacy_text");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // "Privet" (Cyrillic) in Windows-1251: all high bytes, but no NUL / control / magic.
        let cp1251: Vec<u8> = vec![0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
        tokio::fs::write(dir.join("note.txt"), &cp1251)
            .await
            .unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "note.txt"})).await.unwrap();

        assert!(
            result.success,
            "non-UTF-8 text must not be rejected as binary, error: {:?}",
            result.error
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    fn numbered_payload_lines(count: usize) -> String {
        (1..=count).map(|i| format!("line {i} payload\n")).collect()
    }

    #[tokio::test]
    async fn file_read_windows_file_larger_than_exact_total_limit() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_windowed_large");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let contents = numbered_payload_lines(1_000_000);
        assert!(contents.len() as u64 > EXACT_TOTAL_MAX_FILE_BYTES);
        tokio::fs::write(dir.join("big.txt"), &contents)
            .await
            .unwrap();
        let tool = test_tool(dir.clone());

        let result = tool
            .execute(json!({"path": "big.txt", "offset": 1, "limit": 5}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            result.output,
            "1: line 1 payload\n2: line 2 payload\n3: line 3 payload\n4: line 4 payload\n\
             5: line 5 payload\n[Lines 1-5; more lines follow]"
        );

        let result = tool
            .execute(json!({"path": "big.txt", "offset": 500_000, "limit": 3}))
            .await
            .unwrap();
        assert_eq!(
            result.output,
            "500000: line 500000 payload\n500001: line 500001 payload\n\
             500002: line 500002 payload\n[Lines 500000-500002; more lines follow]"
        );

        let result = tool
            .execute(json!({"path": "big.txt", "offset": 999_999}))
            .await
            .unwrap();
        assert_eq!(
            result.output,
            "999999: line 999999 payload\n1000000: line 1000000 payload\n\
             [Lines 999999-1000000 of 1000000]"
        );

        let result = tool
            .execute(json!({"path": "big.txt", "offset": 99_999_999}))
            .await
            .unwrap();
        assert_eq!(result.output, "[No lines in range, file has 1000000 lines]");

        let result = tool.execute(json!({"path": "big.txt"})).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        let output = result.output.as_str();
        assert!(output.starts_with("1: line 1 payload\n2: "));
        assert!(output.len() <= MAX_OUTPUT_BYTES + 140);
        let (body, footer) = output.rsplit_once('\n').unwrap();
        let next: usize = footer
            .strip_suffix(']')
            .and_then(|f| f.split_once("; output limit reached, continue with offset="))
            .map(|(_, n)| n.parse().unwrap())
            .unwrap_or_else(|| panic!("unexpected footer: {footer}"));
        let last_line = body.rsplit_once('\n').unwrap().1;
        assert_eq!(
            last_line,
            format!("{}: line {} payload", next - 1, next - 1)
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_pages_contiguously_under_output_budget() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_paging");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("lines.txt"), numbered_payload_lines(5000))
            .await
            .unwrap();
        let tool = test_tool(dir.clone());

        let mut seen = Vec::new();
        let mut offset = 1;
        loop {
            let result = tool
                .execute(json!({"path": "lines.txt", "offset": offset}))
                .await
                .unwrap();
            assert!(result.success, "{:?}", result.error);
            let (body, footer) = result.output.rsplit_once('\n').unwrap();
            assert!(body.len() <= MAX_OUTPUT_BYTES);
            for line in body.lines() {
                let (number, text) = line.split_once(": ").unwrap();
                let number: usize = number.parse().unwrap();
                assert_eq!(text, format!("line {number} payload"));
                seen.push(number);
            }
            match footer.split_once(" of 5000; output limit reached, continue with offset=") {
                Some((_, next)) => {
                    offset = next.strip_suffix(']').unwrap().parse().unwrap();
                }
                None => {
                    assert_eq!(footer, format!("[Lines {offset}-5000 of 5000]"));
                    break;
                }
            }
        }
        assert_eq!(seen, (1..=5000).collect::<Vec<_>>());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_truncates_single_overlong_line() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_overlong_line");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let mut contents = vec![b'a'; 5 * 1024 * 1024];
        contents.extend_from_slice(b"\nsecond\n");
        tokio::fs::write(dir.join("long.txt"), &contents)
            .await
            .unwrap();
        let tool = test_tool(dir.clone());

        let result = tool.execute(json!({"path": "long.txt"})).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.starts_with("1: aaaa"));
        assert!(result.output.contains(" …[truncated]"));
        assert!(
            result
                .output
                .ends_with("[Lines 1-1 of 2; output limit reached, continue with offset=2]")
        );
        assert!(result.output.len() <= MAX_OUTPUT_BYTES + 140);

        let result = tool
            .execute(json!({"path": "long.txt", "offset": 2}))
            .await
            .unwrap();
        assert_eq!(result.output, "2: second\n[Lines 2-2 of 2]");

        tokio::fs::write(dir.join("wide.txt"), "é".repeat(100_000))
            .await
            .unwrap();
        let result = tool.execute(json!({"path": "wide.txt"})).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains(" …[truncated]"));
        assert!(!result.output.contains('\u{FFFD}'));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_line_endings_match_str_lines() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_line_endings");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("crlf.txt"), "a\r\nb\r\n\r\nc")
            .await
            .unwrap();
        tokio::fs::write(dir.join("lf.txt"), "x\ny\n")
            .await
            .unwrap();
        let tool = test_tool(dir.clone());

        let result = tool.execute(json!({"path": "crlf.txt"})).await.unwrap();
        assert_eq!(result.output, "1: a\n2: b\n3: \n4: c\n[4 lines total]");
        let result = tool.execute(json!({"path": "lf.txt"})).await.unwrap();
        assert_eq!(result.output, "1: x\n2: y\n[2 lines total]");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn line_window_strips_cr_split_across_chunks() {
        let mut window = LineWindow::new(1, None, 1024, true);
        assert!(!window.feed(b"a\r"));
        assert!(!window.feed(b"\nb"));
        assert_eq!(window.finish(true).render(), "1: a\n2: b\n[2 lines total]");
    }

    #[tokio::test]
    async fn file_read_numbers_non_utf8_text() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_numbered_latin1");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("latin1.txt"), b"caf\xE9\nna\xEFve\n")
            .await
            .unwrap();
        let tool = test_tool(dir.clone());

        let result = tool.execute(json!({"path": "latin1.txt"})).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            result.output,
            "1: caf\u{FFFD}\n2: na\u{FFFD}ve\n[2 lines total]"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_read_bounds_sizeless_device_reads() {
        let tool = test_tool(std::env::temp_dir());

        let result = tool.execute(json!({"path": "/dev/zero"})).await.unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Binary file detected"),
            "{:?}",
            result.error
        );

        let result = tool
            .execute(json!({"path": "/dev/zero", "encoding": "base64"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("File too large"),
            "{:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn file_read_fits_output_under_runtime_result_budget() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_runtime_budget");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("lines.txt"), numbered_payload_lines(5000))
            .await
            .unwrap();

        let tool = test_tool(dir.clone()).with_runtime_result_budget(4000);
        let result = tool.execute(json!({"path": "lines.txt"})).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.len() <= 4000, "{}", result.output.len());
        assert_eq!(
            crate::agent::history::truncate_tool_result(&result.output, 4000),
            result.output.as_str()
        );

        let tool = ephemeral_tool(dir.clone()).with_runtime_result_budget(4000);
        let result = tool.execute(json!({"path": "lines.txt"})).await.unwrap();
        assert!(result.output.len() <= 4000, "{}", result.output.len());
        assert!(result.output.contains("EPHEMERAL WORKSPACE"));
        assert!(result.output.contains("continue with offset="));

        let tool = test_tool(dir.clone()).with_runtime_result_budget(0);
        let result = tool.execute(json!({"path": "lines.txt"})).await.unwrap();
        assert!(result.output.len() > 40_000, "{}", result.output.len());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── E2E: full agent pipeline with real FileReadTool + PDF extraction ──

    mod e2e_helpers {
        use crate::observability::{NoopObserver, Observer};
        use std::sync::{Arc, Mutex};
        use zeroclaw_config::schema::MemoryConfig;
        use zeroclaw_memory::{self, Memory};
        use zeroclaw_providers::{ChatMessage, ChatRequest, ChatResponse, ModelProvider};

        pub type SharedRequests = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

        pub struct RecordingModelProvider {
            responses: Mutex<Vec<ChatResponse>>,
            pub requests: SharedRequests,
        }

        impl RecordingModelProvider {
            pub fn new(responses: Vec<ChatResponse>) -> (Self, SharedRequests) {
                let requests: SharedRequests = Arc::new(Mutex::new(Vec::new()));
                let model_provider = Self {
                    responses: Mutex::new(responses),
                    requests: requests.clone(),
                };
                (model_provider, requests)
            }
        }

        #[async_trait::async_trait]
        impl ModelProvider for RecordingModelProvider {
            async fn chat_with_system(
                &self,
                _system_prompt: Option<&str>,
                _message: &str,
                _model: &str,
                _temperature: Option<f64>,
            ) -> anyhow::Result<String> {
                Ok("fallback".into())
            }

            async fn chat(
                &self,
                request: ChatRequest<'_>,
                _model: &str,
                _temperature: Option<f64>,
            ) -> anyhow::Result<ChatResponse> {
                self.requests
                    .lock()
                    .unwrap()
                    .push(request.messages.to_vec());

                let mut guard = self.responses.lock().unwrap();
                if guard.is_empty() {
                    return Ok(ChatResponse {
                        text: Some("done".into()),
                        tool_calls: vec![],
                        usage: None,
                        reasoning_content: None,
                    });
                }
                Ok(guard.remove(0))
            }
        }
        impl ::zeroclaw_api::attribution::Attributable for RecordingModelProvider {
            fn role(&self) -> ::zeroclaw_api::attribution::Role {
                ::zeroclaw_api::attribution::Role::Provider(
                    ::zeroclaw_api::attribution::ProviderKind::Model(
                        ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                    ),
                )
            }
            fn alias(&self) -> &str {
                "RecordingModelProvider"
            }
        }

        pub fn make_memory() -> Arc<dyn Memory> {
            let cfg = MemoryConfig {
                backend: "none".into(),
                ..MemoryConfig::default()
            };
            Arc::from(zeroclaw_memory::create_memory(&cfg, &std::env::temp_dir(), None).unwrap())
        }

        pub fn make_observer() -> Arc<dyn Observer> {
            Arc::from(NoopObserver {})
        }
    }

    #[tokio::test]
    async fn e2e_agent_file_read_rejects_binary() {
        use crate::agent::agent::Agent;
        use crate::agent::dispatcher::NativeToolDispatcher;
        use e2e_helpers::*;
        use zeroclaw_providers::{ChatResponse, ModelProvider, ToolCall};

        // ── Set up workspace with binary file ──
        let workspace = std::env::temp_dir().join("zeroclaw_test_e2e_file_read_lossy");
        let _ = tokio::fs::remove_dir_all(&workspace).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        let binary_data: Vec<u8> = vec![0x00, 0x80, 0xFF, 0xFE, b'v', b'a', b'l', b'i', b'd', 0x80];
        tokio::fs::write(workspace.join("data.bin"), &binary_data)
            .await
            .unwrap();

        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace.clone(),
            ..SecurityPolicy::default()
        });
        let file_read_tool: Box<dyn Tool> = Box::new(FileReadTool::new(security));

        let (model_provider, recorded) = RecordingModelProvider::new(vec![
            ChatResponse {
                text: Some(String::new()),
                tool_calls: vec![ToolCall {
                    id: "tc1".into(),
                    name: "file_read".into(),
                    arguments: r#"{"path": "data.bin"}"#.into(),
                    extra_content: None,
                }],
                usage: None,
                reasoning_content: None,
            },
            ChatResponse {
                text: Some("The file appears to be binary data.".into()),
                tool_calls: vec![],
                usage: None,
                reasoning_content: None,
            },
        ]);

        let mut agent = Agent::builder()
            .model_provider(Box::new(model_provider) as Box<dyn ModelProvider>)
            .tools(crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(
                vec![file_read_tool],
            ))
            .memory(make_memory())
            .observer(make_observer())
            .tool_dispatcher(Box::new(NativeToolDispatcher))
            .workspace_dir(workspace.clone())
            .build()
            .unwrap();

        let response = agent.turn("Read data.bin").await.unwrap();

        assert!(
            response.contains("binary"),
            "agent response must mention binary, got: {response}",
        );

        // Verify the tool result carries the binary-rejection error, not lossy output
        {
            let all_requests = recorded.lock().unwrap();
            assert!(
                all_requests.len() >= 2,
                "expected at least 2 model_provider requests, got {}",
                all_requests.len(),
            );

            let tool_result_msg = all_requests[1]
                .iter()
                .find(|m| m.role == "tool")
                .expect("second request must contain a tool result message");

            assert!(
                tool_result_msg.content.contains("Binary file detected"),
                "tool result must contain the binary-rejection error, got: {}",
                tool_result_msg.content,
            );
            assert!(
                !tool_result_msg.content.contains('\u{FFFD}'),
                "tool result must NOT contain lossy replacement characters, got: {}",
                tool_result_msg.content,
            );
        }

        let _ = tokio::fs::remove_dir_all(&workspace).await;
    }

    #[tokio::test]
    async fn file_read_blocks_null_byte_in_path() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_null_byte");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({"path": "test\0evil.txt"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("not allowed"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_read_allows_dev_null() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_dev_null");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool.execute(json!({"path": "/dev/null"})).await.unwrap();

        assert!(
            result.success,
            "file_read of /dev/null must succeed, error: {:?}",
            result.error
        );
        assert_eq!(result.output, "", "/dev/null must read as empty");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_read_allowed_root_with_workspace_only() {
        let root = std::env::temp_dir().join("zeroclaw_test_file_read_allowed_root");
        let workspace = root.join("workspace");
        let allowed = root.join("allowed_dir");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&allowed).await.unwrap();
        tokio::fs::write(allowed.join("data.txt"), "allowed content")
            .await
            .unwrap();

        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace.clone(),
            workspace_only: true,
            allowed_roots: vec![allowed.clone()],
            ..SecurityPolicy::default()
        });
        let tool = FileReadTool::new(security);

        // Absolute path under allowed_root should succeed
        let abs_path = allowed.join("data.txt").to_string_lossy().to_string();
        let result = tool.execute(json!({"path": &abs_path})).await.unwrap();

        assert!(
            result.success,
            "file_read with allowed_root path should succeed, error: {:?}",
            result.error
        );
        assert!(result.output.contains("allowed content"));

        // Path outside both workspace and allowed_roots should still fail
        let outside = root.join("outside");
        tokio::fs::create_dir_all(&outside).await.unwrap();
        tokio::fs::write(outside.join("secret.txt"), "secret")
            .await
            .unwrap();
        let outside_path = outside.join("secret.txt").to_string_lossy().to_string();
        let result = tool.execute(json!({"path": &outside_path})).await.unwrap();
        assert!(!result.success);

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn wrapped_file_read_failure_preserves_rate_limit_budget() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_read_probe");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let inner = test_tool_with(dir.clone(), AutonomyLevel::Supervised, 2);
        let security = inner.security.clone();
        let tool = RateLimitedTool::new(
            PathGuardedTool::new(inner, security.clone()),
            security.clone(),
        );

        // Failed production-wrapped reads release their reservations.
        let r1 = tool.execute(json!({"path": "nope1.txt"})).await.unwrap();
        assert!(!r1.success);
        assert!(
            r1.error
                .as_deref()
                .unwrap_or("")
                .contains("Failed to resolve")
        );

        let r2 = tool.execute(json!({"path": "nope2.txt"})).await.unwrap();
        assert!(!r2.success);
        assert!(
            r2.error
                .as_deref()
                .unwrap_or("")
                .contains("Failed to resolve")
        );

        let r3 = tool.execute(json!({"path": "nope3.txt"})).await.unwrap();
        assert!(!r3.success);

        assert!(security.record_action(), "first slot remains available");
        assert!(security.record_action(), "second slot remains available");
        assert!(
            !security.record_action(),
            "successful debits exhaust the budget"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
