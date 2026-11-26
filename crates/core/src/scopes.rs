//! Canonical scope vocabulary (`SPEC-contracts.md` §1.8).
//!
//! Manifests, native engines and instances declare canonical `id`s only;
//! callers may use any synonym, resolved here case-insensitively.

/// One scope of the vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ScopeDef {
    pub id: &'static str,
    pub parent: Option<&'static str>,
    /// Exclusive scopes never mix in general web engines.
    pub exclusive: bool,
    pub synonyms: &'static [&'static str],
}

const fn def(
    id: &'static str,
    parent: Option<&'static str>,
    exclusive: bool,
    synonyms: &'static [&'static str],
) -> ScopeDef {
    ScopeDef {
        id,
        parent,
        exclusive,
        synonyms,
    }
}

/// The complete vocabulary, in documentation order.
pub static SCOPES: &[ScopeDef] = &[
    def("web", None, false, &["general", "internet"]),
    def(
        "academic",
        None,
        true,
        &["paper", "papers", "scholar", "literature", "论文", "学术"],
    ),
    def("academic.index", Some("academic"), true, &["paper.index"]),
    def(
        "academic.preprint",
        Some("academic"),
        true,
        &["paper.preprint", "preprints"],
    ),
    def(
        "academic.biomed",
        Some("academic"),
        true,
        &["paper.biomed", "health", "medicine", "biomedical"],
    ),
    def(
        "academic.cs",
        Some("academic"),
        true,
        &["paper.cs", "ai", "ml"],
    ),
    def("academic.math", Some("academic"), true, &["paper.math"]),
    def(
        "academic.physics",
        Some("academic"),
        true,
        &["physics", "astro", "hep"],
    ),
    def(
        "academic.openaccess",
        Some("academic"),
        true,
        &["paper.openaccess", "oa"],
    ),
    def(
        "academic.trial",
        Some("academic"),
        true,
        &["paper.trial", "trials", "clinical"],
    ),
    def(
        "academic.humanities",
        Some("academic"),
        true,
        &["humanities", "social_science"],
    ),
    def(
        "academic.grants",
        Some("academic"),
        true,
        &["grants", "funding"],
    ),
    def(
        "academic.theses",
        Some("academic"),
        true,
        &["theses", "dissertations"],
    ),
    def(
        "academic.books",
        Some("academic"),
        true,
        &["books", "monographs"],
    ),
    def("news", None, false, &["news.world", "press"]),
    def(
        "code",
        None,
        false,
        &[
            "github",
            "software",
            "packages",
            "software.github",
            "software.python",
            "software.node",
            "software.rust",
            "software.registry",
            "software.lifecycle",
            "software.app",
        ],
    ),
    def(
        "docs",
        None,
        false,
        &["docs.web", "docs.rfc", "documentation", "standards"],
    ),
    def(
        "dataset",
        None,
        true,
        &[
            "data",
            "datasets",
            "dataset.repository",
            "dataset.ml",
            "dataset.gov",
            "数据集",
        ],
    ),
    def(
        "finance",
        None,
        false,
        &[
            "finance.filings",
            "finance.market",
            "finance.macro",
            "finance.fx",
            "finance.entity",
            "finance.crypto",
        ],
    ),
    def(
        "security",
        None,
        false,
        &[
            "security.cve",
            "security.package",
            "security.exploited",
            "cve",
            "vuln",
        ],
    ),
    def(
        "gov",
        None,
        false,
        &["gov.us", "gov.uk", "gov.eu", "government"],
    ),
    def(
        "gov.law",
        Some("gov"),
        false,
        &["law", "legal", "statutes", "法律"],
    ),
    def(
        "gov.patents",
        Some("gov"),
        false,
        &["patents", "patent", "专利"],
    ),
    def(
        "media",
        None,
        true,
        &["image", "images", "video", "audio", "podcast"],
    ),
    // A1 §G.3: storefronts and price sources stay out of `reference` and `web`.
    def(
        "commerce",
        None,
        true,
        &[
            "shop", "store", "shopping", "products", "prices", "商品", "电商",
        ],
    ),
    def("forum", None, false, &["social", "community", "qa"]),
    def(
        "reference",
        None,
        false,
        &[
            "reference.domain",
            "weather",
            "calendar",
            "calendar.holidays",
            "calendar.clock",
            "stats",
            "stats.indicator",
            "wiki",
        ],
    ),
    def(
        "reference.geo",
        Some("reference"),
        false,
        &["geo", "maps", "places"],
    ),
    def("cache", None, true, &["local_cache"]),
    def(
        "catalog",
        None,
        true,
        &["sdk", "api", "tools", "engines", "sources", "目录"],
    ),
];

