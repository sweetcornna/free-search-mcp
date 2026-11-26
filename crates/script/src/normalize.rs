//! Code normalization and the TypeScript strip retry (`SPEC.md` §3.3, S10).
//!
//! The script is wrapped as the default export of a module that only *defines*
//! an async arrow; [`rquickjs::Module::declare`] compiles it with
//! `JS_EVAL_FLAG_COMPILE_ONLY`, so a `SyntaxError` there is compile-time by
//! construction and nothing has run yet. Only such an error moves on to the
//! next candidate, and only after both plain-JS candidates fail does the code
//! go through `swc_ts_fast_strip` (strip-only: types become whitespace, so
//! every line and column stays put). A `SyntaxError` thrown while the script
//! runs (`JSON.parse("{")`) is an ordinary runtime error and never causes a
//! second run.
//!
//! Every wrapper prefix sits on the script's first line and every suffix starts
//! with a newline (a trailing `// comment` cannot swallow it), so line numbers
//! in errors equal the submitted text's; only columns on line 1 shift, by
//! [`Candidate::line1_offset`].

use std::panic::{AssertUnwindSafe, catch_unwind};

/// Module name, and so the file name in every stack frame of user code.
pub(crate) const MODULE_NAME: &str = "exec.js";

/// How a candidate wraps the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// `async () => (CODE)`: a bare expression. If it evaluates to a function
    /// (`async () => {...}`, `function main() {...}`), that function is called.
    Expression,
    /// `async () => { CODE }`: statements, `return` gives the result.
    Statements,
}

/// One way of wrapping the script into module source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub form: Form,
    /// Complete module source handed to `Module::declare`.
    pub source: String,
    /// Characters the wrapper adds in front of line 1.
    pub line1_offset: u32,
}

/// [`normalize_code`] output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    /// The script with outer Markdown fences removed (same line numbering as
    /// the submitted text).
    pub body: String,
    /// Expression form first (a second expression candidate when dropping a
    /// trailing `; // comment` changed the text), then statements.
    pub candidates: Vec<Candidate>,
}

const EXPR_PREFIX: &str = "export default (async () => (";
const EXPR_SUFFIX: &str = "\n));\n";
const STMT_PREFIX: &str = "export default (async () => {";
const STMT_SUFFIX: &str = "\n});\n";

/// Strip one pair of outer Markdown fences. The opening fence line becomes an
/// empty line, so line numbers still match what the caller sent.
pub fn strip_fences(code: &str) -> String {
    let trimmed = code.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return code.to_owned();
    };
    // The info string (`js`, `typescript`, ...) runs to the end of the line.
    let Some(nl) = rest.find('\n') else {
        return code.to_owned();
    };
    let info = rest[..nl].trim();
    if info.contains('`') || info.contains(' ') {
        return code.to_owned();
    }
    let mut inner = &rest[nl + 1..];
    match inner.trim_end().strip_suffix("```") {
        Some(body) if body.is_empty() || body.ends_with('\n') => inner = body,
        _ => return code.to_owned(),
    }
    // Keep the leading lines the caller had before the fence (all blank).
    let lead = code.len() - code.trim_start().len();
    let blank_lines = code[..lead].matches('\n').count();
    let mut out = "\n".repeat(blank_lines + 1);
    out.push_str(inner.trim_end_matches(['\n', '\r']));
    out
}

/// Fence-strip the script and build the wrapper candidates.
pub fn normalize_code(code: &str) -> Normalized {
    let body = strip_fences(code);
    let expression = |expr: &str| Candidate {
        form: Form::Expression,
        source: format!("{EXPR_PREFIX}{expr}{EXPR_SUFFIX}"),
        line1_offset: EXPR_PREFIX.chars().count() as u32,
    };
    // The expression form tolerates trailing semicolons (`await web.fetch(u);`),
    // also before a trailing comment (`f(); // done`). Cutting at a `;` that sat
    // inside a string or comment leaves an unterminated literal that does not
    // compile, so keep both the single-step cut (handles `"a; // b"); // c`),
    // the repeated cut (handles `f(); /* a */ ; // b`), and the plain cut as
    // fallback expression candidates before Statements.
    let plain = body.trim_end().trim_end_matches(';').trim_end();
    let step = strip_trailing_semi_once(&body);
    let cut = expression_text(&body);
    let mut candidates = Vec::with_capacity(4);
    for expr in [step, cut, plain] {
        let cand = expression(expr);
        if !candidates.contains(&cand) {
            candidates.push(cand);
        }
    }
    candidates.push(Candidate {
        form: Form::Statements,
        source: format!("{STMT_PREFIX}{body}{STMT_SUFFIX}"),
        line1_offset: STMT_PREFIX.chars().count() as u32,
    });
    Normalized { body, candidates }
}

