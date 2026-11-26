//! User manifest override loader (`<config_dir>/sources.d/*.toml`) enforcing
//! DoS lint (SPEC.md §4) and built-in ID / auth.env isolation (S4).

use crate::jaq_check;
use fsm_core::FsmError;
use fsm_core::manifest::ManifestDecl;
use regex::Regex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Outcome of loading one user manifest file.
#[derive(Debug, Clone)]
pub struct LoadedUserManifest {
    pub path: PathBuf,
    pub manifest: ManifestDecl,
}

/// Static DoS linter for untrusted user `jaq` expressions (SPEC.md §4).
///
/// Forbids unbounded loop/recursion/combinatorial constructs (`while`, `until`, `repeat`,
/// `recurse`, `walk`, `combinations`, `paths`, `def`, `range`, `..`), environment inspection
/// (`env`), process exit (`halt`, `halt_error`), and raw output (`stderr`, `debug`).
pub fn lint_user_jaq(expr: &str) -> Result<(), String> {
    static FORBIDDEN_IDENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\b(while|until|repeat|recurse|walk|combinations|paths|def|range|env|halt|halt_error|stderr|stderr_empty|debug|debug_empty)\b",
        )
        .expect("valid lint regex")
    });

    // Strip string literals and comments (tracking nested `\( ... )` interpolations)
    // so harmless strings like "repeat" or "https://a..b" do not trigger false positives
    // while code inside or after interpolations remains visible to the linter.
    let stripped = strip_jq_strings_and_comments(expr)?;

    if let Some(m) = FORBIDDEN_IDENT.find(&stripped) {
        return Err(format!(
            "user manifest jaq filter forbids {:?} (DoS/isolation lint)",
            m.as_str()
        ));
    }
    if stripped.contains("..") {
        return Err(
            "user manifest jaq filter forbids recursive descent '..' (DoS lint)".to_owned(),
        );
    }
    Ok(())
}