/// The scope with exactly this canonical id.
pub fn get(id: &str) -> Option<&'static ScopeDef> {
    SCOPES.iter().find(|s| s.id == id)
}

/// Resolve a caller-supplied scope or synonym (trimmed, ASCII
/// case-insensitive) to its canonical definition.
pub fn resolve(input: &str) -> Option<&'static ScopeDef> {
    let key = input.trim().to_ascii_lowercase();
    if key.is_empty() {
        return None;
    }
    get(&key).or_else(|| SCOPES.iter().find(|s| s.synonyms.contains(&key.as_str())))
}

/// `declared` equals `requested` or is one of its descendants
/// (`is_within("gov.law", "gov")`). Both must be canonical ids.
pub fn is_within(declared: &str, requested: &str) -> bool {
    declared == requested
        || (declared.len() > requested.len()
            && declared.starts_with(requested)
            && declared.as_bytes()[requested.len()] == b'.')
}

/// Top-level ancestor of a canonical id (`"academic"` for `"academic.cs"`).
pub fn root(id: &str) -> &str {
    id.split_once('.').map_or(id, |(r, _)| r)
}

/// Direct children of a canonical id.
pub fn children(id: &str) -> impl Iterator<Item = &'static ScopeDef> + '_ {
    SCOPES.iter().filter(move |s| s.parent == Some(id))
}

/// Comma-separated canonical ids, for "unknown scope" error messages.
pub fn valid_scope_list() -> String {
    SCOPES.iter().map(|s| s.id).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn table_is_consistent() {
        assert_eq!(SCOPES.len(), 30);
        let ids: HashSet<&str> = SCOPES.iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), SCOPES.len(), "duplicate id");
        let mut seen = HashSet::new();
        for s in SCOPES {
            if let Some(p) = s.parent {
                assert!(ids.contains(p), "{} has unknown parent {p}", s.id);
                assert!(s.id.starts_with(&format!("{p}.")), "{} not under {p}", s.id);
                assert_eq!(
                    get(p).unwrap().exclusive,
                    s.exclusive,
                    "{} exclusivity differs from parent",
                    s.id
                );
            } else {
                assert!(!s.id.contains('.'), "{} dotted without parent", s.id);
            }
            for syn in s.synonyms {
                assert!(!ids.contains(syn), "synonym {syn} shadows an id");
                assert!(seen.insert(*syn), "synonym {syn} maps to two scopes");
            }
        }
    }

    #[test]
    fn resolves_spec_synonyms() {
        let r = |s: &str| resolve(s).map(|d| d.id);
        // SPEC.md §2.3 examples.
        for (input, want) in [
            ("paper", "academic"),
            ("papers", "academic"),
            ("scholar", "academic"),
            ("literature", "academic"),
            ("论文", "academic"),
            ("sdk", "catalog"),
            ("API", "catalog"),
            ("tools", "catalog"),
            ("engines", "catalog"),
            ("sources", "catalog"),
            ("目录", "catalog"),
            ("github", "code"),
            ("software", "code"),
            ("packages", "code"),
            ("weather", "reference"),
            ("calendar", "reference"),
            ("stats", "reference"),
            ("image", "media"),
            ("video", "media"),
            ("law", "gov.law"),
            ("patents", "gov.patents"),
            ("health", "academic.biomed"),
            ("geo", "reference.geo"),
            ("  Web ", "web"),
            ("paper.cs", "academic.cs"),
            ("software.rust", "code"),
            ("shop", "commerce"),
            ("电商", "commerce"),
        ] {
            assert_eq!(r(input), Some(want), "{input}");
        }
        assert_eq!(r("pdf"), None);
        assert_eq!(r(""), None);
        assert!(valid_scope_list().starts_with("web, academic"));
    }

    #[test]
    fn hierarchy_helpers() {
        assert!(is_within("gov.law", "gov"));
        assert!(is_within("gov", "gov"));
        assert!(!is_within("gov", "gov.law"));
        assert!(!is_within("governance", "gov"));
        assert_eq!(root("academic.cs"), "academic");
        assert_eq!(children("academic").count(), 12);
        assert_eq!(
            children("reference").map(|d| d.id).collect::<Vec<_>>(),
            ["reference.geo"]
        );
        assert!(get("academic").unwrap().exclusive && !get("web").unwrap().exclusive);
        assert!(get("commerce").unwrap().exclusive && get("commerce").unwrap().parent.is_none());
    }
}
