//! Literate tinct: extract and evaluate tinct code blocks from Markdown files.
//!
//! # Overview
//!
//! Markdown files containing fenced code blocks tagged as `tinct` or `llt` can be
//! used as literate programs. This module provides two core primitives:
//!
//! - [`extract_code_blocks`] — scan Markdown source for ```` ```tinct ```` or ```` ```llt ````
//!   fenced code blocks and return their contents as a `Vec<String>`.
//! - [`tangle`] — join extracted blocks with `\n---\n` separators to produce a
//!   tinct source string ready for the standard parser pipeline.
//!
//! # Literate Modes
//!
//! The `tinct literate` subcommand provides three modes (see `src/main.rs`):
//!
//! - **`tangle`** — extract code blocks and print the joined tinct source.
//! - **`eval`** — extract, join, and evaluate; print the result as JSON.
//! - **`weave`** — evaluate each block in pipeline order and output the original
//!   Markdown with JSON results appended as comments after each block.
//!
//! # Pipeline Semantics
//!
//! Blocks are treated as pipeline stages, exactly like `---`-separated documents
//! in a single `.llt` file. `%` threads between blocks in document order:
//! the output of block N becomes `%` for block N+1.
//!
//! # Markdown Extraction
//!
//! The extractor uses a simple line-by-line state machine. It does not require a
//! full Markdown parser — it only looks for fenced code block boundaries:
//!
//! - Opening fence: a line whose trimmed content is `` ```tinct `` or `` ```llt ``
//!   (exactly those two language identifiers, case-sensitive, with optional trailing
//!   whitespace).
//! - Closing fence: a line whose trimmed content is `` ``` `` (three backticks,
//!   no language tag).
//!
//! Nested fences (four or more backticks) are not supported. Code blocks that are
//! never closed (by end of input) are silently discarded.

/// Extract tinct code block contents from a Markdown string.
///
/// Scans the Markdown source line-by-line for fenced code blocks tagged with
/// `` `tinct `` or `` `llt ``. Returns the interior content of each such block
/// as a separate `String` (trailing newline stripped).
///
/// # Fencing rules
///
/// - Opening fence: a line whose trimmed form is exactly `` ```tinct `` or `` ```llt ``.
/// - Closing fence: a line whose trimmed form is exactly `` ``` ``.
/// - Content lines inside the fence are included verbatim (with their original newlines).
/// - An unclosed block at end-of-input is silently discarded.
///
/// # Example
///
/// ```text
/// # My Docs
///
/// ```tinct
/// [x: 1]
/// ```
///
/// Some prose.
///
/// ```llt
/// [y: 2]
/// ```
/// ```
///
/// Returns `vec!["[x: 1]\n".to_string(), "[y: 2]\n".to_string()]` (content with
/// interior newlines, trailing newline stripped by `tangle`).
pub fn extract_code_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current_block: Option<String> = None;

    for line in markdown.lines() {
        let trimmed = line.trim();
        match current_block {
            None => {
                // Look for an opening fence.
                if trimmed == "```tinct" || trimmed == "```llt" {
                    current_block = Some(String::new());
                }
                // All other lines (prose, other code blocks) are ignored.
            }
            Some(ref mut buf) => {
                // Inside a tinct/llt block. Look for the closing fence.
                if trimmed == "```" {
                    // Closing fence found: commit the accumulated block.
                    blocks.push(current_block.take().unwrap());
                } else {
                    // Content line: append verbatim (restore the newline that
                    // `str::lines()` strips from each line).
                    buf.push_str(line);
                    buf.push('\n');
                }
            }
        }
    }
    // An unclosed block (no closing ``` found) is silently discarded.
    blocks
}

/// Join extracted code blocks into a single tinct pipeline source string.
///
/// Each block is separated by `\n---\n`, which is the tinct pipeline separator.
/// This makes the joined result equivalent to a single `.llt` file with `---`
/// boundaries — `%` threads between blocks in order.
///
/// An empty `blocks` list produces an empty string.
pub fn tangle(blocks: Vec<String>) -> String {
    blocks.join("\n---\n")
}

