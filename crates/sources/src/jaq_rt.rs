//! `jaq 3.1` compilation and execution runtime with all 13 normative A1 §C prelude
//! functions (`arr`, `first_str`, `xml_text`, `unescape_html`, `strip_tags`,
//! `collapse_ws`, `epoch_to_iso`, `parse_date`, `to_num`, `to_int`, `doi_norm`,
//! `openalex_abstract`, `resolve_url`).

use fsm_core::FsmError;
use fsm_core::manifest::ManifestDecl;
use jaq_core::box_iter::box_once;
use jaq_core::load::{Arena, File, Loader};
use jaq_core::native::{Fun, run, unary, v};
use jaq_core::{Compiler, Ctx, Filter, ValT as _, Vars, data};
use jaq_json::{Map as JaqMap, Num, Val};
use jiff::Timestamp;
use jiff::civil::Date;
use regex::Regex;
use serde_json::{Map as JsonMap, Number as JsonNumber, Value};
use std::sync::LazyLock;
use url::Url;

/// Maximum items emitted by any single `jaq` filter evaluation (SPEC.md §4).
pub const MAX_JAQ_STREAM_ITEMS: usize = 200;

/// Global variables injected into every manifest `jaq` expression.
pub const JAQ_GLOBAL_VARS: [&str; 2] = ["$p", "$cfg"];

type JaqData = data::JustLut<Val>;

/// A compiled `jaq` filter bound to `$p` and `$cfg`.
pub struct CompiledFilter {
    filter: Filter<JaqData>,
}

// Compile-time proof that CompiledFilter can live inside OnceLock/Arc across async tasks.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CompiledFilter>();
};

impl std::fmt::Debug for CompiledFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledFilter").finish_non_exhaustive()
    }
}

/// Blocked `jaq-std` native functions (process exit, env read, raw stderr/debug, 5-entity unescape).
const BLOCKED_STD_FUNS: &[&str] = &[
    "env",
    "unescape_html",
    "halt",
    "stderr_empty",
    "debug_empty",
];

/// Blocked `jaq-std` jq defs (wrappers over halt/stderr/debug).
const BLOCKED_STD_DEFS: &[&str] = &["halt", "halt_error", "stderr", "debug"];

/// Blocked `jaq-core` jq defs for untrusted user manifests (loop/recursion/combinatorial DoS).
const BLOCKED_USER_CORE_DEFS: &[&str] = &[
    "while",
    "until",
    "repeat",
    "recurse",
    "walk",
    "combinations",
    "paths",
];

/// Compile a `jaq` expression with the full 13-function prelude and `$p`/`$cfg` globals.
///
/// Process environment inspection (`env`), process termination (`halt`/`halt_error`), and
/// raw stderr/debug output (`stderr`/`debug`) are stripped from `jaq-std`; our A1 §C
/// `unescape_html` overrides `jaq-std`'s 5-entity version.
pub fn compile_filter(expr: &str) -> Result<CompiledFilter, FsmError> {
    compile_filter_inner(expr, false)
}

/// Compile an untrusted user-manifest `jaq` expression: runs static DoS linting and
/// additionally strips unbounded loop/recursion defs (`while`, `until`, `repeat`,
/// `recurse`, `walk`, `combinations`, `paths`) from `jaq-core`.
pub fn compile_user_filter(expr: &str) -> Result<CompiledFilter, FsmError> {
    fsm_manifests::user_overrides::lint_user_jaq(expr).map_err(FsmError::invalid)?;
    compile_filter_inner(expr, true)
}

fn compile_filter_inner(expr: &str, restrict_user: bool) -> Result<CompiledFilter, FsmError> {
    let core_defs = jaq_core::defs()
        .filter(move |d| !restrict_user || !BLOCKED_USER_CORE_DEFS.contains(&d.name));
    let std_defs = jaq_std::defs().filter(|d| !BLOCKED_STD_DEFS.contains(&d.name));
    let defs = core_defs.chain(std_defs).chain(jaq_json::defs());
    let funs = prelude_funs()
        .chain(jaq_core::funs())
        .chain(jaq_std::funs().filter(|(name, _, _)| !BLOCKED_STD_FUNS.contains(name)))
        .chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();
    let program = File {
        code: expr,
        path: (),
    };
    let modules = loader.load(&arena, program).map_err(|errs| {
        FsmError::invalid(format!("jaq syntax error: {}", format_load_errs(&errs)))
    })?;

    let filter = Compiler::default()
        .with_global_vars(JAQ_GLOBAL_VARS)
        .with_funs(funs)
        .compile(modules)
        .map_err(|errs| {
            FsmError::invalid(format!("jaq compile error: {}", format_compile_errs(&errs)))
        })?;

    Ok(CompiledFilter { filter })
}

