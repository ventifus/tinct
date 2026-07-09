//! Coordinate system conversion between LLT spans and LSP positions,
//! and between filesystem paths and LSP URIs.
//!
//! - **LLT Position**: 1-indexed line and column, byte offsets
//! - **LSP Position**: 0-indexed line and column, UTF-16 code units

use crate::ast::Span;

/// Convert an LLT span to an LSP range.
pub fn llt_span_to_lsp_range(span: &Span, source: &str) -> lsp_types::Range {
    let start = llt_position_to_lsp(span.start.line, span.start.column, source);
    let end = llt_position_to_lsp(span.end.line, span.end.column, source);
    lsp_types::Range { start, end }
}

/// Convert LLT 1-indexed line/column to LSP 0-indexed line/UTF-16 column.
fn llt_position_to_lsp(line: usize, col: usize, source: &str) -> lsp_types::Position {
    // LLT lines are 1-indexed; LSP lines are 0-indexed.
    let lsp_line = line.saturating_sub(1) as u32;

    // Find the start of the LLT line in the source text.
    let line_start_offset = match compute_line_start(source, line.saturating_sub(1)) {
        Some(offset) => offset,
        None => {
            return lsp_types::Position {
                line: lsp_line,
                character: 0,
            }
        }
    };

    // LLT column is a 1-indexed Unicode scalar count; convert to UTF-16 code units for LSP.
    // Iterate chars (not bytes) to avoid byte-boundary panics on multi-byte characters.
    let line_text = source[line_start_offset..].lines().next().unwrap_or("");
    let utf16_col = line_text
        .chars()
        .take(col.saturating_sub(1))
        .map(|c| c.len_utf16() as u32)
        .sum::<u32>();

    lsp_types::Position {
        line: lsp_line,
        character: utf16_col,
    }
}

/// Convert an LSP position to a byte offset in the source text.
///
/// Returns `None` if the position is out of bounds.
pub fn lsp_position_to_offset(pos: &lsp_types::Position, source: &str) -> Option<usize> {
    let line_idx = pos.line as usize;
    let utf16_char = pos.character as usize;

    let offset = compute_line_start(source, line_idx)?;

    let line_text = source[offset..].lines().next().unwrap_or("");
    let mut utf16_count = 0;
    for (byte_idx, _) in line_text.char_indices() {
        if utf16_count == utf16_char {
            return Some(offset + byte_idx);
        }
        utf16_count += line_text[byte_idx..].chars().next()?.len_utf16();
    }
    if utf16_count == utf16_char {
        return Some(offset + line_text.len());
    }
    None
}

/// Compute the byte offset of the start of a 0-indexed line, handling both LF and CRLF.
/// Returns `None` if the target line does not exist in the source.
fn compute_line_start(source: &str, target_line: usize) -> Option<usize> {
    if target_line == 0 {
        return Some(0);
    }
    let bytes = source.as_bytes();
    let mut line = 0;
    let mut i = 0;
    while i < bytes.len() && line < target_line {
        if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            i += 2;
            line += 1;
        } else if bytes[i] == b'\n' {
            i += 1;
            line += 1;
        } else {
            i += 1;
        }
    }
    if line == target_line {
        Some(i)
    } else {
        None
    }
}

/// Convert a filesystem path to an LSP URI.
///
/// Returns `None` if the path is not absolute (required by `url::Url::from_file_path`),
/// or if the resulting URL string cannot be parsed as an `lsp_types::Uri`.
/// URI-invalid characters in the path are automatically percent-encoded.
pub fn file_path_to_uri(path: &std::path::Path) -> Option<lsp_types::Uri> {
    let url = url::Url::from_file_path(path).ok()?;
    url.as_str().parse::<lsp_types::Uri>().ok()
}