/// A tinct code block extracted from markdown with byte offset information.
///
/// Used by the LSP server to map positions between markdown coordinates and
/// block-local tinct source coordinates.
#[derive(Debug, Clone)]
pub struct LiterateBlock {
    /// The tinct code portion (everything before first === line).
    pub code: String,
    /// Expected output sections from === markers.
    pub expectations: BlockExpectations,
    /// Byte offset of opening ``` fence in markdown.
    pub md_start: usize,
    /// Byte offset of first code line (after ``` and language tag) in markdown.
    pub md_code_start: usize,
    /// Byte offset of closing ``` fence in markdown.
    pub md_code_end: usize,
}

/// Expected output sections from a code block.
#[derive(Debug, Clone)]
pub struct BlockExpectations {
    /// Expected standard output (from `=== out` section).
    pub out: Option<String>,
    /// Expected warnings (from `=== warn` section).
    pub warn: Option<String>,
    /// Expected error substring (from `=== error` section).
    pub error: Option<String>,
    /// Expected info/log output (from `=== info` section).
    pub info: Option<String>,
}

/// A code block with its code portion and optional expected output sections.
#[derive(Debug)]
pub struct BlockWithExpectations {
    /// The tinct code to execute (everything before the first `===` marker).
    pub code: String,
    /// Expected outputs from `=== out`, `=== warn`, `=== error`, `=== info` sections.
    pub expectations: BlockExpectations,
}