impl CompiledFilter {
    /// Evaluate the filter as a stream of up to `max_items` JSON values.
    ///
    /// Never calls `jaq_core::unwrap_valr` (which invokes `std::process::exit` on `halt`);
    /// any halt exception is converted into `FsmError::InvalidInput`.
    pub fn eval_stream(
        &self,
        input: &Value,
        p: &Value,
        cfg: &Value,
        max_items: usize,
    ) -> Result<Vec<Value>, FsmError> {
        let vars = Vars::new([json_to_val(p), json_to_val(cfg)]);
        let ctx = Ctx::<JaqData>::new(&self.filter.lut, vars);
        let in_val = json_to_val(input);

        let mut out = Vec::new();
        for item in self.filter.id.run((ctx, in_val)).take(max_items) {
            let v = item.map_err(|exn| match exn.get_err() {
                Ok(e) => FsmError::invalid(format!("jaq runtime error: {e}")),
                Err(_) => FsmError::invalid("jaq filter halted"),
            })?;
            out.push(val_to_json(&v));
        }
        Ok(out)
    }

    /// Evaluate the filter and return its first output (if any).
    pub fn eval_one(
        &self,
        input: &Value,
        p: &Value,
        cfg: &Value,
    ) -> Result<Option<Value>, FsmError> {
        let mut items = self.eval_stream(input, p, cfg, 1)?;
        Ok(items.pop())
    }
}

/// Validate that every `jaq` expression in `manifest` compiles with the real prelude.
pub fn validate_manifest_jaq(manifest: &ManifestDecl) -> Vec<String> {
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
            if let Err(e) = compile_filter(resolved) {
                problems.push(format!("{label}: {e}"));
            }
        };

        if let Some(qj) = &op.query_jaq {
            check(format!("op.{op_name}.query_jaq"), qj);
        }
        if let Some(b) = &op.body {
            if let Some(j) = &b.jaq {
                check(format!("op.{op_name}.body.jaq"), j);
            }
            if let Some(vj) = &b.variables_jaq {
                check(format!("op.{op_name}.body.variables_jaq"), vj);
            }
        }
        for (i, pre) in op.pre.iter().enumerate() {
            check(format!("op.{op_name}.pre[{i}].bind"), &pre.bind);
            if let Some(b) = &pre.body {
                if let Some(j) = &b.jaq {
                    check(format!("op.{op_name}.pre[{i}].body.jaq"), j);
                }
                if let Some(vj) = &b.variables_jaq {
                    check(format!("op.{op_name}.pre[{i}].body.variables_jaq"), vj);
                }
            }
        }
        if let Some(m) = &op.map {
            check(format!("op.{op_name}.map.items"), &m.items);
            for (field, val) in [
                ("next_cursor", m.next_cursor.as_deref()),
                ("total", m.total.as_deref()),
                ("error", m.error.as_deref()),
                ("work", m.work.as_deref()),
                ("record", m.record.as_deref()),
                ("hit", m.hit.as_deref()),
            ] {
                if let Some(v) = val {
                    check(format!("op.{op_name}.map.{field}"), v);
                }
            }
        }
    }
    problems
}

// ---------------------------------------------------------------------------
// Value conversion: serde_json::Value <-> jaq_json::Val
// ---------------------------------------------------------------------------

pub fn json_to_val(v: &Value) -> Val {
    match v {
        Value::Null => Val::Null,
        Value::Bool(b) => Val::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Val::Num(Num::from_integral(i))
            } else if let Some(u) = n.as_u64() {
                Val::Num(Num::from_integral(u))
            } else if let Some(f) = n.as_f64() {
                Val::Num(Num::Float(f))
            } else {
                Val::Null
            }
        }
        Value::String(s) => Val::from(s.clone()),
        Value::Array(arr) => arr.iter().map(json_to_val).collect(),
        Value::Object(map) => {
            let mut jm = JaqMap::default();
            for (k, val) in map {
                jm.insert(Val::from(k.clone()), json_to_val(val));
            }
            Val::obj(jm)
        }
    }
}

