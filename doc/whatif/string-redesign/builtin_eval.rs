// doc/whatif/string-redesign/builtin_eval.rs
//
// Proposed changes to builtin-eval's type registration.
// This file is a design stub — not compiled, not exhaustive.
// Relevant current file: src/imports.rs
//
// The core problem: builtin-eval currently registers ret: Type::Unknown.
// Type::Unknown propagates via the consistency relation ~ and disables type
// checking downstream (eval-document-runtime → include return Unknown).
//
// The fix must NOT add a special-case code path in the type checker.
// builtin-eval should be a regular polymorphic function — same type structure
// as if it were declared as a tinct function in prelude:
//
//   builtin-eval: [fn@T [let exprs@[Seq Expression[T]]] ...]
//
// This requires Expression to be parameterized by the result type T.
// See: Expression[T] — an expression that evaluates to a value of type T.
// The type checker instantiates T fresh at each call site and unifies it
// with the actual element type of the exprs argument, exactly as for any
// polymorphic tinct function. No special-case intercept needed.

// ─── Prerequisite: parameterised Expression type ──────────────────────────────
//
// Expression is currently declared in prelude.llt as:
//   Expression: [type [let T] ...]   — parameterised by result type T
//
// Expression[Int] is an expression that evaluates to an Int.
// Expression[Dict] is an expression that evaluates to a Dict.
// [Seq Expression[T]] is the type of a homogeneous sequence of T-returning expressions.
//
// This is already sound: the type checker knows the result type of every
// expression it processes. Expression[T] simply makes that information
// explicit in the type of the expression AST value.

// ─── src/imports.rs — builtin-eval type registration ─────────────────────────
//
// Registered as a polymorphic function. The type structure is identical to what
// the type checker would produce for a tinct function declaration with the same
// signature. No special case in the type checker — normal TypeVar instantiation
// and unification at every call site.

fn register_builtin_eval(env: &mut TypeEnv) {
    // forall T. (Seq Expression[T]) -> T
    // Same structure as any polymorphic tinct function type scheme.
    let t = fresh_type_var("T");
    let expr_seq = Type::App(
        Box::new(Type::TyCon("Seq".into())),
        Box::new(Type::App(
            Box::new(Type::TyCon("Expression".into())),
            Box::new(t.clone()),
        )),
    );
    env.insert_scheme(
        "builtin-eval",
        TypeScheme {
            vars: vec![extract_var_id(&t)], // generalised over T
            ty: Type::Function {
                params: vec![(None, expr_seq)], // exprs: [Seq Expression[T]]
                ret:    Box::new(t),            // T
                variadic: true,                 // named args: scope:, %:, program:, expects:
                required_count: 1,
            },
        },
    );
}

// ─── How this eliminates the Unknown cascade ──────────────────────────────────
//
// At a call site like:
//   [builtin-eval doc.expressions scope: env %: prev ...]
//
// 1. The type checker instantiates T to a fresh TypeVar _t0
// 2. It unifies _t0 with the element type of doc.expressions
// 3. doc.expressions: [Seq Expression[T_doc]] where T_doc was inferred when
//    the type checker processed the document's source code
// 4. _t0 unifies with T_doc — builtin-eval returns T_doc at this call site
// 5. eval-document-runtime returns Dict containing percent: T_doc
// 6. eval-document-pipeline extracts percent → T_doc
// 7. eval-file → T_doc, include → T_doc
//
// No special case. Same path as any other polymorphic function call.
// The type flows from the expressions — the same way % propagates and
// the same way function return types are inferred from bodies.
//
// BEFORE (current): ret: Type::Unknown  → disables type checking downstream
// AFTER (proposed): ret: TypeVar T      → inferred correctly at every call site
