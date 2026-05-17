//! Bidirectional conversion between `Type` and runtime dict representations.
//!
//! This module provides `type_to_dict` (Type → Value::Dict) and `dict_to_type` (Value::Dict → Type)
//! for runtime reflection and metaprogramming. Type dicts use a canonical `kind:` field to
//! dispatch to variant-specific logic.

use std::rc::Rc;

use indexmap::IndexMap;

use crate::arena::{ThunkArena, ThunkId};
use crate::ast::Span;
use crate::types::{Row, Type, TypeError};
use crate::value::{Key, Thunk, ThunkState, Value};

/// Convert a `Type` to its canonical dict representation (for runtime reflection).
///
/// Each type variant maps to a dict with a `kind:` field:
/// - `Int` → `[kind: "named" name: "Int"]`
/// - `Str` → `[kind: "named" name: "String"]`
/// - `Seq(elem)` → `[kind: "seq" elem: <recurse>]`
/// - `Function { params, ret, .. }` → `[kind: "fn" params: [...] ret: <recurse>]`
/// - etc.
///
/// # Arguments
///
/// * `ty` - The type to convert
/// * `arena` - ThunkArena for allocating dict entry thunks
///
/// # Returns
///
/// A `Value::Dict` representing the type structure.
pub fn type_to_dict(ty: &Type, arena: &mut ThunkArena) -> Value {
    match ty {
        Type::Int => make_named_dict("Int", arena),
        Type::IntLiteral(n) => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("int-literal"),
                    Span::origin(),
                ))),
            );
            map.insert(
                Key::String("value".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    Value::Int(*n),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
        Type::Float => make_named_dict("Float", arena),
        Type::Str => make_named_dict("String", arena),
        Type::StringLiteral(s) => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("string-literal"),
                    Span::origin(),
                ))),
            );
            map.insert(
                Key::String("value".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val(s),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
        Type::Bool => make_named_dict("Bool", arena),
        Type::Bytes => make_named_dict("Bytes", arena),
        Type::Number => make_named_dict("Number", arena),
        Type::Record(row) => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("record"),
                    Span::origin(),
                ))),
            );
            // Convert row fields to a dict
            let mut fields_map = IndexMap::new();
            for (field_name, field_ty) in &row.fields {
                let field_ty_dict = type_to_dict(field_ty, arena);
                fields_map.insert(
                    Key::String(field_name.clone()),
                    arena.alloc(Rc::new(Thunk::new_materialized(
                        field_ty_dict,
                        Span::origin(),
                    ))),
                );
            }
            map.insert(
                Key::String("fields".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    Value::Dict(fields_map),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
        Type::Function {
            params,
            ret,
            variadic,
        } => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("fn"),
                    Span::origin(),
                ))),
            );
            // Convert params to a sequence, preserving parameter names
            let mut params_map = IndexMap::new();
            for (i, (param_name, param_ty)) in params.iter().enumerate() {
                if let Some(name) = param_name {
                    // Param with name: [type: <type_dict>  name: "param_name"]
                    let mut param_dict = IndexMap::new();
                    let param_ty_dict = type_to_dict(param_ty, arena);
                    param_dict.insert(
                        Key::String("type".to_string()),
                        arena.alloc(Rc::new(Thunk::new_materialized(
                            param_ty_dict,
                            Span::origin(),
                        ))),
                    );
                    param_dict.insert(
                        Key::String("name".to_string()),
                        arena.alloc(Rc::new(Thunk::new_materialized(
                            string_val(name),
                            Span::origin(),
                        ))),
                    );
                    params_map.insert(
                        Key::Int(i as i64),
                        arena.alloc(Rc::new(Thunk::new_materialized(
                            Value::Dict(param_dict),
                            Span::origin(),
                        ))),
                    );
                } else {
                    // Param without name: just the type dict
                    let param_ty_dict = type_to_dict(param_ty, arena);
                    params_map.insert(
                        Key::Int(i as i64),
                        arena.alloc(Rc::new(Thunk::new_materialized(
                            param_ty_dict,
                            Span::origin(),
                        ))),
                    );
                }
            }
            map.insert(
                Key::String("params".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    Value::Dict(params_map),
                    Span::origin(),
                ))),
            );
            let ret_dict = type_to_dict(ret, arena);
            map.insert(
                Key::String("ret".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(ret_dict, Span::origin()))),
            );
            map.insert(
                Key::String("variadic".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    Value::Bool(*variadic),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
        Type::Seq(elem) => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("seq"),
                    Span::origin(),
                ))),
            );
            let elem_dict = type_to_dict(elem, arena);
            map.insert(
                Key::String("elem".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(elem_dict, Span::origin()))),
            );
            Value::Dict(map)
        }
        Type::Map(key, value) => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("map"),
                    Span::origin(),
                ))),
            );
            let key_dict = type_to_dict(key, arena);
            map.insert(
                Key::String("key".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(key_dict, Span::origin()))),
            );
            let value_dict = type_to_dict(value, arena);
            map.insert(
                Key::String("value".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(value_dict, Span::origin()))),
            );
            Value::Dict(map)
        }
        Type::Proxy => make_named_dict("Proxy", arena),
        Type::TypeVar(name, _level) => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("typevar"),
                    Span::origin(),
                ))),
            );
            map.insert(
                Key::String("name".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val(name),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
        Type::Unknown => make_named_dict("Unknown", arena),
        Type::Top => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("top"),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
        Type::Error => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("error"),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
        Type::DirCap => make_named_dict("DirCap", arena),
        Type::NetCap => make_named_dict("NetCap", arena),
        Type::Handle => make_named_dict("Handle", arena),
        Type::Uri => make_named_dict("Uri", arena),
        Type::Timestamp => make_named_dict("Timestamp", arena),
        Type::Duration => make_named_dict("Duration", arena),
        Type::ClockCap => make_named_dict("ClockCap", arena),
        Type::Timezone => make_named_dict("Timezone", arena),
        Type::QuicSession => make_named_dict("QuicSession", arena),
        Type::Http2Session => make_named_dict("Http2Session", arena),
        Type::Http3Session => make_named_dict("Http3Session", arena),
        Type::QuicDatagramHandle => make_named_dict("QuicDatagramHandle", arena),
        Type::DatagramHandle => make_named_dict("DatagramHandle", arena),
        Type::Union(members) => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("union"),
                    Span::origin(),
                ))),
            );
            let mut members_map = IndexMap::new();
            for (i, member_ty) in members.iter().enumerate() {
                let member_dict = type_to_dict(member_ty, arena);
                members_map.insert(
                    Key::Int(i as i64),
                    arena.alloc(Rc::new(Thunk::new_materialized(
                        member_dict,
                        Span::origin(),
                    ))),
                );
            }
            map.insert(
                Key::String("members".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    Value::Dict(members_map),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
        Type::Intersection(members) => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("intersection"),
                    Span::origin(),
                ))),
            );
            let mut members_map = IndexMap::new();
            for (i, member_ty) in members.iter().enumerate() {
                let member_dict = type_to_dict(member_ty, arena);
                members_map.insert(
                    Key::Int(i as i64),
                    arena.alloc(Rc::new(Thunk::new_materialized(
                        member_dict,
                        Span::origin(),
                    ))),
                );
            }
            map.insert(
                Key::String("members".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    Value::Dict(members_map),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
        Type::Negation(inner) => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("negation"),
                    Span::origin(),
                ))),
            );
            let inner_dict = type_to_dict(inner, arena);
            map.insert(
                Key::String("inner".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(inner_dict, Span::origin()))),
            );
            Value::Dict(map)
        }
        Type::Never => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("never"),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
        Type::App(f, a) => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("app"),
                    Span::origin(),
                ))),
            );
            let f_dict = type_to_dict(f, arena);
            map.insert(
                Key::String("func".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(f_dict, Span::origin()))),
            );
            let a_dict = type_to_dict(a, arena);
            map.insert(
                Key::String("arg".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(a_dict, Span::origin()))),
            );
            Value::Dict(map)
        }
        Type::Operator(name) => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("operator"),
                    Span::origin(),
                ))),
            );
            map.insert(
                Key::String("name".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val(name),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
        Type::TypeStageApp { fn_name, args } => {
            let mut map = IndexMap::new();
            map.insert(
                Key::String("kind".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val("type-stage-app"),
                    Span::origin(),
                ))),
            );
            map.insert(
                Key::String("fn".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    string_val(fn_name),
                    Span::origin(),
                ))),
            );
            let mut args_map = IndexMap::new();
            for (i, arg_ty) in args.iter().enumerate() {
                let arg_dict = type_to_dict(arg_ty, arena);
                args_map.insert(
                    Key::Int(i as i64),
                    arena.alloc(Rc::new(Thunk::new_materialized(arg_dict, Span::origin()))),
                );
            }
            map.insert(
                Key::String("args".to_string()),
                arena.alloc(Rc::new(Thunk::new_materialized(
                    Value::Dict(args_map),
                    Span::origin(),
                ))),
            );
            Value::Dict(map)
        }
    }
}