pub fn val_to_json(v: &Val) -> Value {
    match v {
        Val::Null => Value::Null,
        Val::Bool(b) => Value::Bool(*b),
        Val::Num(n) => match n {
            Num::Int(i) => Value::Number(JsonNumber::from(*i as i64)),
            Num::BigInt(bi) => {
                if let Ok(i) = i64::try_from(bi.as_ref()) {
                    Value::Number(JsonNumber::from(i))
                } else if let Ok(u) = u64::try_from(bi.as_ref()) {
                    Value::Number(JsonNumber::from(u))
                } else {
                    bi.to_string()
                        .parse::<f64>()
                        .ok()
                        .and_then(JsonNumber::from_f64)
                        .map_or(Value::Null, Value::Number)
                }
            }
            Num::Float(f) => JsonNumber::from_f64(*f).map_or(Value::Null, Value::Number),
            Num::Dec(s) => {
                if let Ok(i) = s.parse::<i64>() {
                    Value::Number(JsonNumber::from(i))
                } else if let Ok(f) = s.parse::<f64>() {
                    JsonNumber::from_f64(f).map_or(Value::Null, Value::Number)
                } else {
                    Value::Null
                }
            }
        },
        Val::TStr(b) | Val::BStr(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
        Val::Arr(arr) => Value::Array(arr.iter().map(val_to_json).collect()),
        Val::Obj(map) => {
            let mut out = JsonMap::with_capacity(map.len());
            for (k, val) in map.iter() {
                let key = match k {
                    Val::TStr(b) | Val::BStr(b) => String::from_utf8_lossy(b).into_owned(),
                    other => other.to_string(),
                };
                out.insert(key, val_to_json(val));
            }
            Value::Object(out)
        }
    }
}

// ---------------------------------------------------------------------------
// 13 Normative Prelude Functions (A1 §C)
// ---------------------------------------------------------------------------

fn prelude_funs() -> impl Iterator<Item = Fun<JaqData>> {
    let list: [Fun<JaqData>; 13] = [
        run(("arr", v(0), |cv| box_once(Ok(fn_arr(cv.1))))),
        run(("first_str", v(0), |cv| box_once(Ok(fn_first_str(&cv.1))))),
        run(("xml_text", v(0), |cv| {
            box_once(Ok(Val::from(fn_xml_text(&cv.1))))
        })),
        run(("unescape_html", v(0), |cv| {
            box_once(Ok(match val_as_str(&cv.1) {
                Some(s) => Val::from(unescape_html_str(s)),
                None => Val::Null,
            }))
        })),
        run(("strip_tags", v(0), |cv| {
            box_once(Ok(match val_as_str(&cv.1) {
                Some(s) => Val::from(strip_tags_str(s)),
                None => Val::Null,
            }))
        })),
        run(("collapse_ws", v(0), |cv| {
            box_once(Ok(match val_as_str(&cv.1) {
                Some(s) => Val::from(collapse_ws_str(s)),
                None => Val::Null,
            }))
        })),
        run(("epoch_to_iso", v(0), |cv| {
            box_once(Ok(fn_epoch_to_iso(&cv.1).map_or(Val::Null, Val::from)))
        })),
        run(("parse_date", v(0), |cv| {
            box_once(Ok(fn_parse_date(&cv.1).map_or(Val::Null, Val::from)))
        })),
        run(("to_num", v(0), |cv| box_once(Ok(fn_to_num(&cv.1))))),
        run(("to_int", v(0), |cv| box_once(Ok(fn_to_int(&cv.1))))),
        run(("doi_norm", v(0), |cv| {
            box_once(Ok(match val_as_str(&cv.1) {
                Some(s) => doi_norm_str(s).map_or(Val::Null, Val::from),
                None => Val::Null,
            }))
        })),
        run(("openalex_abstract", v(0), |cv| {
            box_once(Ok(fn_openalex_abstract(&cv.1).map_or(Val::Null, Val::from)))
        })),
        run(("resolve_url", v(1), |cv| {
            unary(cv, |input, base| {
                let res = match (val_as_str(&input), val_as_str(&base)) {
                    (Some(inp), Some(b)) => resolve_url_str(inp, b).map_or(Val::Null, Val::from),
                    _ => Val::Null,
                };
                Ok(res)
            })
        })),
    ];
    list.into_iter()
}

fn val_as_str(v: &Val) -> Option<&str> {
    match v {
        Val::TStr(b) | Val::BStr(b) => std::str::from_utf8(b).ok(),
        _ => None,
    }
}

/// #1 `arr`: null -> [], array -> unchanged, any other value x -> [x].
fn fn_arr(v: Val) -> Val {
    match v {
        Val::Null => Val::from_iter(std::iter::empty::<Val>()),
        arr @ Val::Arr(_) => arr,
        other => Val::from_iter([other]),
    }
}

/// #2 `first_str`: first non-empty trimmed string from string / object(.#text then .value) / array.
fn fn_first_str(v: &Val) -> Val {
    fn inner(v: &Val) -> Option<String> {
        match v {
            Val::TStr(b) | Val::BStr(b) => {
                let s = String::from_utf8_lossy(b);
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_owned())
                }
            }
            Val::Obj(map) => {
                if let Some(t) = map.get(&Val::from("#text".to_owned()))
                    && let Some(s) = inner(t)
                {
                    return Some(s);
                }
                if let Some(val) = map.get(&Val::from("value".to_owned()))
                    && let Some(s) = inner(val)
                {
                    return Some(s);
                }
                None
            }
            Val::Arr(arr) => arr.iter().find_map(inner),
            _ => None,
        }
    }
    inner(v).map_or(Val::Null, Val::from)
}

