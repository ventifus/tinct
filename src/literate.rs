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
}
