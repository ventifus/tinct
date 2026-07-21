/// Proc macro crate for tinct — derives bidirectional AST conversions.
///
/// `#[derive(ExprConvert)]` on an enum generates `to_expr_variant` and
/// `from_expr_variant` methods that convert between Rust `SurfaceExpression`
/// variants and runtime `Value::Variant` (Expr.* tagged) values.
///
/// # Attribute vocabulary
///
/// **On the enum:**
/// - `#[expr(prefix = "Expr", helpers = "crate::surface_convert")]`
///
/// **On variants:**
/// - `tag = "VarRef"` — the unqualified runtime tag name
/// - `kind = "int"` — discriminator for N-to-1 grouped variants sharing a tag
/// - `inject(bare = true)` — emit constant bool field `bare: true` in payload
/// - `unit` — unit variant; no payload fields
/// - `skip` — omit; produces `AstError` in `from_expr`, placeholder in `to_expr`
///
/// **On fields:**
/// - `key = "name"` — runtime dict key (kebab-case)
/// - `key_aliases = ["let_bindings"]` — alternative keys for `from_expr`
/// - Conversion strategy (bare ident): `child`, `child_opt`, `child_list`, `entry_list`,
///   `named_arg_list`, `param_list`, `match_arm_list`, `annotation`,
///   `annotation_opt`, `string_opt`, `dot_key`, `span`, `ann_span_flat`
/// - `skip` — omit from payload; use `default = <literal>` or `default_fn = "path"`
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    parse_macro_input, Data, DeriveInput, Fields, Ident, Lit, Meta, MetaList, MetaNameValue,
    NestedMeta, Type,
};

// ============================================================================
// Parsed attribute structures
// ============================================================================

/// Attributes parsed from `#[expr(...)]` on the enum itself.
#[derive(Default)]
struct EnumAttrs {
    prefix: Option<String>,
    helpers: Option<String>,
}

/// Attributes parsed from `#[expr(...)]` on a variant.
#[derive(Default)]
struct VariantAttrs {
    tag: Option<String>,
    kind: Option<String>,
    inject: Vec<(String, bool)>, // (field_name, bool_value)
    unit: bool,
    skip: bool,
}

/// Conversion strategy for a single field.
#[derive(Debug, Clone, PartialEq)]
enum FieldStrategy {
    /// Primitive: dispatch on Rust type (String→str, bool→bool, i64→int, u64→u64, f64→float)
    Primitive,
    /// Arc<SurfaceNode> → recursive ExprConvert
    Child,
    /// Option<Arc<SurfaceNode>> → child variant or null (absent)
    ChildOpt,
    /// Vec<Arc<SurfaceNode>> → integer-keyed dict of child variants
    ChildList,
    /// Vec<Spanned<SurfaceEntry>> → delegate to helper
    EntryList,
    /// Vec<Spanned<SurfaceNamedArg>> → delegate to helper
    NamedArgList,
    /// Vec<Spanned<SurfaceParam>> → delegate to helper
    ParamList,
    /// Vec<SurfaceMatchArm> → delegate to helper
    MatchArmList,
    /// Spanned<Annotation> → delegate to helper
    Annotation,
    /// Option<Spanned<Annotation>> → delegate to helper
    AnnotationOpt,
    /// Option<String> → string or null
    StringOpt,
    /// DotKey → String (Ident) or Int (Int)
    DotKey,
    /// Span → span dict
    Span,
    /// Like Span but also emits 6 flat span fields after the main field
    AnnSpanFlat,
    /// Skip this field; use default or default_fn for from_expr reconstruction
    Skip,
}

/// Attributes parsed from `#[expr(...)]` on a field.
#[derive(Default)]
struct FieldAttrs {
    key: Option<String>,
    key_aliases: Vec<String>,
    strategy: Option<FieldStrategy>,
    skip: bool,
    default: Option<String>,
    default_fn: Option<String>,
}

// ============================================================================
// Attribute parsing helpers
// ============================================================================