/// #3 `xml_text`: flattens `roxmltree` XML->JSON text.
fn fn_xml_text(v: &Val) -> String {
    match v {
        Val::TStr(b) | Val::BStr(b) => String::from_utf8_lossy(b).into_owned(),
        Val::Obj(map) => map
            .get(&Val::from("#text".to_owned()))
            .and_then(val_as_str)
            .map_or_else(String::new, str::to_owned),
        Val::Arr(arr) => arr
            .iter()
            .map(fn_xml_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Val::Num(_) | Val::Bool(_) => v
            .clone()
            .into_string()
            .to_string()
            .trim_matches('"')
            .to_owned(),
        Val::Null => String::new(),
    }
}

/// #4 `unescape_html`: named HTML entities + decimal/hex character references.
pub fn unescape_html_str(input: &str) -> String {
    if !input.contains('&') {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            // Look ahead up to 16 bytes for ';'
            let max_end = (i + 16).min(bytes.len());
            if let Some(rel_semi) = bytes[i + 1..max_end].iter().position(|&b| b == b';') {
                let entity = &input[i + 1..i + 1 + rel_semi];
                if let Some(decoded) = decode_html_entity(entity) {
                    out.push_str(&decoded);
                    i += rel_semi + 2;
                    continue;
                }
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn decode_html_entity(entity: &str) -> Option<String> {
    if let Some(num_str) = entity.strip_prefix('#') {
        let cp = if let Some(hex) = num_str
            .strip_prefix('x')
            .or_else(|| num_str.strip_prefix('X'))
        {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num_str.parse::<u32>().ok()?
        };
        if cp == 0 {
            return None;
        }
        return char::from_u32(cp).map(|c| c.to_string());
    }
    let mapped = match entity {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "ndash" => "\u{2013}",
        "mdash" => "\u{2014}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "ldquo" => "\u{201C}",
        "rdquo" => "\u{201D}",
        "hellip" => "\u{2026}",
        "copy" => "\u{00A9}",
        "reg" => "\u{00AE}",
        "trade" => "\u{2122}",
        "times" => "\u{00D7}",
        "divide" => "\u{00F7}",
        "plusmn" => "\u{00B1}",
        "deg" => "\u{00B0}",
        "micro" => "\u{00B5}",
        // Greek lowercase
        "alpha" => "\u{03B1}",
        "beta" => "\u{03B2}",
        "gamma" => "\u{03B3}",
        "delta" => "\u{03B4}",
        "epsilon" => "\u{03B5}",
        "zeta" => "\u{03B6}",
        "eta" => "\u{03B7}",
        "theta" => "\u{03B8}",
        "iota" => "\u{03B9}",
        "kappa" => "\u{03BA}",
        "lambda" => "\u{03BB}",
        "mu" => "\u{03BC}",
        "nu" => "\u{03BD}",
        "xi" => "\u{03BE}",
        "omicron" => "\u{03BF}",
        "pi" => "\u{03C0}",
        "rho" => "\u{03C1}",
        "sigmaf" => "\u{03C2}",
        "sigma" => "\u{03C3}",
        "tau" => "\u{03C4}",
        "upsilon" => "\u{03C5}",
        "phi" => "\u{03C6}",
        "chi" => "\u{03C7}",
        "psi" => "\u{03C8}",
        "omega" => "\u{03C9}",
        // Greek uppercase
        "Alpha" => "\u{0391}",
        "Beta" => "\u{0392}",
        "Gamma" => "\u{0393}",
        "Delta" => "\u{0394}",
        "Epsilon" => "\u{0395}",
        "Zeta" => "\u{0396}",
        "Eta" => "\u{0397}",
        "Theta" => "\u{0398}",
        "Iota" => "\u{0399}",
        "Kappa" => "\u{039A}",
        "Lambda" => "\u{039B}",
        "Mu" => "\u{039C}",
        "Nu" => "\u{039D}",
        "Xi" => "\u{039E}",
        "Omicron" => "\u{039F}",
        "Pi" => "\u{03A0}",
        "Rho" => "\u{03A1}",
        "Sigma" => "\u{03A3}",
        "Tau" => "\u{03A4}",
        "Upsilon" => "\u{03A5}",
        "Phi" => "\u{03A6}",
        "Chi" => "\u{03A7}",
        "Psi" => "\u{03A8}",
        "Omega" => "\u{03A9}",
        // Common extras
        "bull" => "\u{2022}",
        "middot" => "\u{00B7}",
        "laquo" => "\u{00AB}",
        "raquo" => "\u{00BB}",
        "euro" => "\u{20AC}",
        "pound" => "\u{00A3}",
        "yen" => "\u{00A5}",
        "cent" => "\u{00A2}",
        "sect" => "\u{00A7}",
        "para" => "\u{00B6}",
        "frac12" => "\u{00BD}",
        "frac14" => "\u{00BC}",
        "frac34" => "\u{00BE}",
        "le" => "\u{2264}",
        "ge" => "\u{2265}",
        "ne" => "\u{2260}",
        "approx" => "\u{2248}",
        "infin" => "\u{221E}",
        "larr" => "\u{2190}",
        "rarr" => "\u{2192}",
        "uarr" => "\u{2191}",
        "darr" => "\u{2193}",
        "ensp" | "emsp" | "thinsp" => " ",
        "shy" | "zwj" | "zwnj" => "",
        _ => return None,
    };
    Some(mapped.to_owned())
}

/// #5 `strip_tags`: remove `<[^>]*>`, decode HTML entities, collapse whitespace and trim.
pub fn strip_tags_str(input: &str) -> String {
    static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("tag regex"));
    let without_tags = TAG_RE.replace_all(input, " ");
    let decoded = unescape_html_str(&without_tags);
    collapse_ws_str(&decoded)
}

/// #6 `collapse_ws`: collapse Unicode whitespace runs to single ASCII space and trim.
pub fn collapse_ws_str(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_ws = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !out.is_empty() && !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// #7 `epoch_to_iso`: converts Unix seconds or milliseconds to UTC `YYYY-MM-DDTHH:MM:SSZ`.
pub fn epoch_number_to_iso(n: f64) -> Option<String> {
    if !n.is_finite() {
        return None;
    }
    let secs = if n.abs() >= 100_000_000_000.0 {
        (n / 1000.0).trunc()
    } else {
        n.trunc()
    };
    if secs < i64::MIN as f64 || secs >= i64::MAX as f64 {
        return None;
    }
    let ts = Timestamp::from_second(secs as i64).ok()?;
    let dt = jiff::tz::TimeZone::UTC.to_datetime(ts);
    let y = dt.year();
    if !(1000..=9999).contains(&y) {
        return None;
    }
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    ))
}

fn val_as_finite_f64(v: &Val) -> Option<f64> {
    match v {
        Val::Num(n) => {
            let f = match n {
                Num::Int(i) => *i as f64,
                Num::BigInt(bi) => bi.to_string().parse::<f64>().ok()?,
                Num::Float(f) => *f,
                Num::Dec(s) => s.parse::<f64>().ok()?,
            };
            f.is_finite().then_some(f)
        }
        _ => None,
    }
}

fn fn_epoch_to_iso(v: &Val) -> Option<String> {
    if let Some(f) = val_as_finite_f64(v) {
        return epoch_number_to_iso(f);
    }
    if let Some(s) = val_as_str(v) {
        let t = s.trim();
        if t.is_empty() {
            return None;
        }
        if let Ok(i) = t.parse::<i64>() {
            return epoch_i64_to_iso(i);
        }
        if let Ok(f) = t.parse::<f64>() {
            return epoch_number_to_iso(f);
        }
    }
    None
}

fn epoch_i64_to_iso(n: i64) -> Option<String> {
    let secs = if n.unsigned_abs() >= 100_000_000_000 {
        n / 1000
    } else {
        n
    };
    let ts = Timestamp::from_second(secs).ok()?;
    let dt = jiff::tz::TimeZone::UTC.to_datetime(ts);
    let y = dt.year();
    if !(1000..=9999).contains(&y) {
        return None;
    }
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    ))
}