/// Replace jq double-quoted string literal text and `# ...` comments with spaces while
/// tracking nested `\( ... )` string interpolation expressions accurately.
fn strip_jq_strings_and_comments(src: &str) -> Result<String, String> {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let mut in_str = false;
    // Stack of open parenthesis nesting depths inside active `\( ... )` interpolations.
    let mut interp_stack: Vec<usize> = Vec::new();

    while let Some(c) = chars.next() {
        if in_str {
            if c == '\\' {
                if let Some(&'(') = chars.peek() {
                    chars.next();
                    interp_stack.push(0);
                    in_str = false;
                    out.push(' ');
                } else {
                    if chars.next().is_none() {
                        return Err("unterminated escape sequence in jaq string".to_owned());
                    }
                    out.push(' ');
                }
            } else if c == '"' {
                in_str = false;
                out.push(' ');
            } else {
                out.push(' ');
            }
        } else if c == '"' {
            in_str = true;
            out.push(' ');
        } else if c == '(' {
            if let Some(depth) = interp_stack.last_mut() {
                *depth += 1;
            }
            out.push('(');
        } else if c == ')' {
            if let Some(depth) = interp_stack.last_mut() {
                if *depth == 0 {
                    interp_stack.pop();
                    in_str = true;
                    out.push(' ');
                } else {
                    *depth -= 1;
                    out.push(')');
                }
            } else {
                out.push(')');
            }
        } else if c == '#' {
            for nc in chars.by_ref() {
                if nc == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }

    if in_str || !interp_stack.is_empty() {
        return Err("unterminated string or interpolation in jaq filter".to_owned());
    }
    Ok(out)
}

/// Validate a single user manifest TOML string against schema, `jaq` compilation,
/// DoS lint, and S4 isolation (`forbidden_ids`, `forbidden_auth_envs`).
pub fn parse_and_validate_user_manifest(
    toml_text: &str,
    forbidden_ids: &BTreeSet<String>,
    forbidden_auth_envs: &BTreeSet<String>,
) -> Result<ManifestDecl, FsmError> {
    let manifest = ManifestDecl::parse_validated(toml_text)?;

    // S4: user manifests MUST NOT reuse any built-in or reserved source ID.
    if forbidden_ids.contains(&manifest.id) {
        return Err(FsmError::invalid(format!(
            "user manifest id {:?} conflicts with a built-in or reserved source id (S4)",
            manifest.id
        )));
    }

    // S4: user manifests MUST NOT reference any built-in source's auth env var.
    if let Some(auth) = &manifest.auth {
        for env_name in auth.credential_envs() {
            if forbidden_auth_envs.contains(env_name) {
                return Err(FsmError::invalid(format!(
                    "user manifest {:?} references built-in credential env {:?} (S4)",
                    manifest.id, env_name
                )));
            }
        }
    }

    // Compile every jaq expression with the manifest prelude.
    let jaq_errs = jaq_check::check_manifest_jaq(&manifest);
    if !jaq_errs.is_empty() {
        return Err(FsmError::invalid(format!(
            "user manifest {:?} jaq errors: {}",
            manifest.id,
            jaq_errs.join("; ")
        )));
    }

    // Static DoS lint on every jaq expression.
    for (op_name, op) in manifest.op.iter() {
        let check_dos = |label: String, expr: &str| -> Result<(), FsmError> {
            let resolved = manifest.resolve_ref(expr)?;
            lint_user_jaq(resolved).map_err(|e| FsmError::invalid(format!("{label}: {e}")))
        };
        if let Some(qj) = &op.query_jaq {
            check_dos(format!("op.{op_name}.query_jaq"), qj)?;
        }
        if let Some(b) = &op.body {
            if let Some(j) = &b.jaq {
                check_dos(format!("op.{op_name}.body.jaq"), j)?;
            }
            if let Some(vj) = &b.variables_jaq {
                check_dos(format!("op.{op_name}.body.variables_jaq"), vj)?;
            }
        }
        for (i, pre) in op.pre.iter().enumerate() {
            check_dos(format!("op.{op_name}.pre[{i}].bind"), &pre.bind)?;
            if let Some(b) = &pre.body {
                if let Some(j) = &b.jaq {
                    check_dos(format!("op.{op_name}.pre[{i}].body.jaq"), j)?;
                }
                if let Some(vj) = &b.variables_jaq {
                    check_dos(format!("op.{op_name}.pre[{i}].body.variables_jaq"), vj)?;
                }
            }
        }
        if let Some(m) = &op.map {
            check_dos(format!("op.{op_name}.map.items"), &m.items)?;
            for (field, val) in [
                ("next_cursor", m.next_cursor.as_deref()),
                ("total", m.total.as_deref()),
                ("error", m.error.as_deref()),
                ("work", m.work.as_deref()),
                ("record", m.record.as_deref()),
                ("hit", m.hit.as_deref()),
            ] {
                if let Some(v) = val {
                    check_dos(format!("op.{op_name}.map.{field}"), v)?;
                }
            }
        }
    }

    Ok(manifest)
}

/// Load all valid user overrides from `dir` (typically `<config_dir>/sources.d`).
///
/// Missing directory is treated as empty (not an error). Individual invalid files
/// are returned in the error list (and logged by the caller) without aborting the
/// remaining valid user manifests.
pub fn load_user_overrides(
    dir: &Path,
    builtin_ids: &BTreeSet<String>,
    builtin_auth_envs: &BTreeSet<String>,
) -> (Vec<LoadedUserManifest>, Vec<(PathBuf, FsmError)>) {
    if !dir.is_dir() {
        return (Vec::new(), Vec::new());
    }
    let mut files = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_file() && p.extension().is_some_and(|e| e == "toml") {
                    files.push(p);
                }
            }
        }
        Err(e) => {
            return (
                Vec::new(),
                vec![(
                    dir.to_path_buf(),
                    FsmError::invalid(format!("cannot read {}: {e}", dir.display())),
                )],
            );
        }
    }
    files.sort();

    let mut loaded = Vec::new();
    let mut errors = Vec::new();
    let mut seen_ids = builtin_ids.clone();

    for path in files {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                errors.push((
                    path.clone(),
                    FsmError::invalid(format!("cannot read {}: {e}", path.display())),
                ));
                continue;
            }
        };
        match parse_and_validate_user_manifest(&text, &seen_ids, builtin_auth_envs) {
            Ok(manifest) => {
                seen_ids.insert(manifest.id.clone());
                loaded.push(LoadedUserManifest { path, manifest });
            }
            Err(e) => errors.push((path, e)),
        }
    }
    (loaded, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_user_manifest(id: &str, env_var: Option<&str>, items_expr: &str) -> String {
        let auth = match env_var {
            Some(e) => format!(
                "\n[auth]\nmode = \"optional\"\nenv = \"{e}\"\nin = \"header\"\nname = \"X-Key\"\n"
            ),
            None => String::new(),
        };
        format!(
            r#"
id = "{id}"
name = "User {id}"
summary = "Custom user manifest."
tier = 2
scopes = ["web"]
caps = ["search"]
transport = "plain"
pool_role = "explicit"
limits = {{ rps = 1.0, burst = 2, concurrency = 1, timeout_ms = 5000, cache_ttl_s = 60 }}
{auth}
[op.search]
summary = "Search."
method = "GET"
url = "https://user.example.org/search"
response = "json"
returns = "hits"
[op.search.params]
query = {{ type = "string", required = true }}
[op.search.map]
items = "{items_expr}"
hit = '{{ title: .t, url: .u, snippet: .s }}'
"#
        )
    }

    #[test]
    fn load_user_overrides_enforces_s4_and_dos_lint_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        // 1. Valid user manifest with own custom env var
        fs::write(
            dir.path().join("01_good.toml"),
            sample_user_manifest(
                "my_custom_src",
                Some("SEARCH_MCP_MY_CUSTOM_API_KEY"),
                ".items[]",
            ),
        )
        .unwrap();
        // 2. S4 violation: tries to shadow built-in seed ID `arxiv`
        fs::write(
            dir.path().join("02_shadow.toml"),
            sample_user_manifest("arxiv", None, ".items[]"),
        )
        .unwrap();
        // 3. S4 violation: tries to read built-in credential env var
        fs::write(
            dir.path().join("03_steal_env.toml"),
            sample_user_manifest(
                "stealer_src",
                Some("SEARCH_MCP_OPENALEX_API_KEY"),
                ".items[]",
            ),
        )
        .unwrap();
        // 4. DoS lint violation: `recurse` in jaq filter
        fs::write(
            dir.path().join("04_dos.toml"),
            sample_user_manifest("dos_src", None, "recurse"),
        )
        .unwrap();

        let builtin_ids = BTreeSet::from(["arxiv".to_owned()]);
        let builtin_envs = BTreeSet::from(["SEARCH_MCP_OPENALEX_API_KEY".to_owned()]);
        let (loaded, errors) = load_user_overrides(dir.path(), &builtin_ids, &builtin_envs);

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].manifest.id, "my_custom_src");
        assert_eq!(errors.len(), 3, "expected 3 rejected files, got {errors:?}");
    }
}