/// How far back from the end a trailing `; // comment` is looked for.
const TRAILING_COMMENT_WINDOW: usize = 4096;

/// Strip one trailing `;` (plus any adjacent `;`s) and the comments after it.
fn strip_trailing_semi_once(body: &str) -> &str {
    let head = body.trim_end();
    let start = head.len().saturating_sub(TRAILING_COMMENT_WINDOW);
    match head
        .rmatch_indices(';')
        .take_while(|(i, _)| *i >= start)
        .find(|(i, _)| only_comments(&head[i + 1..]))
    {
        Some((i, _)) => head[..i].trim_end().trim_end_matches(';').trim_end(),
        None => head,
    }
}

/// `body` without trailing semicolons and the comments after them.
fn expression_text(body: &str) -> &str {
    let mut head = body.trim_end();
    loop {
        let next = strip_trailing_semi_once(head);
        if next.len() == head.len() {
            return head;
        }
        head = next;
    }
}

/// Whether `s` holds nothing but whitespace and complete comments.
fn only_comments(mut s: &str) -> bool {
    loop {
        s = s.trim_start();
        if s.is_empty() {
            return true;
        }
        if let Some(rest) = s.strip_prefix("//") {
            match rest.find(['\n', '\r', '\u{2028}', '\u{2029}']) {
                Some(i) => s = &rest[i..],
                None => return true,
            }
        } else if let Some(rest) = s.strip_prefix("/*") {
            match rest.find("*/") {
                Some(i) => s = &rest[i + 2..],
                None => return false,
            }
        } else {
            return false;
        }
    }
}

/// Erase TypeScript syntax with `swc_ts_fast_strip` in strip-only mode: type
/// annotations, interfaces, `as`/`satisfies` and friends become whitespace, so
/// positions are preserved. Fails on syntax that needs a transform (`enum`,
/// `namespace`, parameter properties) and on anything that is not TS either.
pub fn strip_types(src: &str) -> Result<String, String> {
    use swc_common::errors::Handler;
    use swc_common::sync::Lrc;
    use swc_common::{GLOBALS, Globals, SourceMap};
    use swc_ts_fast_strip::{Mode, Options, operate};

    let run = || {
        let cm: Lrc<SourceMap> = Default::default();
        let handler = Handler::with_emitter_writer(Box::new(std::io::sink()), Some(cm.clone()));
        GLOBALS.set(&Globals::new(), || {
            let opts = Options {
                mode: Mode::StripOnly,
                module: Some(true),
                ..Options::default()
            };
            operate(&cm, &handler, src.to_owned(), opts)
                .map(|out| out.code)
                .map_err(|e| e.to_string())
        })
    };
    catch_unwind(AssertUnwindSafe(run)).unwrap_or_else(|_| Err("TypeScript strip panicked".into()))
}

/// `(line, column)` of the first `exec.js:L:C` frame in a QuickJS stack,
/// mapped back to the submitted script (1-based, clamped to its lines).
pub(crate) fn locate(stack: &str, cand: &Candidate, body: &str) -> Option<(u32, u32)> {
    let needle = format!("{MODULE_NAME}:");
    let at = stack.find(&needle)? + needle.len();
    let mut parts = stack[at..].splitn(3, ':');
    let line: u32 = parts.next()?.parse().ok()?;
    let col: u32 = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    let lines = body.lines().count().max(1) as u32;
    let line = line.clamp(1, lines);
    let col = if line == 1 {
        col.saturating_sub(cand.line1_offset).max(1)
    } else {
        col.max(1)
    };
    Some((line, col))
}