/// #8 `parse_date`: all 9 normative rules in A1 §C #8, preserving precision (`YYYY-MM-DD`, `YYYY-MM`, `YYYY`).
fn fn_parse_date(v: &Val) -> Option<String> {
    // (1) finite number -> epoch_to_iso[0..10]
    if let Some(f) = val_as_finite_f64(v) {
        let iso = epoch_number_to_iso(f)?;
        return validate_ymd_str(&iso[0..10]);
    }
    let raw = val_as_str(v)?;
    parse_date_str(raw)
}

pub fn parse_date_str(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }

    // (2) trimmed numeric string of >= 9 digits (optional leading '-') -> epoch_to_iso[0..10]
    let digits_part = s.strip_prefix('-').unwrap_or(s);
    if digits_part.len() >= 9 && digits_part.bytes().all(|b| b.is_ascii_digit()) {
        let n = s.parse::<i64>().ok()?;
        let iso = epoch_i64_to_iso(n)?;
        return validate_ymd_str(&iso[0..10]);
    }

    // (3) ISO-8601 / RFC 3339 prefix YYYY-MM-DD... (also accepts unpadded YYYY-M-D)
    static RE_ISO_YMD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(\d{4})-(\d{1,2})-(\d{1,2})(?:$|[T\sZ+\-])").expect("regex")
    });
    if let Some(c) = RE_ISO_YMD.captures(s) {
        return fmt_ymd(&c[1], &c[2], &c[3]);
    }

    // (4) slash/dot YYYY/MM/DD or YYYY.MM.DD
    static RE_SLASH_DOT_YMD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(\d{4})[/.](\d{1,2})[/.](\d{1,2})(?:$|[\sT])").expect("regex")
    });
    if let Some(c) = RE_SLASH_DOT_YMD.captures(s) {
        return fmt_ymd(&c[1], &c[2], &c[3]);
    }

    // (5) compact 8-digit YYYYMMDD
    if s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()) {
        return fmt_ymd(&s[0..4], &s[4..6], &s[6..8]);
    }

    // (6) RFC 2822: "Wed, 25 Nov 2026 ..." or "25 Nov 2026 ..."
    static RE_RFC2822: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(?:(?:mon|tue|wed|thu|fri|sat|sun)[a-z]*,?\s+)?(\d{1,2})\s+([a-z]{3,9})\.?\s+(\d{4})(?:$|\s|,)").expect("regex")
    });
    if let Some(c) = RE_RFC2822.captures(s)
        && let Some(m) = parse_english_month(&c[2])
    {
        return fmt_ymd_ints(c[3].parse().ok()?, m, c[1].parse().ok()?);
    }

    // (7a) English "Nov 25, 2026" / "November 25, 2026" / "Nov 25 2026"
    static RE_MDY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^([a-z]{3,9})\.?\s+(\d{1,2}),?\s+(\d{4})(?:$|\s)").expect("regex")
    });
    if let Some(c) = RE_MDY.captures(s)
        && let Some(m) = parse_english_month(&c[1])
    {
        return fmt_ymd_ints(c[3].parse().ok()?, m, c[2].parse().ok()?);
    }

    // (7b) English "2026 Nov 25" / "2026 November 25"
    static RE_Y_MON_D: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(\d{4})\s+([a-z]{3,9})\.?\s+(\d{1,2})(?:$|\s)").expect("regex")
    });
    if let Some(c) = RE_Y_MON_D.captures(s)
        && let Some(m) = parse_english_month(&c[2])
    {
        return fmt_ymd_ints(c[1].parse().ok()?, m, c[3].parse().ok()?);
    }

    // (7c) English "2026 Nov" / "2026 November"
    static RE_Y_MON: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(\d{4})\s+([a-z]{3,9})\.?$").expect("regex"));
    if let Some(c) = RE_Y_MON.captures(s)
        && let Some(m) = parse_english_month(&c[2])
    {
        return fmt_ym_ints(c[1].parse().ok()?, m);
    }

    // (7d) English "Nov 2026" / "November 2026"
    static RE_MON_Y: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^([a-z]{3,9})\.?,?\s+(\d{4})$").expect("regex"));
    if let Some(c) = RE_MON_Y.captures(s)
        && let Some(m) = parse_english_month(&c[1])
    {
        return fmt_ym_ints(c[2].parse().ok()?, m);
    }

    // (8) YYYY-MM or YYYY/MM or YYYY.MM
    static RE_YM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(\d{4})[-/.](\d{1,2})$").expect("regex"));
    if let Some(c) = RE_YM.captures(s) {
        return fmt_ym_ints(c[1].parse().ok()?, c[2].parse().ok()?);
    }

    // (9) 4-digit year YYYY (1000..=2999)
    if s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit()) {
        let y: i16 = s.parse().ok()?;
        if (1000..=2999).contains(&y) {
            return Some(format!("{y:04}"));
        }
    }

    None
}