fn parse_enum_attrs(attrs: &[syn::Attribute]) -> Result<EnumAttrs, syn::Error> {
    let mut result = EnumAttrs::default();
    for attr in attrs {
        if !attr.path.is_ident("expr") {
            continue;
        }
        let meta = attr.parse_meta()?;
        if let Meta::List(MetaList { nested, .. }) = meta {
            for item in nested {
                if let NestedMeta::Meta(Meta::NameValue(MetaNameValue { path, lit, .. })) = item {
                    if path.is_ident("prefix") {
                        if let Lit::Str(s) = lit {
                            result.prefix = Some(s.value());
                        }
                    } else if path.is_ident("helpers") {
                        if let Lit::Str(s) = lit {
                            result.helpers = Some(s.value());
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}

fn parse_variant_attrs(attrs: &[syn::Attribute]) -> Result<VariantAttrs, syn::Error> {
    let mut result = VariantAttrs::default();
    for attr in attrs {
        if !attr.path.is_ident("expr") {
            continue;
        }
        let meta = attr.parse_meta()?;
        if let Meta::List(MetaList { nested, .. }) = meta {
            for item in &nested {
                match item {
                    NestedMeta::Meta(Meta::NameValue(MetaNameValue { path, lit, .. })) => {
                        if path.is_ident("tag") {
                            if let Lit::Str(s) = lit {
                                result.tag = Some(s.value());
                            }
                        } else if path.is_ident("kind") {
                            if let Lit::Str(s) = lit {
                                result.kind = Some(s.value());
                            }
                        }
                    }
                    NestedMeta::Meta(Meta::Path(p)) => {
                        if p.is_ident("unit") {
                            result.unit = true;
                        } else if p.is_ident("skip") {
                            result.skip = true;
                        }
                    }
                    NestedMeta::Meta(Meta::List(MetaList {
                        path,
                        nested: inner,
                        ..
                    })) if path.is_ident("inject") => {
                        // inject(bare = true) or inject(field = true/false, ...)
                        for inject_item in inner {
                            if let NestedMeta::Meta(Meta::NameValue(MetaNameValue {
                                path: field_path,
                                lit: Lit::Bool(b),
                                ..
                            })) = inject_item
                            {
                                let field_name = field_path
                                    .get_ident()
                                    .map(|id| id.to_string())
                                    .unwrap_or_default();
                                result.inject.push((field_name, b.value));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(result)
}

fn parse_field_attrs(attrs: &[syn::Attribute]) -> Result<FieldAttrs, syn::Error> {
    let mut result = FieldAttrs::default();
    for attr in attrs {
        if !attr.path.is_ident("expr") {
            continue;
        }
        let meta = attr.parse_meta()?;
        if let Meta::List(MetaList { nested, .. }) = meta {
            for item in &nested {
                match item {
                    NestedMeta::Meta(Meta::NameValue(MetaNameValue { path, lit, .. })) => {
                        if path.is_ident("key") {
                            if let Lit::Str(s) = lit {
                                result.key = Some(s.value());
                            }
                        } else if path.is_ident("key_aliases") {
                            // key_aliases = "[\"alias1\", \"alias2\"]" — not easily parseable
                            // as MetaNameValue with a list. This won't be hit normally;
                            // key_aliases is a list attribute. Handled below in the List arm.
                        } else if path.is_ident("default") {
                            if let Lit::Str(s) = lit {
                                result.default = Some(s.value());
                            }
                        } else if path.is_ident("default_fn") {
                            if let Lit::Str(s) = lit {
                                result.default_fn = Some(s.value());
                            }
                        }
                    }
                    NestedMeta::Meta(Meta::Path(p)) => {
                        if p.is_ident("skip") {
                            result.skip = true;
                            result.strategy = Some(FieldStrategy::Skip);
                        } else if p.is_ident("child") {
                            result.strategy = Some(FieldStrategy::Child);
                        } else if p.is_ident("child_opt") {
                            result.strategy = Some(FieldStrategy::ChildOpt);
                        } else if p.is_ident("child_list") {
                            result.strategy = Some(FieldStrategy::ChildList);
                        } else if p.is_ident("entry_list") {
                            result.strategy = Some(FieldStrategy::EntryList);
                        } else if p.is_ident("named_arg_list") {
                            result.strategy = Some(FieldStrategy::NamedArgList);
                        } else if p.is_ident("param_list") {
                            result.strategy = Some(FieldStrategy::ParamList);
                        } else if p.is_ident("match_arm_list") {
                            result.strategy = Some(FieldStrategy::MatchArmList);
                        } else if p.is_ident("annotation") {
                            result.strategy = Some(FieldStrategy::Annotation);
                        } else if p.is_ident("annotation_opt") {
                            result.strategy = Some(FieldStrategy::AnnotationOpt);
                        } else if p.is_ident("string_opt") {
                            result.strategy = Some(FieldStrategy::StringOpt);
                        } else if p.is_ident("dot_key") {
                            result.strategy = Some(FieldStrategy::DotKey);
                        } else if p.is_ident("span") {
                            result.strategy = Some(FieldStrategy::Span);
                        } else if p.is_ident("ann_span_flat") {
                            result.strategy = Some(FieldStrategy::AnnSpanFlat);
                        }
                    }
                    NestedMeta::Meta(Meta::List(MetaList {
                        path,
                        nested: inner,
                        ..
                    })) if path.is_ident("key_aliases") => {
                        // key_aliases(["alias1", "alias2"]) — parse list of string literals
                        for alias_item in inner {
                            if let NestedMeta::Lit(Lit::Str(s)) = alias_item {
                                result.key_aliases.push(s.value());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(result)
}

// ============================================================================
// Description of a processed field
// ============================================================================

struct FieldDesc {
    /// Rust field name (ident)
    ident: Ident,
    /// Rust field type (used to dispatch Primitive strategy helpers)
    ty: Type,
    /// Runtime dict key (kebab-case)
    key: String,
    /// Alternative runtime keys accepted during `from_expr`
    key_aliases: Vec<String>,
    /// Conversion strategy
    strategy: FieldStrategy,
    /// Whether this field is skipped for to_expr (still needed for from_expr default)
    skip: bool,
    /// Default literal string for skipped fields in from_expr
    default: Option<String>,
    /// Default function path for skipped fields in from_expr
    default_fn: Option<String>,
}

// ============================================================================
// Core derive macro entry point
// ============================================================================

#[proc_macro_derive(ExprConvert, attributes(expr))]
pub fn derive_expr_convert(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match derive_impl(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn derive_impl(input: DeriveInput) -> Result<TokenStream2, syn::Error> {
    let enum_name = &input.ident;

    // Parse enum-level attributes
    let enum_attrs = parse_enum_attrs(&input.attrs)?;
    let prefix = enum_attrs.prefix.unwrap_or_else(|| "Expr".to_string());
    let helpers_path: syn::Path = syn::parse_str(
        &enum_attrs
            .helpers
            .unwrap_or_else(|| "crate::surface_convert".to_string()),
    )?;

    // Require enum data
    let data_enum = match &input.data {
        Data::Enum(e) => e,
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "ExprConvert can only be derived on enums",
            ))
        }
    };

    // Process variants
    let mut to_expr_arms: Vec<TokenStream2> = Vec::new();
    let mut from_expr_entries: Vec<FromExprEntry> = Vec::new();

    for variant in &data_enum.variants {
        let variant_ident = &variant.ident;
        let vattrs = parse_variant_attrs(&variant.attrs)?;

        if vattrs.skip {
            // Skipped variant: emit placeholder in to_expr, no from_expr arm.
            // The wildcard pattern syntax depends on the variant's field kind.
            let skip_pat = match &variant.fields {
                Fields::Named(_) => quote! { { .. } },
                Fields::Unnamed(_) => quote! { (..) },
                Fields::Unit => quote! {},
            };
            let arm = quote! {
                #enum_name::#variant_ident #skip_pat => {
                    #helpers_path::make_unit_variant("Expr.AstError")
                }
            };
            to_expr_arms.push(arm);
            continue;
        }

        if vattrs.unit {
            // Unit variant: no payload
            let tag_name = vattrs
                .tag
                .clone()
                .unwrap_or_else(|| variant_ident.to_string());
            let qualified_tag = format!("{}.{}", prefix, tag_name);

            let arm = quote! {
                #enum_name::#variant_ident => {
                    #helpers_path::make_unit_variant(#qualified_tag)
                }
            };
            to_expr_arms.push(arm);

            from_expr_entries.push(FromExprEntry {
                tag: tag_name,
                kind: None,
                variant_ident: variant_ident.clone(),
                fields: vec![],
                is_unit: true,
                is_tuple: false,
                enum_name: enum_name.clone(),
            });
            continue;
        }

        let tag_name = vattrs
            .tag
            .clone()
            .unwrap_or_else(|| variant_ident.to_string());
        let qualified_tag = format!("{}.{}", prefix, tag_name);

        // Collect field descriptions
        let field_descs = collect_field_descs(&variant.fields)?;

        // ---- to_expr arm ----
        let to_arm = build_to_expr_arm(
            enum_name,
            variant_ident,
            &variant.fields,
            &field_descs,
            &qualified_tag,
            &vattrs,
            &helpers_path,
        )?;
        to_expr_arms.push(to_arm);

        // ---- from_expr entry ----
        let is_tuple = matches!(variant.fields, Fields::Unnamed(_));
        from_expr_entries.push(FromExprEntry {
            tag: tag_name,
            kind: vattrs.kind.clone(),
            variant_ident: variant_ident.clone(),
            fields: field_descs,
            is_unit: false,
            is_tuple,
            enum_name: enum_name.clone(),
        });
    }

    // Build from_expr match arms, grouping by tag
    let from_expr_match = build_from_expr_match(&from_expr_entries, &helpers_path)?;

    // Final impl
    let expanded = quote! {
        impl #enum_name {
            pub fn to_expr_variant(
                node: &::std::sync::Arc<crate::ast::SurfaceNode>,
                ctx: &::std::sync::Arc<crate::eval::EvalContext>,
            ) -> crate::value::Value {
                let __span = &node.span;
                match &node.expr {
                    #(#to_expr_arms)*
                }
            }

            pub fn from_expr_variant(
                val: &crate::value::Value,
                ctx: &::std::sync::Arc<crate::eval::EvalContext>,
            ) -> ::std::result::Result<::std::sync::Arc<crate::ast::SurfaceNode>, crate::surface_convert::AstError> {
                let (tag, dict) = #helpers_path::extract_tag_and_dict(val, ctx)?;
                match tag.as_str() {
                    #from_expr_match
                    _ => Err(crate::surface_convert::AstError {
                        message: format!("unknown Expr tag: {}", tag),
                        field_path: vec![],
                    }),
                }
            }
        }
    };

    Ok(expanded)
}

// ============================================================================
// Field description collection
// ============================================================================

fn collect_field_descs(fields: &Fields) -> Result<Vec<FieldDesc>, syn::Error> {
    let mut descs = Vec::new();
    match fields {
        Fields::Named(named) => {
            for field in &named.named {
                let ident = field.ident.clone().expect("named field has ident");
                let attrs = parse_field_attrs(&field.attrs)?;

                let key = attrs
                    .key
                    .clone()
                    .unwrap_or_else(|| ident.to_string().replace('_', "-"));
                let strategy = attrs.strategy.clone().unwrap_or(FieldStrategy::Primitive);
                let skip = attrs.skip;

                descs.push(FieldDesc {
                    ident,
                    ty: field.ty.clone(),
                    key,
                    key_aliases: attrs.key_aliases,
                    strategy,
                    skip,
                    default: attrs.default,
                    default_fn: attrs.default_fn,
                });
            }
        }
        Fields::Unnamed(unnamed) => {
            for (i, field) in unnamed.unnamed.iter().enumerate() {
                let ident = Ident::new(&format!("__val{}", i), proc_macro2::Span::call_site());
                let attrs = parse_field_attrs(&field.attrs)?;
                let key = attrs.key.clone().unwrap_or_else(|| "value".to_string());
                let strategy = attrs.strategy.clone().unwrap_or(FieldStrategy::Primitive);
                let skip = attrs.skip;

                descs.push(FieldDesc {
                    ident,
                    ty: field.ty.clone(),
                    key,
                    key_aliases: attrs.key_aliases,
                    strategy,
                    skip,
                    default: attrs.default,
                    default_fn: attrs.default_fn,
                });
            }
        }
        Fields::Unit => {}
    }
    Ok(descs)
}

// ============================================================================
// Primitive type dispatch helpers
// ============================================================================

/// Map a Rust field type to the `alloc_*` helper function name and value expression suffix for `to_expr`.
///
/// Returns `(fn_name, needs_as_str)` where `needs_as_str` indicates the field binding must be
/// coerced to `&str` via `.as_str()` before passing (for `String` fields).
fn primitive_alloc_fn(ty: &Type) -> (Ident, bool) {
    let ty_str = quote!(#ty).to_string();
    // Normalize whitespace for consistent matching
    let ty_str = ty_str.replace(' ', "");
    let (name, needs_as_str) = match ty_str.as_str() {
        "bool" => ("alloc_bool", false),
        "i64" => ("alloc_int", false),
        "u64" => ("alloc_u64", false),
        "f64" => ("alloc_float", false),
        // String and Arc<String> both convert via alloc_str; need .as_str() coercion
        _ => ("alloc_str", true),
    };
    (
        Ident::new(name, proc_macro2::Span::call_site()),
        needs_as_str,
    )
}

/// Map a Rust field type to the `get_*_field_with_aliases` helper function name for `from_expr`.
fn primitive_get_fn(ty: &Type) -> Ident {
    let ty_str = quote!(#ty).to_string();
    let ty_str = ty_str.replace(' ', "");
    let name = match ty_str.as_str() {
        "bool" => "get_bool_field_with_aliases",
        "i64" => "get_int_field_with_aliases",
        "u64" => "get_u64_field_with_aliases",
        "f64" => "get_float_field_with_aliases",
        _ => "get_string_field_with_aliases",
    };
    Ident::new(name, proc_macro2::Span::call_site())
}

// ============================================================================
// to_expr arm generation
// ============================================================================

fn build_to_expr_arm(
    enum_name: &Ident,
    variant_ident: &Ident,
    fields: &Fields,
    descs: &[FieldDesc],
    qualified_tag: &str,
    vattrs: &VariantAttrs,
    helpers: &syn::Path,
) -> Result<TokenStream2, syn::Error> {
    // Build destructuring pattern and field insertion statements
    let (destructure_pat, insert_stmts) =
        build_to_expr_body(fields, descs, qualified_tag, vattrs, helpers)?;

    let arm = quote! {
        #enum_name::#variant_ident #destructure_pat => {
            let mut __payload: ::indexmap::IndexMap<crate::value::HashableValue, crate::value::ThunkId> = ::indexmap::IndexMap::new();
            #insert_stmts
            #helpers::make_variant_with_payload(#qualified_tag, __payload, __span, ctx)
        }
    };
    Ok(arm)
}

fn build_to_expr_body(
    fields: &Fields,
    descs: &[FieldDesc],
    _qualified_tag: &str,
    vattrs: &VariantAttrs,
    helpers: &syn::Path,
) -> Result<(TokenStream2, TokenStream2), syn::Error> {
    // Destructuring pattern
    let destructure_pat = match fields {
        Fields::Named(named) => {
            let bindings: Vec<TokenStream2> = named
                .named
                .iter()
                .zip(descs.iter())
                .map(|(f, desc)| {
                    let id = f.ident.as_ref().unwrap();
                    if desc.skip {
                        // Skipped fields are not used in the arm body; bind as wildcard
                        // to suppress unused variable warnings.
                        quote! { #id: _ }
                    } else {
                        quote! { #id }
                    }
                })
                .collect();
            // Check if there are any skipped-via-`..` fields — always use `..` for safety
            quote! { { #(#bindings,)* .. } }
        }
        Fields::Unnamed(unnamed) => {
            let bindings: Vec<TokenStream2> = (0..unnamed.unnamed.len())
                .map(|i| {
                    let id = Ident::new(&format!("__val{}", i), proc_macro2::Span::call_site());
                    quote! { #id }
                })
                .collect();
            quote! { (#(#bindings),*) }
        }
        Fields::Unit => quote! {},
    };

    let mut stmts: Vec<TokenStream2> = Vec::new();

    // Inject kind discriminator if present
    if let Some(kind_val) = &vattrs.kind {
        stmts.push(quote! {
            __payload.insert(
                crate::value::HashableValue::Str("kind".into()),
                #helpers::alloc_str(#kind_val, __span, ctx),
            );
        });
    }

    // Inject constant bool fields (inject attribute)
    for (field_name, bool_val) in &vattrs.inject {
        stmts.push(quote! {
            __payload.insert(
                crate::value::HashableValue::Str(#field_name.into()),
                #helpers::alloc_bool(#bool_val, __span, ctx),
            );
        });
    }

    // Field insertions
    for desc in descs {
        if desc.skip {
            continue;
        }

        let key = &desc.key;
        let ident = &desc.ident;

        let alloc_expr = match &desc.strategy {
            FieldStrategy::Primitive => {
                // Dispatch to the correct typed helper based on the field's Rust type.
                let (alloc_fn, needs_as_str) = primitive_alloc_fn(&desc.ty);
                if needs_as_str {
                    quote! {
                        #helpers::#alloc_fn(#ident.as_str(), __span, ctx)
                    }
                } else {
                    quote! {
                        #helpers::#alloc_fn(*#ident, __span, ctx)
                    }
                }
            }
            FieldStrategy::Child => quote! {
                #helpers::alloc_expr_child(#ident, ctx)
            },
            FieldStrategy::ChildOpt => quote! {
                #helpers::alloc_expr_child_opt(#ident.as_ref(), ctx)
            },
            FieldStrategy::ChildList => quote! {
                #helpers::alloc_child_list(#ident, ctx)
            },
            FieldStrategy::EntryList => quote! {
                #helpers::alloc_entry_list(#ident, ctx)
            },
            FieldStrategy::NamedArgList => quote! {
                #helpers::alloc_named_arg_list(#ident, ctx)
            },
            FieldStrategy::ParamList => quote! {
                #helpers::alloc_param_list(#ident, ctx)
            },
            FieldStrategy::MatchArmList => quote! {
                #helpers::alloc_match_arm_list(#ident, ctx)
            },
            FieldStrategy::Annotation => quote! {
                #helpers::alloc_annotation(#ident, ctx)
            },
            FieldStrategy::AnnotationOpt => quote! {
                #helpers::alloc_annotation_opt(#ident.as_ref(), ctx)
            },
            FieldStrategy::StringOpt => quote! {
                #helpers::alloc_string_opt(#ident.as_deref(), ctx)
            },
            FieldStrategy::DotKey => quote! {
                #helpers::alloc_dot_key(#ident, __span, ctx)
            },
            FieldStrategy::Span => quote! {
                #helpers::alloc_span(#ident, ctx)
            },
            FieldStrategy::AnnSpanFlat => {
                // Emits the main annotation field plus 6 flat span fields.
                // The main insert happens below; we also emit the 6 extra fields here.
                let ann_start_offset_key = format!("{}-start-offset", key);
                let ann_start_line_key = format!("{}-start-line", key);
                let ann_start_col_key = format!("{}-start-col", key);
                let ann_end_offset_key = format!("{}-end-offset", key);
                let ann_end_line_key = format!("{}-end-line", key);
                let ann_end_col_key = format!("{}-end-col", key);

                // Push the 6 flat span fields first (main field inserted below)
                stmts.push(quote! {
                    __payload.insert(
                        crate::value::HashableValue::Str(#key.into()),
                        #helpers::alloc_annotation(#ident, ctx),
                    );
                    __payload.insert(
                        crate::value::HashableValue::Str(#ann_start_offset_key.into()),
                        #helpers::alloc_int(#ident.span.start.offset as i64, __span, ctx),
                    );
                    __payload.insert(
                        crate::value::HashableValue::Str(#ann_start_line_key.into()),
                        #helpers::alloc_int(#ident.span.start.line as i64, __span, ctx),
                    );
                    __payload.insert(
                        crate::value::HashableValue::Str(#ann_start_col_key.into()),
                        #helpers::alloc_int(#ident.span.start.column as i64, __span, ctx),
                    );
                    __payload.insert(
                        crate::value::HashableValue::Str(#ann_end_offset_key.into()),
                        #helpers::alloc_int(#ident.span.end.offset as i64, __span, ctx),
                    );
                    __payload.insert(
                        crate::value::HashableValue::Str(#ann_end_line_key.into()),
                        #helpers::alloc_int(#ident.span.end.line as i64, __span, ctx),
                    );
                    __payload.insert(
                        crate::value::HashableValue::Str(#ann_end_col_key.into()),
                        #helpers::alloc_int(#ident.span.end.column as i64, __span, ctx),
                    );
                });
                // Skip the normal insert below (already pushed above)
                continue;
            }
            FieldStrategy::Skip => continue,
        };

        stmts.push(quote! {
            __payload.insert(
                crate::value::HashableValue::Str(#key.into()),
                #alloc_expr,
            );
        });
    }

    let stmts_ts = quote! { #(#stmts)* };
    Ok((destructure_pat, stmts_ts))
}

// ============================================================================
// from_expr arm generation
// ============================================================================

struct FromExprEntry {
    tag: String,
    kind: Option<String>,
    variant_ident: Ident,
    fields: Vec<FieldDesc>,
    is_unit: bool,
    is_tuple: bool,
    enum_name: Ident,
}

fn build_from_expr_match(
    entries: &[FromExprEntry],
    helpers: &syn::Path,
) -> Result<TokenStream2, syn::Error> {
    use std::collections::HashMap;

    // Group entries by tag
    let mut by_tag: HashMap<String, Vec<&FromExprEntry>> = HashMap::new();
    for entry in entries {
        by_tag.entry(entry.tag.clone()).or_default().push(entry);
    }

    // Order tags deterministically (preserve insertion order from entries)
    let mut ordered_tags: Vec<String> = Vec::new();
    for entry in entries {
        if !ordered_tags.contains(&entry.tag) {
            ordered_tags.push(entry.tag.clone());
        }
    }

    let mut match_arms: Vec<TokenStream2> = Vec::new();

    for tag in &ordered_tags {
        let variants = &by_tag[tag];
        let tag_str = tag.as_str();

        if variants.len() == 1 && variants[0].kind.is_none() {
            // Single variant for this tag — no kind discrimination
            let entry = variants[0];
            let arm_body = build_from_expr_arm_body(entry, helpers)?;
            match_arms.push(quote! {
                #tag_str => { #arm_body }
            });
        } else {
            // Multiple variants sharing a tag (grouped by kind) — or single with kind
            let mut kind_arms: Vec<TokenStream2> = Vec::new();
            let mut has_kind = false;

            for entry in variants {
                if let Some(kind_val) = &entry.kind {
                    has_kind = true;
                    let kind_str = kind_val.as_str();
                    let arm_body = build_from_expr_arm_body(entry, helpers)?;
                    kind_arms.push(quote! {
                        #kind_str => { #arm_body }
                    });
                } else if entry.is_unit {
                    // Unit variant with same tag — no kind
                    let arm_body = build_from_expr_arm_body(entry, helpers)?;
                    kind_arms.push(quote! {
                        _ => { #arm_body }
                    });
                }
            }

            if has_kind {
                match_arms.push(quote! {
                    #tag_str => {
                        let __kind = #helpers::get_string_field_with_aliases(&dict, "kind", &[], ctx)?;
                        match __kind.as_str() {
                            #(#kind_arms)*
                            _ => Err(crate::surface_convert::AstError {
                                message: format!("unknown {} kind: {}", #tag_str, __kind),
                                field_path: vec![],
                            }),
                        }
                    }
                });
            } else {
                // No kind — just take first variant (shouldn't happen in well-formed usage)
                let entry = variants[0];
                let arm_body = build_from_expr_arm_body(entry, helpers)?;
                match_arms.push(quote! {
                    #tag_str => { #arm_body }
                });
            }
        }
    }

    Ok(quote! { #(#match_arms)* })
}

fn build_from_expr_arm_body(
    entry: &FromExprEntry,
    helpers: &syn::Path,
) -> Result<TokenStream2, syn::Error> {
    let enum_name = &entry.enum_name;
    let variant_ident = &entry.variant_ident;

    if entry.is_unit {
        return Ok(quote! {
            let __span = #helpers::get_span_from_dict(&dict, ctx);
            Ok(#helpers::make_surface_node(
                #enum_name::#variant_ident,
                __span,
            ))
        });
    }

    // Extract each field
    let mut field_extracts: Vec<TokenStream2> = Vec::new();

    for desc in &entry.fields {
        let ident = &desc.ident;
        let key = &desc.key;
        let aliases: Vec<&str> = desc.key_aliases.iter().map(|s| s.as_str()).collect();
        let aliases_ts = quote! { &[#(#aliases),*] };

        if desc.skip {
            // Use default or default_fn
            if let Some(ref default_fn_str) = desc.default_fn {
                let fn_path: syn::Path = syn::parse_str(default_fn_str).map_err(|e| {
                    syn::Error::new(
                        proc_macro2::Span::call_site(),
                        format!("invalid default_fn path: {}", e),
                    )
                })?;
                field_extracts.push(quote! {
                    let #ident = #fn_path();
                });
            } else if let Some(ref default_str) = desc.default {
                // Parse the default as a Rust literal expression
                let default_expr: syn::Expr = syn::parse_str(default_str).map_err(|e| {
                    syn::Error::new(
                        proc_macro2::Span::call_site(),
                        format!("invalid default: {}", e),
                    )
                })?;
                field_extracts.push(quote! {
                    let #ident = #default_expr;
                });
            } else {
                // No default specified — use Default::default()
                field_extracts.push(quote! {
                    let #ident = ::std::default::Default::default();
                });
            }
            continue;
        }

        let extract_expr = match &desc.strategy {
            FieldStrategy::Primitive => {
                // Dispatch to the correct typed helper based on the field's Rust type.
                let get_fn = primitive_get_fn(&desc.ty);
                quote! {
                    #helpers::#get_fn(&dict, #key, #aliases_ts, ctx)?
                }
            }
            FieldStrategy::Child => quote! {
                #helpers::get_child_field_with_aliases(&dict, #key, #aliases_ts, ctx)?
            },
            FieldStrategy::ChildOpt => quote! {
                #helpers::get_child_opt_field_with_aliases(&dict, #key, #aliases_ts, ctx)?
            },
            FieldStrategy::ChildList => quote! {
                #helpers::get_child_list_field_with_aliases(&dict, #key, #aliases_ts, ctx)?
            },
            FieldStrategy::EntryList => quote! {
                #helpers::get_entry_list_field_with_aliases(&dict, #key, #aliases_ts, ctx)?
            },
            FieldStrategy::NamedArgList => quote! {
                #helpers::get_named_arg_list_field_with_aliases(&dict, #key, #aliases_ts, ctx)?
            },
            FieldStrategy::ParamList => quote! {
                #helpers::get_param_list_field_with_aliases(&dict, #key, #aliases_ts, ctx)?
            },
            FieldStrategy::MatchArmList => quote! {
                #helpers::get_match_arm_list_field_with_aliases(&dict, #key, #aliases_ts, ctx)?
            },
            FieldStrategy::Annotation => quote! {
                #helpers::get_annotation_field_with_aliases(&dict, #key, #aliases_ts, ctx)?
            },
            FieldStrategy::AnnotationOpt => quote! {
                #helpers::get_annotation_opt_field_with_aliases(&dict, #key, #aliases_ts, ctx)?
            },
            FieldStrategy::StringOpt => quote! {
                #helpers::get_string_opt_field_with_aliases(&dict, #key, #aliases_ts, ctx)?
            },
            FieldStrategy::DotKey => quote! {
                #helpers::get_dot_key_field_with_aliases(&dict, #key, #aliases_ts, ctx)?
            },
            FieldStrategy::Span => quote! {
                #helpers::get_span_from_dict(&dict, ctx)
            },
            FieldStrategy::AnnSpanFlat => {
                let start_offset_key = format!("{}-start-offset", desc.key);
                let start_line_key = format!("{}-start-line", desc.key);
                let start_col_key = format!("{}-start-col", desc.key);
                let end_offset_key = format!("{}-end-offset", desc.key);
                let end_line_key = format!("{}-end-line", desc.key);
                let end_col_key = format!("{}-end-col", desc.key);
                quote! {
                    {
                        let __ann = #helpers::get_annotation_field_with_aliases(&dict, #key, #aliases_ts, ctx)?;
                        let __start_offset = #helpers::get_int_field_with_aliases(&dict, #start_offset_key, &[], ctx)? as u32;
                        let __start_line   = #helpers::get_int_field_with_aliases(&dict, #start_line_key,   &[], ctx)? as u32;
                        let __start_col    = #helpers::get_int_field_with_aliases(&dict, #start_col_key,    &[], ctx)? as u32;
                        let __end_offset   = #helpers::get_int_field_with_aliases(&dict, #end_offset_key,   &[], ctx)? as u32;
                        let __end_line     = #helpers::get_int_field_with_aliases(&dict, #end_line_key,     &[], ctx)? as u32;
                        let __end_col      = #helpers::get_int_field_with_aliases(&dict, #end_col_key,      &[], ctx)? as u32;
                        let __ann_span = crate::ast::Span::new(
                            crate::ast::Position { offset: __start_offset, line: __start_line, column: __start_col },
                            crate::ast::Position { offset: __end_offset,   line: __end_line,   column: __end_col   },
                            crate::rust_span!().file.clone(),
                        );
                        crate::ast::Spanned::new(__ann.node, __ann_span)
                    }
                }
            }
            FieldStrategy::Skip => unreachable!("handled above"),
        };

        field_extracts.push(quote! {
            let #ident = #extract_expr;
        });
    }

    // Build the constructor expression
    let constructor = if entry.is_tuple {
        let field_idents: Vec<&Ident> = entry.fields.iter().map(|d| &d.ident).collect();
        quote! {
            #enum_name::#variant_ident(#(#field_idents),*)
        }
    } else {
        // Named fields
        let field_assignments: Vec<TokenStream2> = entry
            .fields
            .iter()
            .map(|d| {
                let rust_ident = &d.ident;
                quote! { #rust_ident }
            })
            .collect();
        quote! {
            #enum_name::#variant_ident { #(#field_assignments),* }
        }
    };

    Ok(quote! {
        #(#field_extracts)*
        let __span = #helpers::get_span_from_dict(&dict, ctx);
        Ok(#helpers::make_surface_node(
            #constructor,
            __span,
        ))
    })
}