/// Convert a runtime dict Value to a Type.
///
/// This is the inverse of `type_to_dict`. The dict must have a `kind:` field
/// that determines which Type variant to construct.
///
/// # Arguments
///
/// * `val` - The Value to convert (must be Value::Dict)
/// * `arena` - ThunkArena for accessing dict entry thunks
/// * `span` - Source span for error reporting
///
/// # Returns
///
/// `Ok(Type)` if conversion succeeds, `Err(TypeError)` otherwise.
///
/// # Errors
///
/// Returns a `TypeError` if:
/// - The value is not a dict
/// - The dict has no `kind:` field
/// - The `kind:` field has an unknown value
/// - Required fields for a kind are missing or malformed
pub fn dict_to_type(val: &Value, arena: &ThunkArena, span: Span) -> Result<Type, TypeError> {
    let dict = match val {
        Value::Dict(map) => map,
        _ => {
            return Err(TypeError::new(
                format!("expected dict for type conversion, got {}", val.type_name()),
                span,
            ))
        }
    };

    // Extract the "kind" field
    let kind_thunk_id = dict
        .get(&Key::String("kind".to_string()))
        .ok_or_else(|| TypeError::new("type dict missing 'kind' field".to_string(), span))?;

    let kind_val = get_value_from_id(arena, *kind_thunk_id, span)?;
    let kind_str = match &kind_val {
        Value::String { source, start, end } => &source[*start..*end],
        _ => {
            return Err(TypeError::new(
                format!("kind field must be a string, got {}", kind_val.type_name()),
                span,
            ))
        }
    };

    match kind_str {
        "named" => {
            let name_thunk_id = dict.get(&Key::String("name".to_string())).ok_or_else(|| {
                TypeError::new("named type missing 'name' field".to_string(), span)
            })?;
            let name_val = get_value_from_id(arena, *name_thunk_id, span)?;
            let name_str = match &name_val {
                Value::String { source, start, end } => &source[*start..*end],
                _ => {
                    return Err(TypeError::new(
                        "name field must be a string".to_string(),
                        span,
                    ))
                }
            };

            match name_str {
                "Int" => Ok(Type::Int),
                "Float" => Ok(Type::Float),
                "String" => Ok(Type::Str),
                "Bool" => Ok(Type::Bool),
                "Bytes" => Ok(Type::Bytes),
                "Number" => Ok(Type::Number),
                "Unknown" => Ok(Type::Unknown),
                "Proxy" => Ok(Type::Proxy),
                "DirCap" => Ok(Type::DirCap),
                "NetCap" => Ok(Type::NetCap),
                "Handle" => Ok(Type::Handle),
                "Uri" => Ok(Type::Uri),
                "Timestamp" => Ok(Type::Timestamp),
                "Duration" => Ok(Type::Duration),
                "ClockCap" => Ok(Type::ClockCap),
                "Timezone" => Ok(Type::Timezone),
                "QuicSession" => Ok(Type::QuicSession),
                "Http2Session" => Ok(Type::Http2Session),
                "Http3Session" => Ok(Type::Http3Session),
                "QuicDatagramHandle" => Ok(Type::QuicDatagramHandle),
                "DatagramHandle" => Ok(Type::DatagramHandle),
                _ => Err(TypeError::new(
                    format!("unknown named type: {}", name_str),
                    span,
                )),
            }
        }
        "int-literal" => {
            let value_thunk_id = dict.get(&Key::String("value".to_string())).ok_or_else(|| {
                TypeError::new("int-literal type missing 'value' field".to_string(), span)
            })?;
            let value_val = get_value_from_id(arena, *value_thunk_id, span)?;
            match value_val {
                Value::Int(n) => Ok(Type::IntLiteral(n)),
                _ => Err(TypeError::new(
                    "int-literal value must be an Int".to_string(),
                    span,
                )),
            }
        }
        "string-literal" => {
            let value_thunk_id = dict.get(&Key::String("value".to_string())).ok_or_else(|| {
                TypeError::new(
                    "string-literal type missing 'value' field".to_string(),
                    span,
                )
            })?;
            let value_val = get_value_from_id(arena, *value_thunk_id, span)?;
            match &value_val {
                Value::String { source, start, end } => {
                    Ok(Type::StringLiteral(source[*start..*end].to_string()))
                }
                _ => Err(TypeError::new(
                    "string-literal value must be a String".to_string(),
                    span,
                )),
            }
        }
        "seq" => {
            let elem_thunk_id = dict
                .get(&Key::String("elem".to_string()))
                .ok_or_else(|| TypeError::new("seq type missing 'elem' field".to_string(), span))?;
            let elem_val = get_value_from_id(arena, *elem_thunk_id, span)?;
            let elem_ty = dict_to_type(&elem_val, arena, span)?;
            Ok(Type::Seq(Box::new(elem_ty)))
        }
        "map" => {
            let key_thunk_id = dict
                .get(&Key::String("key".to_string()))
                .ok_or_else(|| TypeError::new("map type missing 'key' field".to_string(), span))?;
            let key_val = get_value_from_id(arena, *key_thunk_id, span)?;

            let value_thunk_id = dict.get(&Key::String("value".to_string())).ok_or_else(|| {
                TypeError::new("map type missing 'value' field".to_string(), span)
            })?;
            let value_val = get_value_from_id(arena, *value_thunk_id, span)?;

            let key_ty = dict_to_type(&key_val, arena, span)?;
            let value_ty = dict_to_type(&value_val, arena, span)?;
            Ok(Type::Map(Box::new(key_ty), Box::new(value_ty)))
        }
        "record" => {
            let fields_thunk_id =
                dict.get(&Key::String("fields".to_string()))
                    .ok_or_else(|| {
                        TypeError::new("record type missing 'fields' field".to_string(), span)
                    })?;
            let fields_val = get_value_from_id(arena, *fields_thunk_id, span)?;

            let fields_dict = match &fields_val {
                Value::Dict(map) => map,
                _ => {
                    return Err(TypeError::new(
                        "record fields must be a dict".to_string(),
                        span,
                    ))
                }
            };

            let mut fields = std::collections::HashMap::new();
            for (key, thunk_id) in fields_dict {
                let field_name = match key {
                    Key::String(s) => s.clone(),
                    Key::Int(_) => {
                        return Err(TypeError::new(
                            "record field keys must be strings".to_string(),
                            span,
                        ))
                    }
                };
                let field_val = get_value_from_id(arena, *thunk_id, span)?;
                let field_ty = dict_to_type(&field_val, arena, span)?;
                fields.insert(field_name, field_ty);
            }

            Ok(Type::Record(Row { fields }))
        }
        "fn" => {
            let params_thunk_id =
                dict.get(&Key::String("params".to_string()))
                    .ok_or_else(|| {
                        TypeError::new("fn type missing 'params' field".to_string(), span)
                    })?;
            let params_val = get_value_from_id(arena, *params_thunk_id, span)?;

            let params_dict = match &params_val {
                Value::Dict(map) => map,
                _ => return Err(TypeError::new("fn params must be a dict".to_string(), span)),
            };

            let mut params = Vec::new();
            // Expect sequential int keys 0, 1, 2, ...
            for i in 0..params_dict.len() {
                let param_thunk_id = params_dict.get(&Key::Int(i as i64)).ok_or_else(|| {
                    TypeError::new(format!("fn params missing index {}", i), span)
                })?;
                let param_val = get_value_from_id(arena, *param_thunk_id, span)?;

                // Check if param is a dict with type/name fields or just a type dict
                let (param_name, param_ty) = match &param_val {
                    Value::Dict(param_dict) => {
                        // Check if it has both 'type' and 'name' fields (named param)
                        if let (Some(type_thunk_id), Some(name_thunk_id)) = (
                            param_dict.get(&Key::String("type".to_string())),
                            param_dict.get(&Key::String("name".to_string())),
                        ) {
                            let type_val = get_value_from_id(arena, *type_thunk_id, span)?;
                            let name_val = get_value_from_id(arena, *name_thunk_id, span)?;
                            let name_str = match &name_val {
                                Value::String { source, start, end } => {
                                    Some(source[*start..*end].to_string())
                                }
                                _ => {
                                    return Err(TypeError::new(
                                        "param name must be a string".to_string(),
                                        span,
                                    ))
                                }
                            };
                            let ty = dict_to_type(&type_val, arena, span)?;
                            (name_str, ty)
                        } else {
                            // No name field, treat as unnamed param (whole dict is the type)
                            let ty = dict_to_type(&param_val, arena, span)?;
                            (None, ty)
                        }
                    }
                    _ => {
                        // Not a dict, shouldn't happen but handle gracefully
                        let ty = dict_to_type(&param_val, arena, span)?;
                        (None, ty)
                    }
                };
                params.push((param_name, param_ty));
            }

            let ret_thunk_id = dict
                .get(&Key::String("ret".to_string()))
                .ok_or_else(|| TypeError::new("fn type missing 'ret' field".to_string(), span))?;
            let ret_val = get_value_from_id(arena, *ret_thunk_id, span)?;
            let ret_ty = dict_to_type(&ret_val, arena, span)?;

            let variadic =
                if let Some(variadic_thunk_id) = dict.get(&Key::String("variadic".to_string())) {
                    let variadic_val = get_value_from_id(arena, *variadic_thunk_id, span)?;
                    match variadic_val {
                        Value::Bool(b) => b,
                        _ => {
                            return Err(TypeError::new(
                                "variadic field must be a Bool".to_string(),
                                span,
                            ))
                        }
                    }
                } else {
                    false
                };

            Ok(Type::Function {
                params,
                ret: Box::new(ret_ty),
                variadic,
            })
        }
        "union" => {
            let members_thunk_id =
                dict.get(&Key::String("members".to_string()))
                    .ok_or_else(|| {
                        TypeError::new("union type missing 'members' field".to_string(), span)
                    })?;
            let members_val = get_value_from_id(arena, *members_thunk_id, span)?;

            let members_dict = match &members_val {
                Value::Dict(map) => map,
                _ => {
                    return Err(TypeError::new(
                        "union members must be a dict".to_string(),
                        span,
                    ))
                }
            };

            let mut members = Vec::new();
            for i in 0..members_dict.len() {
                let member_thunk_id = members_dict.get(&Key::Int(i as i64)).ok_or_else(|| {
                    TypeError::new(format!("union members missing index {}", i), span)
                })?;
                let member_val = get_value_from_id(arena, *member_thunk_id, span)?;
                let member_ty = dict_to_type(&member_val, arena, span)?;
                members.push(member_ty);
            }

            Ok(Type::normalize_union(members))
        }
        "intersection" => {
            let members_thunk_id =
                dict.get(&Key::String("members".to_string()))
                    .ok_or_else(|| {
                        TypeError::new(
                            "intersection type missing 'members' field".to_string(),
                            span,
                        )
                    })?;
            let members_val = get_value_from_id(arena, *members_thunk_id, span)?;

            let members_dict = match &members_val {
                Value::Dict(map) => map,
                _ => {
                    return Err(TypeError::new(
                        "intersection members must be a dict".to_string(),
                        span,
                    ))
                }
            };

            let mut members = Vec::new();
            for i in 0..members_dict.len() {
                let member_thunk_id = members_dict.get(&Key::Int(i as i64)).ok_or_else(|| {
                    TypeError::new(format!("intersection members missing index {}", i), span)
                })?;
                let member_val = get_value_from_id(arena, *member_thunk_id, span)?;
                let member_ty = dict_to_type(&member_val, arena, span)?;
                members.push(member_ty);
            }

            Ok(Type::normalize_intersection(members))
        }
        "negation" => {
            let inner_thunk_id = dict.get(&Key::String("inner".to_string())).ok_or_else(|| {
                TypeError::new("negation type missing 'inner' field".to_string(), span)
            })?;
            let inner_val = get_value_from_id(arena, *inner_thunk_id, span)?;
            let inner_ty = dict_to_type(&inner_val, arena, span)?;
            Ok(Type::Negation(Box::new(inner_ty)))
        }
        "top" => Ok(Type::Top),
        "never" => Ok(Type::Never),
        "error" => Ok(Type::Error),
        "typevar" => {
            let name_thunk_id = dict.get(&Key::String("name".to_string())).ok_or_else(|| {
                TypeError::new("typevar type missing 'name' field".to_string(), span)
            })?;
            let name_val = get_value_from_id(arena, *name_thunk_id, span)?;

            let name_str = match &name_val {
                Value::String { source, start, end } => source[*start..*end].to_string(),
                _ => {
                    return Err(TypeError::new(
                        "typevar name must be a string".to_string(),
                        span,
                    ))
                }
            };

            // TypeVars created from dicts default to level 0
            Ok(Type::TypeVar(name_str, 0))
        }
        "app" => {
            let func_thunk_id = dict
                .get(&Key::String("func".to_string()))
                .ok_or_else(|| TypeError::new("app type missing 'func' field".to_string(), span))?;
            let func_val = get_value_from_id(arena, *func_thunk_id, span)?;

            let arg_thunk_id = dict
                .get(&Key::String("arg".to_string()))
                .ok_or_else(|| TypeError::new("app type missing 'arg' field".to_string(), span))?;
            let arg_val = get_value_from_id(arena, *arg_thunk_id, span)?;

            let func_ty = dict_to_type(&func_val, arena, span)?;
            let arg_ty = dict_to_type(&arg_val, arena, span)?;

            // Normalize builtin constructors after construction
            match (&func_ty, &arg_ty) {
                // App(Operator("Seq"), T) → Type::Seq(Box::new(T))
                (Type::Operator(name), _) if name == "Seq" => Ok(Type::Seq(Box::new(arg_ty))),
                // App(App(Operator("Map"), K), V) → Type::Map(Box::new(K), Box::new(V))
                (Type::App(inner_func, key_ty), _) => {
                    if let Type::Operator(name) = &**inner_func {
                        if name == "Map" {
                            return Ok(Type::Map(Box::new((**key_ty).clone()), Box::new(arg_ty)));
                        }
                    }
                    Ok(Type::App(Box::new(func_ty), Box::new(arg_ty)))
                }
                _ => Ok(Type::App(Box::new(func_ty), Box::new(arg_ty))),
            }
        }
        "operator" => {
            let name_thunk_id = dict.get(&Key::String("name".to_string())).ok_or_else(|| {
                TypeError::new("operator type missing 'name' field".to_string(), span)
            })?;
            let name_val = get_value_from_id(arena, *name_thunk_id, span)?;

            let name_str = match &name_val {
                Value::String { source, start, end } => source[*start..*end].to_string(),
                _ => {
                    return Err(TypeError::new(
                        "operator name must be a string".to_string(),
                        span,
                    ))
                }
            };

            Ok(Type::Operator(name_str))
        }
        "type-stage-app" => {
            let fn_thunk_id = dict.get(&Key::String("fn".to_string())).ok_or_else(|| {
                TypeError::new("type-stage-app type missing 'fn' field".to_string(), span)
            })?;
            let fn_val = get_value_from_id(arena, *fn_thunk_id, span)?;

            let fn_name = match &fn_val {
                Value::String { source, start, end } => source[*start..*end].to_string(),
                _ => {
                    return Err(TypeError::new(
                        "type-stage-app fn must be a string".to_string(),
                        span,
                    ))
                }
            };

            let args_thunk_id = dict.get(&Key::String("args".to_string())).ok_or_else(|| {
                TypeError::new("type-stage-app type missing 'args' field".to_string(), span)
            })?;
            let args_val = get_value_from_id(arena, *args_thunk_id, span)?;

            let args_dict = match &args_val {
                Value::Dict(map) => map,
                _ => {
                    return Err(TypeError::new(
                        "type-stage-app args must be a dict".to_string(),
                        span,
                    ))
                }
            };

            let mut args = Vec::new();
            for i in 0..args_dict.len() {
                let arg_thunk_id = args_dict.get(&Key::Int(i as i64)).ok_or_else(|| {
                    TypeError::new(format!("type-stage-app args missing index {}", i), span)
                })?;
                let arg_val = get_value_from_id(arena, *arg_thunk_id, span)?;
                let arg_ty = dict_to_type(&arg_val, arena, span)?;
                args.push(arg_ty);
            }

            Ok(Type::TypeStageApp { fn_name, args })
        }
        _ => Err(TypeError::new(
            format!("unknown type kind: {}", kind_str),
            span,
        )),
    }
}