fn validate_ymd_str(s: &str) -> Option<String> {
    if s.len() != 10 {
        return None;
    }
    fmt_ymd(&s[0..4], &s[5..7], &s[8..10])
}

fn fmt_ymd(y: &str, m: &str, d: &str) -> Option<String> {
    fmt_ymd_ints(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
}

fn fmt_ymd_ints(y: i16, m: i8, d: i8) -> Option<String> {
    if !(1000..=2999).contains(&y) {
        return None;
    }
    let date = Date::new(y, m, d).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month(),
        date.day()
    ))
}

fn fmt_ym_ints(y: i16, m: i8) -> Option<String> {
    if !(1000..=2999).contains(&y) || !(1..=12).contains(&m) {
        return None;
    }
    Some(format!("{y:04}-{m:02}"))
}

fn parse_english_month(s: &str) -> Option<i8> {
    match s.to_ascii_lowercase().as_str() {
        "jan" | "january" => Some(1),
        "feb" | "february" => Some(2),
        "mar" | "march" => Some(3),
        "apr" | "april" => Some(4),
        "may" => Some(5),
        "jun" | "june" => Some(6),
        "jul" | "july" => Some(7),
        "aug" | "august" => Some(8),
        "sep" | "sept" | "september" => Some(9),
        "oct" | "october" => Some(10),
        "nov" | "november" => Some(11),
        "dec" | "december" => Some(12),
        _ => None,
    }
}

