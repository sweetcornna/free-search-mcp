//! Compile-time `jaq` syntax & symbol checker shared between `build.rs` and runtime
//! validation (`fsm-manifests` does not execute filters; `fsm-sources` binds the real
//! 13-function prelude implementations).

use fsm_core::manifest::ManifestDecl;
use jaq_core::box_iter::box_once;
use jaq_core::load::{Arena, File, Loader};
use jaq_core::native::{Fun, run, v};
use jaq_core::{Compiler, Cv, DataT, ValXs, data};
use jaq_json::Val;

/// Global variables injected into every manifest `jaq` expression.
pub const JAQ_GLOBAL_VARS: [&str; 2] = ["$p", "$cfg"];

/// Names and arities of all 13 normative prelude functions (A1 §C).
pub const PRELUDE_SIGNATURES: [(&str, usize); 13] = [
    ("arr", 0),
    ("first_str", 0),
    ("xml_text", 0),
    ("unescape_html", 0),
    ("strip_tags", 0),
    ("collapse_ws", 0),
    ("epoch_to_iso", 0),
    ("parse_date", 0),
    ("to_num", 0),
    ("to_int", 0),
    ("doi_norm", 0),
    ("openalex_abstract", 0),
    ("resolve_url", 1),
];

fn stub_run<'a, D: DataT>(cv: Cv<'a, D>) -> ValXs<'a, D::V<'a>> {
    box_once(Ok(cv.1))
}

fn prelude_stub_funs<D: DataT>() -> impl Iterator<Item = Fun<D>>
where
    for<'a> D::V<'a>: jaq_std::ValT,
{
    PRELUDE_SIGNATURES
        .into_iter()
        .map(|(name, arity)| run((name, v(arity), stub_run::<D>)))
}

const BLOCKED_STD_FUNS: &[&str] = &[
    "env",
    "unescape_html",
    "halt",
    "stderr_empty",
    "debug_empty",
];

const BLOCKED_STD_DEFS: &[&str] = &["halt", "halt_error", "stderr", "debug"];

/// Compile a single `jaq` filter against the manifest prelude and global variables (`$p`, `$cfg`).
///
/// Returns `Err(description)` on lex, parse, or undefined-symbol errors.
pub fn check_jaq_expr(expr: &str) -> Result<(), String> {
    let defs = jaq_core::defs()
        .chain(jaq_std::defs().filter(|d| !BLOCKED_STD_DEFS.contains(&d.name)))
        .chain(jaq_json::defs());
    let funs = prelude_stub_funs::<data::JustLut<Val>>()
        .chain(jaq_core::funs())
        .chain(jaq_std::funs().filter(|(name, _, _)| !BLOCKED_STD_FUNS.contains(name)))
        .chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();
    let program = File {
        code: expr,
        path: (),
    };
    let modules = loader
        .load(&arena, program)
        .map_err(|errs| format_load_errors(&errs))?;

    Compiler::default()
        .with_global_vars(JAQ_GLOBAL_VARS)
        .with_funs(funs)
        .compile(modules)
        .map(|_| ())
        .map_err(|errs| format_compile_errors(&errs))
}

/// Collect every `(location, resolved_expr)` `jaq` filter in a manifest and verify
/// that every single one parses and compiles cleanly.
pub fn check_manifest_jaq(manifest: &ManifestDecl) -> Vec<String> {
    let mut problems = Vec::new();
    for (op_name, op) in manifest.op.iter() {
        let mut check = |label: String, expr: &str| {
            let resolved = match manifest.resolve_ref(expr) {
                Ok(r) => r,
                Err(e) => {
                    problems.push(format!("{label}: {e}"));
                    return;
                }
            };
            if let Err(e) = check_jaq_expr(resolved) {
                problems.push(format!("{label}: {e}"));
            }
        };

        if let Some(qj) = &op.query_jaq {
            check(format!("op.{op_name}.query_jaq"), qj);
        }
        if let Some(body) = &op.body {
            if let Some(j) = &body.jaq {
                check(format!("op.{op_name}.body.jaq"), j);
            }
            if let Some(vj) = &body.variables_jaq {
                check(format!("op.{op_name}.body.variables_jaq"), vj);
            }
        }
        for (i, pre) in op.pre.iter().enumerate() {
            check(format!("op.{op_name}.pre[{i}].bind"), &pre.bind);
            if let Some(body) = &pre.body {
                if let Some(j) = &body.jaq {
                    check(format!("op.{op_name}.pre[{i}].body.jaq"), j);
                }
                if let Some(vj) = &body.variables_jaq {
                    check(format!("op.{op_name}.pre[{i}].body.variables_jaq"), vj);
                }
            }
        }
        if let Some(map) = &op.map {
            check(format!("op.{op_name}.map.items"), &map.items);
            if let Some(e) = &map.next_cursor {
                check(format!("op.{op_name}.map.next_cursor"), e);
            }
            if let Some(e) = &map.total {
                check(format!("op.{op_name}.map.total"), e);
            }
            if let Some(e) = &map.error {
                check(format!("op.{op_name}.map.error"), e);
            }
            if let Some(e) = &map.work {
                check(format!("op.{op_name}.map.work"), e);
            }
            if let Some(e) = &map.record {
                check(format!("op.{op_name}.map.record"), e);
            }
            if let Some(e) = &map.hit {
                check(format!("op.{op_name}.map.hit"), e);
            }
        }
    }
    problems
}

fn format_load_errors<P>(errs: &jaq_core::load::Errors<&str, P>) -> String {
    let mut parts = Vec::new();
    for (_file, err) in errs {
        match err {
            jaq_core::load::Error::Io(io_errs) => {
                for (path, msg) in io_errs {
                    parts.push(format!("import error ({path}): {msg}"));
                }
            }
            jaq_core::load::Error::Lex(lex_errs) => {
                for (expected, found) in lex_errs {
                    parts.push(format!("lex error: expected {expected:?}, found {found:?}"));
                }
            }
            jaq_core::load::Error::Parse(parse_errs) => {
                for (expected, found) in parse_errs {
                    parts.push(format!(
                        "parse error: expected {expected:?}, found {found:?}"
                    ));
                }
            }
        }
    }
    parts.join("; ")
}

fn format_compile_errors<P>(errs: &jaq_core::compile::Errors<&str, P>) -> String {
    let mut parts = Vec::new();
    for (_file, comp_errs) in errs {
        for (sym, undef) in comp_errs {
            parts.push(format!("undefined {} {sym:?}", undef.as_str()));
        }
    }
    parts.join("; ")
}