// Helper to create a named type dict
fn make_named_dict(name: &str, arena: &mut ThunkArena) -> Value {
    let mut map = IndexMap::new();
    map.insert(
        Key::String("kind".to_string()),
        arena.alloc(Rc::new(Thunk::new_materialized(
            string_val("named"),
            Span::origin(),
        ))),
    );
    map.insert(
        Key::String("name".to_string()),
        arena.alloc(Rc::new(Thunk::new_materialized(
            string_val(name),
            Span::origin(),
        ))),
    );
    Value::Dict(map)
}

// Helper to create a String Value
fn string_val(s: &str) -> Value {
    Value::String {
        source: Rc::from(s),
        start: 0,
        end: s.len(),
    }
}

// Helper to extract a materialized value from a thunk
fn get_materialized(thunk: &Rc<Thunk>, span: Span) -> Result<Value, TypeError> {
    match &*thunk.state() {
        ThunkState::Materialized(v) => Ok(v.clone()),
        _ => Err(TypeError::new("thunk not materialized".to_string(), span)),
    }
}

// Helper to get a materialized value from a ThunkId via arena
fn get_value_from_id(arena: &ThunkArena, id: ThunkId, span: Span) -> Result<Value, TypeError> {
    let thunk = arena.get(id);
    get_materialized(thunk, span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_arena() -> ThunkArena {
        ThunkArena::new()
    }

    #[test]
    fn test_int_round_trip() {
        let mut arena = test_arena();
        let ty = Type::Int;
        let dict = type_to_dict(&ty, &mut arena);
        let result = dict_to_type(&dict, &arena, Span::origin()).unwrap();
        assert_eq!(result, ty);
    }

    #[test]
    fn test_seq_int_round_trip() {
        let mut arena = test_arena();
        let ty = Type::Seq(Box::new(Type::Int));
        let dict = type_to_dict(&ty, &mut arena);
        let result = dict_to_type(&dict, &arena, Span::origin()).unwrap();
        assert_eq!(result, ty);
    }

    #[test]
    fn test_union_int_str_round_trip() {
        let mut arena = test_arena();
        let ty = Type::Union(vec![Type::Int, Type::Str]);
        let dict = type_to_dict(&ty, &mut arena);
        let result = dict_to_type(&dict, &arena, Span::origin()).unwrap();
        // normalize_union sorts and deduplicates, so we compare normalized forms
        assert_eq!(result, Type::normalize_union(vec![Type::Int, Type::Str]));
    }

    #[test]
    fn test_record_round_trip() {
        let mut arena = test_arena();
        let mut fields = HashMap::new();
        fields.insert("host".to_string(), Type::Str);
        fields.insert("port".to_string(), Type::Int);
        let ty = Type::Record(Row {
            fields: fields.clone(),
        });
        let dict = type_to_dict(&ty, &mut arena);
        let result = dict_to_type(&dict, &arena, Span::origin()).unwrap();
        assert_eq!(result, ty);
    }

    #[test]
    fn test_function_round_trip() {
        let mut arena = test_arena();
        let ty = Type::Function {
            params: vec![(None, Type::Int), (None, Type::Str)],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let dict = type_to_dict(&ty, &mut arena);
        let result = dict_to_type(&dict, &arena, Span::origin()).unwrap();
        assert_eq!(result, ty);
    }

    #[test]
    fn test_map_round_trip() {
        let mut arena = test_arena();
        let ty = Type::Map(Box::new(Type::Str), Box::new(Type::Int));
        let dict = type_to_dict(&ty, &mut arena);
        let result = dict_to_type(&dict, &arena, Span::origin()).unwrap();
        assert_eq!(result, ty);
    }
}