/// #9 `to_num`: safe numeric conversion returning `null` instead of jq `empty`.
fn fn_to_num(v: &Val) -> Val {
    match v {
        Val::Num(n) => {
            if val_as_finite_f64(v).is_some() {
                Val::Num(n.clone())
            } else {
                Val::Null
            }
        }
        Val::TStr(b) | Val::BStr(b) => {
            let Ok(s) = std::str::from_utf8(b) else {
                return Val::Null;
            };
            let t = s.trim();
            if t.is_empty() {
                return Val::Null;
            }
            if !t.contains(['.', 'e', 'E'])
                && let Ok(i) = t.parse::<i64>()
            {
                return Val::Num(Num::from_integral(i));
            }
            match t.parse::<f64>() {
                Ok(f) if f.is_finite() => Val::Num(Num::Float(f)),
                _ => Val::Null,
            }
        }
        _ => Val::Null,
    }
}

/// #10 `to_int`: safe integer conversion returning `null` instead of jq `empty`.
fn fn_to_int(v: &Val) -> Val {
    match v {
        Val::Num(Num::Int(i)) => Val::Num(Num::Int(*i)),
        Val::Num(Num::BigInt(bi)) => match i64::try_from(bi.as_ref()) {
            Ok(i) => Val::Num(Num::from_integral(i)),
            Err(_) => Val::Null,
        },
        Val::Num(Num::Float(f)) => f64_to_i64_val(*f),
        Val::Num(Num::Dec(s)) => str_to_i64_val(s),
        Val::TStr(b) | Val::BStr(b) => std::str::from_utf8(b)
            .map(str_to_i64_val)
            .unwrap_or(Val::Null),
        _ => Val::Null,
    }
}

