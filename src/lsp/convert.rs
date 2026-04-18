//! Coordinate system conversion between LLT spans and LSP positions.
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
    let line_start_offset = source
        .lines()
        .take(line.saturating_sub(1))
        .map(|l| l.len() + 1) // +1 for newline
        .sum::<usize>();

    // LLT column is a 1-indexed byte offset; convert to 0-indexed.
    let byte_offset_in_line = col.saturating_sub(1);

    // Extract the line text up to the target column (in bytes).
    let line_text = source[line_start_offset..].lines().next().unwrap_or("");
    let prefix = &line_text[..byte_offset_in_line.min(line_text.len())];

    // Count UTF-16 code units in the prefix.
    let utf16_col = prefix.encode_utf16().count() as u32;

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

    // Find the byte offset of the start of the line.
    let mut offset = 0;
    for (i, line) in source.lines().enumerate() {
        if i == line_idx {
            // Found the target line. Now find the UTF-16 character offset.
            let mut utf16_count = 0;
            for (byte_idx, _) in line.char_indices() {
                if utf16_count == utf16_char {
                    return Some(offset + byte_idx);
                }
                utf16_count += line[byte_idx..].chars().next()?.len_utf16();
            }
            // Past all characters on this line; return end-of-line offset.
            if utf16_count == utf16_char {
                return Some(offset + line.len());
            }
            return None; // Character index out of bounds
        }
        offset += line.len() + 1; // +1 for newline
    }
    None // Line index out of bounds
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
        // "[x: é]" is 7 bytes but 6 UTF-16 code units.
        let source = "[x: é]";
        let span = make_span(1, 1, 1, 8); // LLT column 8 = one past the 7-byte string
        let range = llt_span_to_lsp_range(&span, source);

        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 0);
        assert_eq!(range.end.line, 0);
        assert_eq!(range.end.character, 6); // "[x: é]" is 6 UTF-16 code units
    }

    #[test]
    fn test_llt_to_lsp_emoji() {
        // "😀" is 4 bytes in UTF-8, 2 code units in UTF-16 (surrogate pair).
        let source = "[x: 😀]";
        let span = make_span(1, 5, 1, 9); // "😀" occupies bytes 4-8 (1-indexed: cols 5-9)
        let range = llt_span_to_lsp_range(&span, source);

        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 4); // after "[x: "
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
}
