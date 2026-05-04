; LLT Tree-sitter Highlight Queries

; Keywords
(keyword_call) @keyword
(keyword_fn) @keyword
(keyword_type) @keyword

; Comments
(comment) @comment

; Literals
(int_lit) @number
(float_lit) @number.float
(bool_lit) @boolean
(quoted_string) @string
(escape_sequence) @string.escape

; Variables
(escaped_ref) @variable

; Parameters
(param (param_name) @variable.parameter)
(variadic_param (param_name) @variable.parameter)

; Named arguments in calls
(named_arg (named_arg_key) @variable.parameter)

; Dict keys
(keyed_entry key: (_) @property)

; Access chains
(dot_access (access_field) @property)

; Rest entry names (must precede general annotation rules)
(rest_entry (annotation_word) @variable)

; Annotations and types
(type_assert "@" @punctuation.special)
(type_assert annotation: (_) @type)
(fn_annotation (annotation_word) @type)
(fn_annotation (bracket_expr) @type)
(param_annotation (annotation_word) @type)
(param_annotation (bracket_expr) @type)
(annotated_bare name: (identifier) @variable)
(annotated_bare annotation: (annotation_word) @type)

; Range operator
".." @operator

; Brackets
"[" @punctuation.bracket
"]" @punctuation.bracket

; Delimiters
":" @punctuation.delimiter
";" @punctuation.delimiter

; Document separator and section headers
(doc_separator) @punctuation.special
(section_name) @namespace
(output_annotation "@" @punctuation.special)
(output_annotation (_) @type)
(expects_pragma "expects" @keyword)
(expects_pragma ":" @punctuation.delimiter)
(expects_pragma (_) @type)

; Rest / spread
"..." @punctuation.special

; Identifiers (unquoted symbols / variable references)
(identifier) @variable
