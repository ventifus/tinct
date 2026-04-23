/// <reference types="tree-sitter-cli/dsl" />
// @ts-check
//
// Tree-sitter grammar for LLT (tinct)
// Ported from src/grammar.pest. See doc/02-syntax.md and doc/15-ast.md.

// Dot excluded from bare_word (unlike pest) to avoid ambiguity with
// range_expr/dot_access; bare words like file.txt must be quoted.
const BARE_WORD_CHAR = /[^\s\[\]:;#"@$.]/;
const VAR_IDENT_CHAR = /[^\s\[\]:;#"@.]/;

module.exports = grammar({
  name: "llt",

  extras: ($) => [/\s/, $.comment],

  externals: ($) => [$.doc_separator],

  conflicts: ($) => [
    // Keywords (call, fn, type) can appear as values (aliased to bare_word
    // in _atom) or as the leading keyword of a special form. When tree-
    // sitter sees `[ call [`, it cannot tell if `call` is an auto_entry
    // value or the start of a call_form until more tokens are consumed.
    // GLR conflict resolution lets both paths proceed; the correct parse
    // survives.
    [$.call_form, $._atom],
    [$.fn_form, $._atom],
    [$.type_form, $._atom],
  ],

  word: ($) => $.bare_word,

  rules: {
    // === File and Document Structure ===

    file: ($) =>
      seq(
        optional($.document),
        repeat(seq($.doc_separator, optional($.document))),
      ),

    document: ($) => repeat1($.expression),

    expression: ($) => $._value,

    // === Values ===

    _value: ($) =>
      choice(
        $.access_expr,
        $.bracket_expr,
        $._atom,
      ),

    _atom: ($) =>
      choice(
        $.float_lit,
        $.int_lit,
        $.bool_lit,
        $.quoted_string,
        $.var_ref,
        $.annotated_bare,
        $.bare_word,
        alias($.keyword_call, $.bare_word),
        alias($.keyword_fn, $.bare_word),
        alias($.keyword_type, $.bare_word),
      ),

    // === Annotated bare words: name@Type ===

    annotated_bare: ($) =>
      prec(2, seq(
        field("name", $.bare_word),
        token.immediate("@"),
        field("annotation", $._annotation_value),
      )),

    // === Bracket expressions ===

    bracket_expr: ($) =>
      choice(
        $.empty_bracket,
        $.type_assert,
        $.call_form,
        $.fn_form,
        $.type_form,
        $.dict,
      ),

    empty_bracket: (_$) => prec(1, seq("[", "]")),

    // === Type assertions: [@Type value] ===

    type_assert: ($) =>
      seq(
        "[",
        "@",
        field("annotation", $._annotation_value),
        field("expr", $._value),
        "]",
      ),

    // === Special forms ===

    call_form: ($) =>
      seq(
        "[",
        $.keyword_call,
        field("func", $._value),
        optional($.call_args),
        "]",
      ),

    call_args: ($) =>
      repeat1(
        choice(
          $.named_arg,
          $._value,
        ),
      ),

    named_arg: ($) =>
      prec(3, seq(
        field("name", $.named_arg_key),
        ":",
        field("value", $._value),
      )),

    named_arg_key: ($) =>
      choice(
        $.var_ref,
        $.bare_word,
      ),

    fn_form: ($) =>
      seq(
        "[",
        $.keyword_fn,
        optional($.fn_annotation),
        $.param_list,
        field("body", $._value),
        "]",
      ),

    fn_annotation: ($) =>
      seq(
        "@",
        $._annotation_value,
      ),

    param_list: ($) =>
      seq(
        "[",
        repeat(
          choice(
            $.variadic_param,
            $.param,
          ),
        ),
        "]",
      ),

    param: ($) =>
      seq(
        $.param_name,
        optional($.param_annotation),
      ),

    param_name: (_$) =>
      token(seq(
        /[a-zA-Z_]/,
        repeat(/[a-zA-Z0-9_-]/),
        optional("?"),
      )),

    param_annotation: ($) =>
      seq(
        token.immediate("@"),
        $._annotation_value,
      ),

    variadic_param: ($) =>
      seq(
        "...",
        $.param_name,
      ),

    type_form: ($) =>
      seq(
        "[",
        $.keyword_type,
        field("expr", $._value),
        "]",
      ),

    // Keywords: matched as plain strings. tree-sitter's `word` rule
    // handles the word-boundary check — bare_word is the word rule, so
    // "call" will be recognized as a keyword rather than bare_word when
    // used in keyword position. The colon-ahead check (call: x is a dict
    // entry, not a keyword) is handled structurally: keyed_entry has
    // higher precedence than special forms.
    keyword_call: (_$) => "call",
    keyword_fn: (_$) => "fn",
    keyword_type: (_$) => "type",

    // === Annotation values ===

    _annotation_value: ($) =>
      choice(
        $.bracket_expr,
        $.annotation_word,
      ),

    annotation_word: (_$) =>
      token(seq(
        /[a-zA-Z_]/,
        repeat(/[a-zA-Z0-9_-]/),
        optional("?"),
      )),

    // === Dict entries ===

    dict: ($) =>
      seq(
        "[",
        repeat(
          seq(
            $._entry,
            optional(";"),
          ),
        ),
        "]",
      ),

    _entry: ($) =>
      choice(
        $.keyed_entry,
        $.rest_entry,
        $.auto_entry,
      ),

    keyed_entry: ($) =>
      prec(2, seq(
        field("key", $._key),
        ":",
        field("value", $._value),
      )),

    rest_entry: ($) =>
      seq(
        "...",
        optional(alias($._rest_name, $.annotation_word)),
      ),

    _rest_name: (_$) =>
      token.immediate(seq(
        /[a-zA-Z_]/,
        repeat(/[a-zA-Z0-9_-]/),
        optional("?"),
      )),

    auto_entry: ($) =>
      $._value,

    _key: ($) =>
      choice(
        $.bracket_expr,
        $.var_ref,
        $.quoted_string,
        $._bare_token,
      ),

    _bare_token: ($) =>
      choice(
        $.float_lit,
        $.int_lit,
        $.bool_lit,
        $.bare_word,
        alias($.keyword_call, $.bare_word),
        alias($.keyword_fn, $.bare_word),
        alias($.keyword_type, $.bare_word),
      ),

    // === Access chains ===

    access_expr: ($) =>
      prec(10, seq(
        $.var_ref,
        repeat1($._access_chain),
      )),

    _access_chain: ($) =>
      choice(
        $.dot_access,
        $.bracket_access,
      ),

    dot_access: ($) =>
      seq(
        token.immediate("."),
        $.access_field,
      ),

    access_field: (_$) =>
      token.immediate(seq(
        /[a-zA-Z_]/,
        repeat(/[a-zA-Z0-9_-]/),
        optional("?"),
      )),

    bracket_access: ($) =>
      seq(
        token.immediate("["),
        $._bracket_access_inner,
        "]",
      ),

    _bracket_access_inner: ($) =>
      choice(
        $.range_expr,
        $._value,
      ),

    range_expr: ($) =>
      seq(
        optional(field("start", $._range_value)),
        "..",
        optional(field("end", $._range_value)),
      ),

    _range_value: ($) =>
      choice(
        $.float_lit,
        $.int_lit,
        $.var_ref,
      ),

    // === Literals ===

    var_ref: (_$) =>
      token(seq(
        "$",
        repeat1(VAR_IDENT_CHAR),
      )),

    float_lit: (_$) =>
      token(prec(2, seq(
        optional("-"),
        /[0-9]+/,
        ".",
        /[0-9]+/,
      ))),

    int_lit: (_$) =>
      token(prec(1, seq(
        optional("-"),
        /[0-9]+/,
      ))),

    bool_lit: (_$) => choice("true", "false"),

    quoted_string: ($) =>
      seq(
        '"',
        repeat(choice($.escape_sequence, $.string_content)),
        token.immediate('"'),
      ),

    escape_sequence: (_$) =>
      token.immediate(/\\["\\ntr]/),

    string_content: (_$) =>
      token.immediate(/[^"\\]+/),

    // === Bare words ===

    bare_word: (_$) =>
      token(seq(
        /[^\s\[\]:;#"@$.\-]/,
        repeat(BARE_WORD_CHAR),
      )),

    // === Comments ===

    comment: (_$) => token(seq("#", /[^\r\n]*/)),
  },
});