fn f64_to_i64_val(f: f64) -> Val {
    if !f.is_finite() {
        return Val::Null;
    }
    let tr = f.trunc();
    // 2^63 = 9223372036854775808.0 is exact in f64; i64::MAX is 2^63 - 1.
    if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&tr) {
        Val::Num(Num::from_integral(tr as i64))
    } else {
        Val::Null
    }
}

fn str_to_i64_val(s: &str) -> Val {
    let t = s.trim();
    if t.is_empty() {
        return Val::Null;
    }
    if let Ok(i) = t.parse::<i64>() {
        return Val::Num(Num::from_integral(i));
    }
    if let Ok(f) = t.parse::<f64>() {
        return f64_to_i64_val(f);
    }
    Val::Null
}

/// #11 `doi_norm`: normalize DOI string and validate `^10\.\d{4,9}/\S+$`.
pub fn doi_norm_str(raw: &str) -> Option<String> {
    static DOI_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(?:https?://(?:dx\.)?doi\.org/|doi:\s*)").expect("regex")
    });
    static DOI_VALID_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^10\.\d{4,9}/\S+$").expect("regex"));

    let mut s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Strip resolver / doi: prefix (loop handles e.g. "doi: https://doi.org/10....")
    while let Some(m) = DOI_PREFIX_RE.find(s) {
        s = s[m.end()..].trim_start();
    }
    let trimmed = s.trim_end_matches(['.', ',', ';', ')']);
    let lower = trimmed.to_ascii_lowercase();
    DOI_VALID_RE.is_match(&lower).then_some(lower)
}

/// #12 `openalex_abstract`: reconstruct OpenAlex `abstract_inverted_index`.
fn fn_openalex_abstract(v: &Val) -> Option<String> {
    let Val::Obj(map) = v else {
        return None;
    };
    let mut pairs: Vec<(usize, &str)> = Vec::new();
    for (word_val, positions_val) in map.iter() {
        let Some(word) = val_as_str(word_val) else {
            continue;
        };
        let Val::Arr(positions) = positions_val else {
            continue;
        };
        for pos_v in positions.iter() {
            if let Some(p) = val_as_finite_f64(pos_v)
                && p >= 0.0
                && p.fract() == 0.0
                && p <= 1_000_000.0
            {
                pairs.push((p as usize, word));
            }
        }
    }
    if pairs.is_empty() {
        return None;
    }
    pairs.sort_by_key(|(pos, _)| *pos);
    Some(
        pairs
            .into_iter()
            .map(|(_, w)| w)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// #13 `resolve_url($base)`: RFC 3986 URL resolution allowing only http(s) schemes.
pub fn resolve_url_str(input: &str, base: &str) -> Option<String> {
    let inp = input.trim();
    if inp.is_empty() {
        return None;
    }
    // Check if input already has a scheme (e.g. http://, https://, javascript:, data:).
    if let Ok(parsed) = Url::parse(inp) {
        return matches!(parsed.scheme(), "http" | "https").then(|| inp.to_owned());
    }
    let base_url = Url::parse(base.trim()).ok()?;
    if !matches!(base_url.scheme(), "http" | "https") {
        return None;
    }
    let joined = base_url.join(inp).ok()?;
    matches!(joined.scheme(), "http" | "https").then(|| joined.into())
}

fn format_load_errs<P>(errs: &jaq_core::load::Errors<&str, P>) -> String {
    let mut parts = Vec::new();
    for (_file, err) in errs {
        match err {
            jaq_core::load::Error::Io(io_errs) => {
                for (path, msg) in io_errs {
                    parts.push(format!("import ({path}): {msg}"));
                }
            }
            jaq_core::load::Error::Lex(lex_errs) => {
                for (expected, found) in lex_errs {
                    parts.push(format!("lex expected {expected:?}, found {found:?}"));
                }
            }
            jaq_core::load::Error::Parse(parse_errs) => {
                for (expected, found) in parse_errs {
                    parts.push(format!("parse expected {expected:?}, found {found:?}"));
                }
            }
        }
    }
    parts.join("; ")
}

fn format_compile_errs<P>(errs: &jaq_core::compile::Errors<&str, P>) -> String {
    let mut parts = Vec::new();
    for (_file, comp_errs) in errs {
        for (sym, undef) in comp_errs {
            parts.push(format!("undefined {} {sym:?}", undef.as_str()));
        }
    }
    parts.join("; ")
}
