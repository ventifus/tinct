// External scanner for LLT tree-sitter grammar.
// Handles doc_separator: exactly "---" NOT followed by a bare_word_char.

#include "tree_sitter/parser.h"

enum TokenType {
  DOC_SEPARATOR,
};

static bool is_bare_word_char(int32_t c) {
  if (c == '\0') return false;
  switch (c) {
    case ' ': case '\t': case '\r': case '\n':
    case '[': case ']': case ':': case ';':
    case '#': case '"': case '@': case '$':
    case '.':
      return false;
    default:
      return true;
  }
}

void *tree_sitter_llt_external_scanner_create(void) { return NULL; }
void tree_sitter_llt_external_scanner_destroy(void *payload) {}
unsigned tree_sitter_llt_external_scanner_serialize(void *payload, char *buffer) { return 0; }
void tree_sitter_llt_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {}

bool tree_sitter_llt_external_scanner_scan(
  void *payload,
  TSLexer *lexer,
  const bool *valid_symbols
) {
  if (!valid_symbols[DOC_SEPARATOR]) return false;

  // Skip whitespace/newlines (extras may not be consumed before external scan).
  while (lexer->lookahead == ' ' || lexer->lookahead == '\t' ||
         lexer->lookahead == '\r' || lexer->lookahead == '\n') {
    lexer->advance(lexer, true);
  }

  if (lexer->lookahead != '-') return false;
  lexer->advance(lexer, false);

  if (lexer->lookahead != '-') return false;
  lexer->advance(lexer, false);

  if (lexer->lookahead != '-') return false;
  lexer->advance(lexer, false);

  // "---" matched. Check that next char is NOT a bare_word_char.
  // This ensures "----" (and longer) are bare words, not separators.
  if (is_bare_word_char(lexer->lookahead)) return false;

  lexer->mark_end(lexer);
  lexer->result_symbol = DOC_SEPARATOR;
  return true;
}
