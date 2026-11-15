# Bounded tool file I/O

> **Status: specification.** This page defines how ZeroLite's file tools bound
> what they read and return: `file_read` reads any text file by line window
> under the runtime's tool-result budget, `file_edit` refuses oversized files
> and replaces atomically, and delegate result artifacts are read under a cap.
> For the merge rule, see `docs/merge_update_rules.md`. Decisions here are
> precedent: cite them as `bounded-file-io #N` in merge reports.

## Decision record: why this shape

- **R1 — Bound what is returned or needed, not what `stat` reports.** Text
  `file_read` streams and keeps only the requested lines; file size never
  rejects a text read. Whole-file caps remain only where every byte is needed
  (`encoding="base64"`, `file_edit`, delegate result artifacts) and are
  enforced on bytes actually read (`take(cap + 1)`), because `stat` is not
  authoritative for devices or files still growing.
- **R2 — The tool truncates before the runtime does.** `file_read` cuts on a
  line boundary under `min(48 KiB, max_tool_result_chars − 1 KiB)` (floor
  1 KiB; `0` keeps the 48 KiB ceiling) and ends with
  `continue with offset=N`, so the runtime's head/tail truncation never drops
  the middle of a window. A single line larger than the whole budget is cut
  and marked `…[truncated]`. The budget is fixed at registration
  (`with_runtime_result_budget`); the tool description stays static so the
  prompt-cache prefix does not depend on it.
- **R3 — Exact line totals only when cheap.** Files ≤ 10 MiB are counted to
  EOF and keep the upstream footers (`[N lines total]`, `[Lines a-b of N]`,
  `[No lines in range, file has N lines]`); larger files stop at the window
  (`[Lines a-b; more lines follow]`).
- **R4 — Binary is decided on the first 8 KiB** (NUL or > 30 % control
  bytes), independent of UTF-8 validity; image magic is checked when that
  prefix is binary or invalid UTF-8. Non-UTF-8 text is decoded lossily per
  line and numbered, with `offset`/`limit` support.
- **R5 — `file_edit` reads and replaces through the parent directory
  handle**: a no-follow, size-bounded read (10 MiB) and `write_file_atomic`,
  the same boundary helpers as `file_write`. The replacement keeps the
  original permission bits; hard links no longer share the new content, and
  the parent directory must be writable.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | `file_read` rejects any file whose `stat` size exceeds 10 MiB, then reads the whole file | Text reads stream any size; only base64 keeps the 10 MiB cap, enforced on bytes read | R1 |
| 2 | No output budget: an unlimited read is cut by the runtime's head/tail truncation | Line-boundary cut under the runtime budget with a `continue with offset=N` footer | R2 |
| 3 | Lines always counted over the whole file | Exact totals up to 10 MiB; larger files stop at the window | R3 |
| 4 | Binary detected only when the whole file is invalid UTF-8; such text returned unnumbered | 8 KiB sniff decides binary; non-UTF-8 text numbered per line | R4 |
| 5 | `file_edit` reads with `read_to_string` and writes in place with `tokio::fs::write`, no size cap | Bounded no-follow read and atomic replace through the parent handle | R5 |
| 6 | Delegate result artifacts read with unbounded `read_to_string` | `DelegateTool::read_result_artifact`, capped at 10 MiB | R1 |

## Merge notes

- **Upstream #11592 lands in the same function.** Its
  `file_read_allowed_patterns` check goes after `is_resolved_path_readable`
  and before the `metadata` call in `FileReadTool::read_path`; keep this
  page's read tail (`read_text_window`, `LineWindow`) after it.
- **Verify post-merge by behavior:** a windowed read of a file larger than
  10 MiB succeeds; paging along `continue with offset=` yields contiguous
  line numbers; a read under a small `max_tool_result_chars` is not changed
  by `truncate_tool_result`.
- **Code anchors:** `crates/zeroclaw-runtime/src/tools/file_read.rs`
  (`read_text_window`, `LineWindow`, `WindowRead::render`,
  `with_runtime_result_budget`) and its registration in
  `crates/zeroclaw-runtime/src/tools/mod.rs`
  (`all_tools_with_runtime_on_thread`); `crates/zeroclaw-tools/src/file_edit.rs`
  (`replace_in_file`); `DelegateTool::read_result_artifact` in
  `crates/zeroclaw-runtime/src/tools/delegate.rs`.
- **Tests that must survive:** `file_read_base64_rejects_oversized_file`,
  `file_read_windows_file_larger_than_exact_total_limit`,
  `file_read_pages_contiguously_under_output_budget`,
  `file_read_truncates_single_overlong_line`,
  `file_read_line_endings_match_str_lines`,
  `line_window_strips_cr_split_across_chunks`,
  `file_read_numbers_non_utf8_text`, `file_read_bounds_sizeless_device_reads`,
  `file_read_fits_output_under_runtime_result_budget`;
  `file_edit_rejects_file_over_size_limit`,
  `file_edit_replaces_file_atomically`; the `oversized` case in
  `background_lifecycle_reports_artifact_errors_without_changing_terminal_status`.

## Sources

- `file_read_change_request.md` (windowed reads, output budget, exact-total
  semantics, binary sniffing).
- Owner decisions for this change: budget injected at construction;
  `max_tool_result_chars = 0` keeps the tool ceiling; mixed total-line
  semantics; 8 KiB binary sniff; `file_edit` size cap with atomic replace and
  the delegate artifact read cap fixed in the same change.