/// Convert an LSP URI to a filesystem path.
///
/// Returns `None` if the URI is not a `file://` URL or if the path component
/// cannot be decoded into a valid `PathBuf`.
pub fn uri_to_file_path(uri: &lsp_types::Uri) -> Option<std::path::PathBuf> {
    let url = url::Url::parse(uri.as_str()).ok()?;
    url.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Position;

    fn make_span(start_line: usize, start_col: usize, end_line: usize, end_col: usize) -> Span {
        Span {
            start: Position {
                offset: 0,
                line: start_line,
                column: start_col,
            },
            end: Position {
                offset: 0,
                line: end_line,
                column: end_col,
            },
            file: crate::rust_span!().file,
        }
    }

    #[test]
    fn test_llt_to_lsp_simple() {
        let source = "[x: 1]";
        let span = make_span(1, 1, 1, 7);
        let range = llt_span_to_lsp_range(&span, source);

        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 0);
        assert_eq!(range.end.line, 0);
        assert_eq!(range.end.character, 6);
    }

    #[test]
    fn test_llt_to_lsp_multiline() {
        let source = "[x: 1\n y: 2]";
        let span = make_span(1, 1, 2, 6);
        let range = llt_span_to_lsp_range(&span, source);

        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 0);
        assert_eq!(range.end.line, 1);
        assert_eq!(range.end.character, 5);
    }

    #[test]
    fn test_llt_to_lsp_utf8_multibyte() {
        // "é" is 2 bytes in UTF-8, 1 code unit in UTF-16.
        // "[x: é]" is 6 Unicode scalars → end column 7 (1-indexed scalar count).
        let source = "[x: é]";
        let span = make_span(1, 1, 1, 7); // LLT column 7 = one past the 6-scalar string
        let range = llt_span_to_lsp_range(&span, source);

        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 0);
        assert_eq!(range.end.line, 0);
        assert_eq!(range.end.character, 6); // "[x: é]" is 6 UTF-16 code units
    }

    #[test]
    fn test_llt_to_lsp_emoji() {
        // "😀" is 4 bytes in UTF-8 but 1 Unicode scalar and 2 UTF-16 code units.
        // "[x: 😀]" — emoji is scalar 5, "]" is scalar 6.
        let source = "[x: 😀]";
        let span = make_span(1, 5, 1, 6); // scalars 5..6 covers just the emoji
        let range = llt_span_to_lsp_range(&span, source);

        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 4); // after "[x: " (4 scalars = 4 UTF-16 units)
        assert_eq!(range.end.line, 0);
        assert_eq!(range.end.character, 6); // after "😀" (2 UTF-16 code units)
    }

    #[test]
    fn test_lsp_to_offset_simple() {
        let source = "[x: 1]";
        let pos = lsp_types::Position {
            line: 0,
            character: 4,
        };
        let offset = lsp_position_to_offset(&pos, source).unwrap();
        assert_eq!(offset, 4); // Points to '1'
    }

    #[test]
    fn test_lsp_to_offset_multiline() {
        let source = "[x: 1\n y: 2]";
        let pos = lsp_types::Position {
            line: 1,
            character: 1,
        };
        let offset = lsp_position_to_offset(&pos, source).unwrap();
        assert_eq!(offset, 7); // After newline, at 'y'
    }

    #[test]
    fn test_lsp_to_offset_utf8_multibyte() {
        let source = "[x: é]";
        let pos = lsp_types::Position {
            line: 0,
            character: 4,
        };
        let offset = lsp_position_to_offset(&pos, source).unwrap();
        assert_eq!(offset, 4); // After "[x: ", at 'é' (byte offset 4)
    }

    #[test]
    fn test_lsp_to_offset_emoji() {
        let source = "[x: 😀]";
        let pos = lsp_types::Position {
            line: 0,
            character: 4,
        };
        let offset = lsp_position_to_offset(&pos, source).unwrap();
        assert_eq!(offset, 4); // After "[x: ", at '😀' start

        let pos = lsp_types::Position {
            line: 0,
            character: 6,
        };
        let offset = lsp_position_to_offset(&pos, source).unwrap();
        assert_eq!(offset, 8); // After '😀]' (emoji is 4 bytes)
    }

    #[test]
    fn test_lsp_to_offset_out_of_bounds_line() {
        let source = "[x: 1]";
        let pos = lsp_types::Position {
            line: 5,
            character: 0,
        };
        assert!(lsp_position_to_offset(&pos, source).is_none());
    }

    #[test]
    fn test_lsp_to_offset_out_of_bounds_character() {
        let source = "[x: 1]";
        let pos = lsp_types::Position {
            line: 0,
            character: 100,
        };
        assert!(lsp_position_to_offset(&pos, source).is_none());
    }

    #[test]
    fn test_lsp_to_offset_end_of_line() {
        let source = "[x: 1]\n[y: 2]";
        let pos = lsp_types::Position {
            line: 0,
            character: 6,
        };
        let offset = lsp_position_to_offset(&pos, source).unwrap();
        assert_eq!(offset, 6); // After ']' on line 1
    }

    #[test]
    fn test_round_trip_ascii() {
        let source = "[x: 42]\n[y: hello]";
        let span = make_span(2, 5, 2, 10); // "hello"
        let range = llt_span_to_lsp_range(&span, source);

        let offset_start = lsp_position_to_offset(&range.start, source).unwrap();
        let offset_end = lsp_position_to_offset(&range.end, source).unwrap();

        assert_eq!(&source[offset_start..offset_end], "hello");
    }

    #[test]
    fn test_llt_position_to_lsp_first_char() {
        let source = "abc";
        let pos = llt_position_to_lsp(1, 1, source);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 0);
    }

    #[test]
    fn test_llt_position_to_lsp_second_line() {
        let source = "abc\ndef";
        let pos = llt_position_to_lsp(2, 1, source);
        assert_eq!(pos.line, 1);
        assert_eq!(pos.character, 0);
    }

    #[test]
    fn test_llt_to_lsp_crlf() {
        let source = "abc\r\ndef";
        let pos = llt_position_to_lsp(2, 1, source);
        assert_eq!(pos.line, 1);
        assert_eq!(pos.character, 0);
    }

    #[test]
    fn test_lsp_to_offset_crlf() {
        let source = "[x: 1]\r\n[y: 2]";
        let pos = lsp_types::Position {
            line: 1,
            character: 1,
        };
        let offset = lsp_position_to_offset(&pos, source).unwrap();
        assert_eq!(offset, 9); // 6 ("[x: 1]") + 2 ("\r\n") + 1 ("[") = 9
    }

    #[test]
    fn test_round_trip_crlf() {
        let source = "[x: 42]\r\n[y: hello]";
        let span = make_span(2, 5, 2, 10);
        let range = llt_span_to_lsp_range(&span, source);

        let offset_start = lsp_position_to_offset(&range.start, source).unwrap();
        let offset_end = lsp_position_to_offset(&range.end, source).unwrap();

        assert_eq!(&source[offset_start..offset_end], "hello");
    }

    #[test]
    fn test_file_path_round_trip() {
        // An absolute path must survive file_path_to_uri → uri_to_file_path intact.
        let path = std::path::Path::new("/tmp/test_file.llt");
        let uri = file_path_to_uri(path).expect("absolute path should produce a URI");
        let recovered = uri_to_file_path(&uri).expect("file:// URI should convert back to a path");
        assert_eq!(recovered, path);
    }

    #[test]
    fn test_uri_to_file_path_non_file_scheme_returns_none() {
        // Non-file URIs (https://, etc.) must return None; they have no filesystem path.
        let uri: lsp_types::Uri = "https://example.com".parse().expect("valid URI string");
        assert!(uri_to_file_path(&uri).is_none());
    }

    #[test]
    fn test_file_path_to_uri_relative_path_returns_none() {
        // url::Url::from_file_path requires an absolute path; relative paths return None.
        let path = std::path::Path::new("relative/path.llt");
        assert!(file_path_to_uri(path).is_none());
    }

    #[test]
    fn test_file_path_round_trip_with_spaces() {
        // Paths containing spaces must survive file_path_to_uri → uri_to_file_path intact.
        // Spaces are percent-encoded as %20 in the URI; the round-trip must decode them back.
        let path = std::path::Path::new("/tmp/test file with spaces.llt");
        let uri = file_path_to_uri(path).expect("absolute path with spaces should produce a URI");
        // Verify the URI contains percent-encoded spaces.
        assert!(
            uri.as_str().contains("%20"),
            "URI should percent-encode spaces as %20, got: {}",
            uri.as_str()
        );
        // Verify round-trip fidelity.
        let recovered =
            uri_to_file_path(&uri).expect("file:// URI with %20 should convert back to a path");
        assert_eq!(
            recovered, path,
            "round-trip should recover the original path"
        );
    }
}