/// The submitted line `line` (1-based), trimmed and clipped for an error snippet.
pub(crate) fn snippet(body: &str, line: u32) -> String {
    let text = body
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .unwrap_or("")
        .trim();
    let clipped: String = text.chars().take(160).collect();
    if clipped.len() < text.len() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_are_stripped_keeping_line_numbers() {
        let code = "```js\nconst a = 1;\nreturn a;\n```";
        let out = strip_fences(code);
        assert_eq!(out, "\nconst a = 1;\nreturn a;");
        assert_eq!(out.lines().nth(2), Some("return a;"), "line 3 stays line 3");
        assert_eq!(strip_fences("```typescript\nx\n```\n"), "\nx");
        assert_eq!(strip_fences("\n\n```\n1+1\n```"), "\n\n\n1+1");
        // Not a fence pair: unchanged.
        for s in [
            "1 + 1",
            "```js\nno close",
            "``````",
            "```a b\nx\n```",
            "```js\nx```",
        ] {
            assert_eq!(strip_fences(s), s, "{s:?}");
        }
    }

    /// Review round 1, I5: a trailing `;` followed by a comment used to
    /// defeat the expression form (and silently return `undefined`).
    #[test]
    fn r1_i5_trailing_semicolon_before_comments_is_dropped() {
        let expr = |code: &str| normalize_code(code).candidates[0].source.clone();
        let wrap = |e: &str| format!("{EXPR_PREFIX}{e}{EXPR_SUFFIX}");
        assert_eq!(
            expr("async () => { return 42; }; // done"),
            wrap("async () => { return 42; }")
        );
        assert_eq!(expr("f(); /* a;\nb */ // c"), wrap("f()"));
        assert_eq!(expr("f(); // a; b"), wrap("f()"));
        assert_eq!(expr("f();;  \n"), wrap("f()"));
        // Code after the last `;`, or an unclosed comment, stays put.
        assert_eq!(expr("a(); b()"), wrap("a(); b()"));
        assert_eq!(expr("f(); /* open"), wrap("f(); /* open"));
        // A `;` inside a string: the cut does not compile, so the plain
        // expression follows it, then the statements form.
        let n = normalize_code("f('a;//b')");
        let forms: Vec<Form> = n.candidates.iter().map(|c| c.form).collect();
        assert_eq!(
            forms,
            [Form::Expression, Form::Expression, Form::Statements]
        );
        assert_eq!(n.candidates[0].source, wrap("f('a"));
        assert_eq!(n.candidates[1].source, wrap("f('a;//b')"));
        assert_eq!(normalize_code("f()").candidates.len(), 2);
    }

    #[test]
    fn candidates_keep_lines_and_report_offsets() {
        let n = normalize_code("await web.fetch(u);  ");
        assert_eq!(n.candidates[0].form, Form::Expression);
        assert_eq!(
            n.candidates[0].source,
            "export default (async () => (await web.fetch(u)\n));\n"
        );
        assert_eq!(n.candidates[1].form, Form::Statements);
        assert!(
            n.candidates[1]
                .source
                .starts_with("export default (async () => {await web.fetch(u);")
        );
        let n = normalize_code("const x = 1;\n// note\nreturn x // trailing comment");
        for c in &n.candidates {
            assert_eq!(
                c.source.lines().nth(2).map(str::trim_end),
                Some("return x // trailing comment")
            );
            assert_eq!(c.line1_offset as usize, c.source.find("const").unwrap());
        }
    }

    #[test]
    fn locate_maps_line_one_columns_and_clamps() {
        let n = normalize_code("let = ;");
        let c = &n.candidates[1];
        let col = c.line1_offset + 5;
        assert_eq!(
            locate(&format!("    at exec.js:1:{col}\n"), c, &n.body),
            Some((1, 5))
        );
        assert_eq!(
            locate("at f (exec.js:9:3)", c, &n.body),
            Some((1, 1)),
            "clamped to the last line"
        );
        assert_eq!(locate("at <eval> (fsm-prelude.js:2:1)", c, &n.body), None);
        let body = "a\n  bb  \nc";
        assert_eq!(snippet(body, 2), "bb");
        assert_eq!(snippet(&"x".repeat(300), 1).chars().count(), 161);
    }

    #[test]
    fn strip_types_preserves_positions() {
        let src = "export default (async () => {\n  interface Hit { title: string }\n  const n: number = 1 as number;\n  throw new Error('line 4');\n});";
        let out = strip_types(src).expect("strip");
        assert_eq!(out.lines().count(), src.lines().count());
        let line = out
            .lines()
            .position(|l| l.contains("line 4"))
            .map(|i| i + 1);
        assert_eq!(line, Some(4));
        assert!(
            out.contains("const n         = 1          ;") || !out.contains(": number"),
            "{out}"
        );
        assert!(
            strip_types("export default (async () => { enum E { A } });").is_err(),
            "enum needs a transform"
        );
        assert!(strip_types("export default ((( ;").is_err());
    }
}