/// Split a code block into code portion and expected output sections.
///
/// The code portion is everything before the first `===` marker.
/// Expected sections are `=== out`, `=== warn`, `=== error`, `=== info`.
///
/// If no `===` markers are present, the entire block is code with no expectations.
pub fn split_block_sections(block: &str) -> BlockWithExpectations {
    // Find all section delimiters
    let mut sections = Vec::new();
    let mut search_start = 0;

    while let Some(pos) = block[search_start..].find("\n===") {
        let abs_pos = search_start + pos;
        // Check what comes after "==="
        let after_delim = &block[abs_pos + 4..]; // skip "\n==="

        // Extract the label (text between === and the next newline)
        let label_end = after_delim.find('\n').unwrap_or(after_delim.len());
        let label = after_delim[..label_end].trim();

        sections.push((abs_pos, label));
        search_start = abs_pos + 4 + label_end;
    }

    // If no sections found, the entire block is code
    if sections.is_empty() {
        return BlockWithExpectations {
            code: block.to_string(),
            expectations: BlockExpectations {
                out: None,
                warn: None,
                error: None,
                info: None,
            },
        };
    }

    // Code portion is everything before the first delimiter
    let code = &block[..sections[0].0 + 1]; // include trailing newline before ===

    // Parse sections
    let mut out = None;
    let mut warn = None;
    let mut error = None;
    let mut info = None;

    for (i, (pos, label)) in sections.iter().enumerate() {
        // Content starts after "\n=== label\n"
        let label_line_start = pos + 4; // skip "\n==="
        let label_line_end = block[label_line_start..]
            .find('\n')
            .map(|p| label_line_start + p)
            .unwrap_or(block.len());
        let content_start = if label_line_end < block.len() {
            label_line_end + 1 // skip the newline after label
        } else {
            label_line_end
        };

        // Content ends at next section or EOF
        let content_end = sections
            .get(i + 1)
            .map(|(next_pos, _)| *next_pos + 1) // include trailing newline
            .unwrap_or(block.len());

        let section_content = &block[content_start..content_end];
        let trimmed = section_content.trim();

        match *label {
            "out" => {
                out = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            "warn" => {
                warn = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            "error" => {
                error = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            "info" => {
                info = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            _ => {
                // Unknown section label — ignore (allows for future extensions)
            }
        }
    }

    BlockWithExpectations {
        code: code.to_string(),
        expectations: BlockExpectations {
            out,
            warn,
            error,
            info,
        },
    }
}

/// Extract tinct code blocks from markdown with byte offset information.
///
/// Returns a `Vec<LiterateBlock>` with byte offsets for each block's fence
/// positions in the original markdown. Used by the LSP server to map positions
/// between markdown coordinates and block-local tinct source.
///
/// Unclosed blocks (no closing fence) are silently discarded.
pub fn extract_blocks(markdown: &str) -> Vec<LiterateBlock> {
    let mut blocks = Vec::new();
    let mut current_block: Option<(String, usize, usize)> = None; // (content, fence_start, code_start)
    let mut byte_offset = 0;

    for line in markdown.lines() {
        let line_len = line.len();
        let trimmed = line.trim();

        match current_block {
            None => {
                // Look for opening fence
                if trimmed == "```tinct" || trimmed == "```llt" {
                    let fence_start = byte_offset;
                    // Code starts after this line's newline
                    let code_start = byte_offset + line_len + 1; // +1 for the newline
                    current_block = Some((String::new(), fence_start, code_start));
                }
            }
            Some((ref mut buf, fence_start, code_start)) => {
                // Inside a tinct/llt block
                if trimmed == "```" {
                    // Closing fence found
                    let code_end = byte_offset; // Closing fence starts here
                    let full_code = current_block.take().unwrap().0;

                    // Split into code and expectations
                    let BlockWithExpectations { code, expectations } =
                        split_block_sections(&full_code);

                    blocks.push(LiterateBlock {
                        code,
                        expectations,
                        md_start: fence_start,
                        md_code_start: code_start,
                        md_code_end: code_end,
                    });
                } else {
                    // Content line: append verbatim
                    buf.push_str(line);
                    buf.push('\n');
                }
            }
        }

        byte_offset += line_len + 1; // +1 for the newline stripped by lines()
    }

    // Unclosed blocks are silently discarded
    blocks
}

/// Map a markdown byte offset to (block_index, block_relative_offset).
///
/// Returns `None` if the offset is not inside any tinct block.
pub fn md_offset_to_block(blocks: &[LiterateBlock], md_offset: usize) -> Option<(usize, usize)> {
    for (idx, block) in blocks.iter().enumerate() {
        // Check if offset is within this block's code region
        if md_offset >= block.md_code_start && md_offset < block.md_code_end {
            let block_relative = md_offset - block.md_code_start;
            return Some((idx, block_relative));
        }
    }
    None
}

/// Map a tinct span (from block-local source) to markdown coordinates.
///
/// Returns a `Span` with byte offsets and line numbers adjusted to markdown coordinates.
/// `markdown` is the full markdown source (needed to compute the block's start line).
pub fn block_span_to_md(
    blocks: &[LiterateBlock],
    block_idx: usize,
    span: crate::ast::Span,
    markdown: &str,
) -> crate::ast::Span {
    use crate::ast::{Position, Span};

    if let Some(block) = blocks.get(block_idx) {
        let offset_delta = block.md_code_start;
        // Count newlines in markdown before block.md_code_start to get the block's
        // 1-indexed start line in the markdown. Block-local line numbers are 1-indexed,
        // so we add (block_start_line - 1) to get markdown-absolute line numbers.
        let block_start_line = markdown[..offset_delta]
            .chars()
            .filter(|&c| c == '\n')
            .count();
        Span::new(
            Position {
                offset: span.start.offset + offset_delta as u32,
                line: span.start.line + block_start_line as u32,
                column: span.start.column,
            },
            Position {
                offset: span.end.offset + offset_delta as u32,
                line: span.end.line + block_start_line as u32,
                column: span.end.column,
            },
            span.file.clone(),
        )
    } else {
        // Block index out of bounds — return original span unchanged
        span
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // extract_code_blocks
    // ------------------------------------------------------------------

    #[test]
    fn extract_empty_markdown() {
        assert!(extract_code_blocks("").is_empty());
    }

    #[test]
    fn extract_no_code_blocks() {
        let md = "# Title\n\nSome prose.\n\nMore prose.";
        assert!(extract_code_blocks(md).is_empty());
    }

    #[test]
    fn extract_other_language_blocks_ignored() {
        let md = "```rust\nfn main() {}\n```\n```python\nprint('hi')\n```";
        assert!(extract_code_blocks(md).is_empty());
    }

    #[test]
    fn extract_single_tinct_block() {
        let md = "```tinct\n[x: 1]\n```";
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], "[x: 1]\n");
    }

    #[test]
    fn extract_single_llt_block() {
        let md = "```llt\n[x: 1]\n```";
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], "[x: 1]\n");
    }

    #[test]
    fn extract_two_blocks() {
        let md = "```tinct\n[x: 1]\n```\n\nProse.\n\n```tinct\n[y: 2]\n```";
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], "[x: 1]\n");
        assert_eq!(blocks[1], "[y: 2]\n");
    }

    #[test]
    fn extract_mixed_language_blocks() {
        // Only tinct/llt blocks are extracted; rust block is ignored.
        let md = concat!(
            "```tinct\n[a: 1]\n```\n",
            "```rust\nfn main() {}\n```\n",
            "```llt\n[b: 2]\n```",
        );
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], "[a: 1]\n");
        assert_eq!(blocks[1], "[b: 2]\n");
    }

    #[test]
    fn extract_multiline_block() {
        let md = "```tinct\n[x: 1\n y: 2]\n```";
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], "[x: 1\n y: 2]\n");
    }

    #[test]
    fn extract_block_with_pipeline_separator() {
        // A block can contain --- internally.
        let md = "```tinct\n[x: 1]\n---\n[y: 2]\n```";
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], "[x: 1]\n---\n[y: 2]\n");
    }

    #[test]
    fn extract_unclosed_block_discarded() {
        // Block opened but never closed — discarded silently.
        let md = "```tinct\n[x: 1]\n";
        let blocks = extract_code_blocks(md);
        assert!(blocks.is_empty());
    }

    #[test]
    fn extract_opening_fence_with_trailing_whitespace() {
        // Trailing whitespace on the opening line is trimmed and still matches.
        let md = "```tinct   \n[x: 1]\n```";
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], "[x: 1]\n");
    }

    #[test]
    fn extract_prose_between_blocks_ignored() {
        let md = concat!(
            "# Header\n\n",
            "Intro prose.\n\n",
            "```tinct\n[port: 8080]\n```\n\n",
            "Some explanation of port.\n\n",
            "```tinct\n[workers: 4]\n```\n\n",
            "Trailing prose.",
        );
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], "[port: 8080]\n");
        assert_eq!(blocks[1], "[workers: 4]\n");
    }

    // ------------------------------------------------------------------
    // tangle
    // ------------------------------------------------------------------

    #[test]
    fn tangle_empty_list() {
        assert_eq!(tangle(vec![]), "");
    }

    #[test]
    fn tangle_single_block() {
        let result = tangle(vec!["[x: 1]\n".to_string()]);
        assert_eq!(result, "[x: 1]\n");
    }

    #[test]
    fn tangle_two_blocks_joined_with_separator() {
        let result = tangle(vec!["[x: 1]\n".to_string(), "[y: 2]\n".to_string()]);
        assert_eq!(result, "[x: 1]\n\n---\n[y: 2]\n");
    }

    #[test]
    fn tangle_three_blocks() {
        let result = tangle(vec![
            "[a: 1]\n".to_string(),
            "[b: 2]\n".to_string(),
            "[c: 3]\n".to_string(),
        ]);
        assert_eq!(result, "[a: 1]\n\n---\n[b: 2]\n\n---\n[c: 3]\n");
    }

    #[test]
    fn tangle_roundtrip_extract_then_join() {
        // The full tangle pipeline: extract then join.
        let md = concat!(
            "```tinct\n",
            "[\n",
            "  base-url: \"https://api.example.com\"\n",
            "  timeout: 30\n",
            "]\n",
            "```\n\n",
            "Filter active users.\n\n",
            "```tinct\n",
            "[filter [fn [u] u.active] %.users]\n",
            "```",
        );
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks.len(), 2);
        let tangled = tangle(blocks);
        assert!(tangled.contains("base-url"));
        assert!(tangled.contains("\n---\n"));
        assert!(tangled.contains("[filter"));
    }

    // ------------------------------------------------------------------
    // extract_blocks (with byte offsets)
    // ------------------------------------------------------------------

    #[test]
    fn extract_blocks_single() {
        let md = "```tinct\n[x: 1]\n```";
        let blocks = extract_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].code, "[x: 1]\n");
        assert_eq!(blocks[0].md_start, 0); // ``` starts at offset 0
        assert_eq!(blocks[0].md_code_start, 9); // after "```tinct\n"
        assert_eq!(blocks[0].md_code_end, 16); // before closing ```
    }

    #[test]
    fn extract_blocks_with_prose() {
        let md = "# Header\n\n```tinct\n[x: 1]\n```\n";
        let blocks = extract_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].code, "[x: 1]\n");
        // "# Header\n\n" = 10 bytes
        assert_eq!(blocks[0].md_start, 10);
        assert_eq!(blocks[0].md_code_start, 19); // 10 + len("```tinct\n")
    }

    #[test]
    fn extract_blocks_multiple() {
        let md = "```tinct\n[a: 1]\n```\n\n```tinct\n[b: 2]\n```";
        let blocks = extract_blocks(md);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].code, "[a: 1]\n");
        assert_eq!(blocks[1].code, "[b: 2]\n");
    }

    #[test]
    fn extract_blocks_with_expectations() {
        let md = "```tinct\n[x: 1]\n=== out\n42\n```";
        let blocks = extract_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].code, "[x: 1]\n");
        assert_eq!(blocks[0].expectations.out, Some("42".to_string()));
    }

    // ------------------------------------------------------------------
    // md_offset_to_block
    // ------------------------------------------------------------------

    #[test]
    fn md_offset_to_block_inside_first() {
        let md = "```tinct\n[x: 1]\n```";
        let blocks = extract_blocks(md);
        // Offset 10 is inside "[x: 1]\n" (code starts at 9)
        let result = md_offset_to_block(&blocks, 10);
        assert_eq!(result, Some((0, 1))); // block 0, offset 1 within block
    }

    #[test]
    fn md_offset_to_block_outside() {
        let md = "prose\n```tinct\n[x: 1]\n```\nmore";
        let blocks = extract_blocks(md);
        // Offset 0 is in prose before any block
        assert_eq!(md_offset_to_block(&blocks, 0), None);
    }

    #[test]
    fn md_offset_to_block_second_block() {
        let md = "```tinct\n[a: 1]\n```\n\n```tinct\n[b: 2]\n```";
        let blocks = extract_blocks(md);
        // Find offset of second block's code
        let second_code_start = blocks[1].md_code_start;
        let result = md_offset_to_block(&blocks, second_code_start + 1);
        assert_eq!(result, Some((1, 1))); // block 1, offset 1
    }

    // ------------------------------------------------------------------
    // block_span_to_md
    // ------------------------------------------------------------------

    #[test]
    fn block_span_to_md_simple() {
        use crate::ast::{Position, Span};

        let md = "```tinct\n[x: 1]\n```";
        let blocks = extract_blocks(md);
        // Block-local span: offset 1..2 (the "x" in "[x: 1]")
        let block_span = Span::new(
            Position {
                offset: 1,
                line: 1,
                column: 2,
            },
            Position {
                offset: 2,
                line: 1,
                column: 3,
            },
            crate::rust_span!().file,
        );
        let md_span = block_span_to_md(&blocks, 0, block_span, md);
        // Code starts at md offset 9, so offset 1 → 10
        assert_eq!(md_span.start.offset, 10);
        assert_eq!(md_span.end.offset, 11);
        // "```tinct\n" has 1 newline, so block_start_line = 1.
        // Block-local line 1 → markdown line 2 (1 + 1).
        assert_eq!(md_span.start.line, 2);
        assert_eq!(md_span.end.line, 2);
    }
}
