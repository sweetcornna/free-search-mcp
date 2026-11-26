//! Weighted reciprocal-rank fusion, field-wise absorption and fuzzy title
//! dedup (ports of `aggregator._merge`, `_absorb`, `_dedup_by_title`).
//!
//! Ranking order is deterministic: scores accumulate in bucket order, keys
//! keep their first-seen position, and the final sort is stable.

use fsm_core::Hit;
use fsm_core::url_canon::{canonical_host, normalize_url, url_key};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// RRF damping constant (Cormack et al. 2009; moving it 5–60 changed MRR by < 0.01).
pub const RRF_K: f64 = 60.0;
/// One native hit ties two general engines agreeing (`_NATIVE_CATEGORY_WEIGHT`).
pub const NATIVE_WEIGHT: f64 = 2.0;
/// A direct claimant's first result: one looked-up record outranks the whole
/// default pool agreeing on a page about it (`_DIRECT_ANSWER_WEIGHT`).
pub const DIRECT_WEIGHT: f64 = 5.0;
/// Same-host titles at or above this `token_set_ratio` are one result.
pub const DEDUP_RATIO: f64 = 92.0;

/// Which buckets count extra in the fusion.
#[derive(Debug, Clone, Default)]
pub struct Weighting {
    /// Sources whose every result counts [`NATIVE_WEIGHT`].
    pub native: HashSet<String>,
    /// Sources whose FIRST result counts [`DIRECT_WEIGHT`] (later ones native).
    pub direct: HashSet<String>,
}

/// rapidfuzz `fuzz.token_set_ratio` (no preprocessing, 0–100), built on the
/// Indel distance the Rust crate exposes.
pub fn token_set_ratio(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    fn split(s: &str) -> Vec<&str> {
        let mut v: Vec<&str> = s.split_whitespace().collect();
        v.sort_unstable();
        v.dedup();
        v
    }
    let (ta, tb) = (split(a), split(b));
    let inter: Vec<&str> = ta.iter().filter(|t| tb.contains(t)).copied().collect();
    let diff_ab: Vec<&str> = ta.iter().filter(|t| !tb.contains(t)).copied().collect();
    let diff_ba: Vec<&str> = tb.iter().filter(|t| !ta.contains(t)).copied().collect();
    if !inter.is_empty() && (diff_ab.is_empty() || diff_ba.is_empty()) {
        return 100.0;
    }
    let ab = diff_ab.join(" ");
    let ba = diff_ba.join(" ");
    let (ab_len, ba_len) = (ab.chars().count(), ba.chars().count());
    let sect_len = inter.join(" ").chars().count();
    let sep = usize::from(sect_len > 0);
    let sect_ab_len = sect_len + sep + ab_len;
    let sect_ba_len = sect_len + sep + ba_len;
    let norm = |dist: usize, lensum: usize| {
        if lensum == 0 {
            100.0
        } else {
            100.0 * (1.0 - dist as f64 / lensum as f64)
        }
    };
    let dist = rapidfuzz::distance::indel::distance(ab.chars(), ba.chars());
    let result = norm(dist, sect_ab_len + sect_ba_len);
    if sect_len == 0 {
        return result;
    }
    let sect_ab_ratio = norm(sep + ab_len, sect_len + sect_ab_len);
    let sect_ba_ratio = norm(sep + ba_len, sect_len + sect_ba_len);
    result.max(sect_ab_ratio).max(sect_ba_ratio)
}

/// Matches Python `_NUM_RE = re.compile(r"\d+")` (Unicode decimal digits `\p{Nd}`,
/// excluding non-decimal numeric characters like `²`, `½`, `Ⅱ`).
static DIGIT_RUN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+").expect("digit run regex"));

fn digit_runs(s: &str) -> Vec<String> {
    DIGIT_RUN_RE
        .find_iter(s)
        .map(|m| m.as_str().to_owned())
        .collect()
}

/// `_dedup_by_title`: drop near-duplicate titles on the same canonical host
/// (AMP/mobile/ccTLD syndication). Different hosts are kept (wire stories);
/// titles whose digit tokens both exist and differ are distinct ("Python 3.13"
/// vs "3.12"); empty titles are kept.
pub fn dedup_by_title(items: Vec<Hit>) -> Vec<Hit> {
    let mut keep: Vec<Hit> = Vec::with_capacity(items.len());
    let mut keys: Vec<(String, Vec<String>, String)> = Vec::with_capacity(items.len());
    for it in items {
        let t = it.title.to_lowercase().trim().to_owned();
        if t.is_empty() {
            keep.push(it);
            keys.push((String::new(), Vec::new(), String::new()));
            continue;
        }
        let host = canonical_host(&it.url);
        let nums = digit_runs(&t);
        let dup = keys.iter().any(|(k_host, k_nums, kt)| {
            !kt.is_empty()
                && *k_host == host
                && !(!k_nums.is_empty() && !nums.is_empty() && *k_nums != nums)
                && token_set_ratio(&t, kt) >= DEDUP_RATIO
        });
        if !dup {
            keep.push(it);
            keys.push((host, nums, t));
        }
    }
    keep
}

/// A `date_source` that is present and not `snippet` (`api`, `rss`,
/// `structured`, ...) marks a date from a structured source.
pub fn is_confident(h: &Hit) -> bool {
    h.date.is_some()
        && h.date_source
            .as_deref()
            .is_some_and(|s| !matches!(s.trim(), "" | "snippet" | "none" | "text"))
}

struct Rep {
    hit: Hit,
    confident: bool,
    score: f64,
    engines: Vec<String>,
}

/// `_absorb`: fold another sighting of the same page into its representative,
/// field by field.
fn absorb(rep: &mut Rep, r: &Hit) {
    if r.snippet.chars().count() > rep.hit.snippet.chars().count() {
        rep.hit.snippet = r.snippet.clone();
    }
    if rep.hit.title.trim().is_empty() && !r.title.trim().is_empty() {
        rep.hit.title = r.title.clone();
    }
    if rep.hit.url.starts_with("http://") {
        let sighted = normalize_url(&r.url);
        if sighted.starts_with("https://") {
            rep.hit.url = sighted;
        }
    }
    let confident = is_confident(r);
    if r.date.is_some() && (rep.hit.date.is_none() || (confident && !rep.confident)) {
        rep.hit.date = r.date.clone();
        rep.hit.date_source = r.date_source.clone();
        rep.confident = confident;
    }
    if rep.hit.extra.is_none() && r.extra.is_some() {
        rep.hit.extra = r.extra.clone();
    }
}

/// Weighted RRF over per-source buckets (each in that source's rank order),
/// then title dedup over the FULL ranked list, then the cut to `limit` — so a
/// duplicate inside the top `limit` is backfilled.
///
/// Output hits carry `source` = first sighting's bucket, `sources` = sorted
/// unique buckets, `score` = rounded RRF score, `date_source` normalised to
/// `"structured"` | `"snippet"` (absent when undated).
pub fn fuse(buckets: &[(String, Vec<Hit>)], w: &Weighting, limit: usize) -> Vec<Hit> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut reps: Vec<Rep> = Vec::new();
    for (src, hits) in buckets {
        let native = w.native.contains(src);
        let direct = w.direct.contains(src);
        for (pos, h) in hits.iter().enumerate() {
            let url = normalize_url(&h.url);
            if url.is_empty() {
                continue;
            }
            let key = url_key(&url);
            let record = pos == 0 && direct;
            let weight = if record {
                DIRECT_WEIGHT
            } else if native || direct {
                NATIVE_WEIGHT
            } else {
                1.0
            };
            let add = weight / (RRF_K + (pos + 1) as f64);
            match index.get(&key) {
                Some(&i) => {
                    let rep = &mut reps[i];
                    rep.score += add;
                    rep.engines.push(src.clone());
                    absorb(rep, h);
                    if record && !h.title.trim().is_empty() {
                        rep.hit.title = h.title.clone();
                    }
                }
                None => {
                    let mut hit = h.clone();
                    hit.url = url;
                    hit.source = src.clone();
                    index.insert(key, reps.len());
                    reps.push(Rep {
                        confident: is_confident(&hit),
                        hit,
                        score: add,
                        engines: vec![src.clone()],
                    });
                }
            }
        }
    }
    let mut order: Vec<usize> = (0..reps.len()).collect();
    order.sort_by(|a, b| reps[*b].score.total_cmp(&reps[*a].score));
    let mut ranked = Vec::with_capacity(order.len());
    let mut slots: Vec<Option<Rep>> = reps.into_iter().map(Some).collect();
    for i in order {
        let Some(rep) = slots[i].take() else { continue };
        let mut hit = rep.hit;
        let mut engines = rep.engines;
        engines.sort();
        engines.dedup();
        hit.sources = engines;
        hit.score = (rep.score * 1e5).round() / 1e5;
        hit.date_source = hit.date.as_ref().map(|_| {
            if rep.confident {
                "structured"
            } else {
                "snippet"
            }
            .to_owned()
        });
        ranked.push(hit);
    }
    let mut out = dedup_by_title(ranked);
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(title: &str, url: &str) -> Hit {
        Hit::new(title, url)
    }

    /// Python reference values: `rapidfuzz.fuzz.token_set_ratio` from the
    /// v0.18.2 venv (rapidfuzz 3.x), generated 2026-11-25.
    #[test]
    fn token_set_ratio_matches_python_rapidfuzz() {
        let vectors: &[(&str, &str, f64)] = &[
            (
                "major storm hits east coast today",
                "major storm hits east coast today.",
                98.50746268656717,
            ),
            (
                "storm hits coast",
                "election results 2026",
                32.432432432432435,
            ),
            ("python 3.13 released", "python 3.12 released", 95.0),
            (
                "fed holds rates at 5% in 2026",
                "fed holds rates at 5% in 2026.",
                98.30508474576271,
            ),
            (
                "how async works in python",
                "how async works in python",
                100.0,
            ),
            (
                "[1512.03385] deep residual learning for image recognition",
                "deep residual learning for image recognition",
                100.0,
            ),
            (
                "deep residual learning for image recognition | ieee conference ...",
                "deep residual learning for image recognition",
                100.0,
            ),
            (
                "rust on steam",
                "rust — explore, build and survive",
                47.05882352941177,
            ),
            (
                "the 6 best mechanical keyboards of 2026",
                "the 6 best mechanical keyboards of 2025",
                97.43589743589743,
            ),
            ("a b c", "c b a", 100.0),
            ("hello world", "world hello there", 100.0),
            ("abc", "abd", 66.66666666666666),
            ("", "abc", 0.0),
            ("deepseek 解析", "deepseek 解析 v3", 100.0),
            ("上海天气预报 一周", "上海天气预报", 100.0),
            (
                "understanding ownership - the rust programming language",
                "references and borrowing - the rust programming language",
                72.09302325581396,
            ),
            (
                "vietnam - wikipedia",
                "history of vietnam - wikipedia",
                100.0,
            ),
            ("same story", "same  story ", 100.0),
            (
                "node.js 22 release notes",
                "node.js 22 release notes and changelog",
                100.0,
            ),
            ("x y z w", "x y q r s", 62.5),
        ];
        for (a, b, want) in vectors {
            let got = token_set_ratio(a, b);
            assert!((got - want).abs() < 1e-9, "{a:?} vs {b:?}: {got} != {want}");
        }
    }

    // --- tests/test_dedup_and_lead.py::_dedup_by_title ---

    #[test]
    fn dedup_removes_near_duplicate_title_on_same_canonical_host() {
        let out = dedup_by_title(vec![
            h(
                "Major Storm Hits East Coast Today",
                "https://www.bbc.com/news/storm",
            ),
            h(
                "Major Storm Hits East Coast Today.",
                "https://www.bbc.co.uk/news/storm-uk",
            ),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://www.bbc.com/news/storm");
    }

    #[test]
    fn dedup_keeps_differing_titles_on_same_host() {
        let out = dedup_by_title(vec![
            h("Storm hits coast", "https://www.bbc.com/news/storm"),
            h("Election results 2026", "https://www.bbc.com/news/election"),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn dedup_keeps_version_bumps_on_same_host() {
        let out = dedup_by_title(vec![
            h(
                "Python 3.13 released",
                "https://www.python.org/downloads/release/3130",
            ),
            h(
                "Python 3.12 released",
                "https://www.python.org/downloads/release/3120",
            ),
        ]);
        assert_eq!(out.len(), 2);
        let out = dedup_by_title(vec![
            h(
                "The 6 best mechanical keyboards of 2026",
                "https://www.nytimes.com/a",
            ),
            h(
                "The 6 best mechanical keyboards of 2025",
                "https://www.nytimes.com/b",
            ),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn dedup_still_collapses_when_numbers_match() {
        let out = dedup_by_title(vec![
            h(
                "Fed holds rates at 5% in 2026",
                "https://www.bbc.com/news/x",
            ),
            h(
                "Fed holds rates at 5% in 2026.",
                "https://amp.bbc.com/news/x",
            ),
        ]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn dedup_keeps_same_title_across_different_canonical_hosts() {
        let out = dedup_by_title(vec![
            h(
                "Breaking: Major Storm Hits Coast",
                "https://www.reuters.com/x",
            ),
            h(
                "Breaking: Major Storm Hits Coast",
                "https://www.apnews.com/x",
            ),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn dedup_collapses_amp_variant_with_same_title() {
        let out = dedup_by_title(vec![
            h("How async works in Python", "https://www.example.com/post"),
            h("How async works in Python", "https://amp.example.com/post"),
        ]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn dedup_keeps_items_without_title() {
        let out = dedup_by_title(vec![h("", "https://a.com/1"), h("", "https://b.com/2")]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn dedup_empty_input() {
        assert!(dedup_by_title(Vec::new()).is_empty());
    }

    #[test]
    fn arxiv_id_prefix_is_not_a_numeric_conflict() {
        // "[1512.03385] Deep Residual..." vs the same title without digits: one paper.
        let out = dedup_by_title(vec![
            h(
                "[1512.03385] Deep Residual Learning for Image Recognition",
                "https://arxiv.org/abs/1512.03385",
            ),
            h(
                "Deep Residual Learning for Image Recognition",
                "https://arxiv.org/pdf/1512.03385v1",
            ),
        ]);
        assert_eq!(out.len(), 1);
    }

    // --- _merge ---

    fn b(src: &str, hits: Vec<Hit>) -> (String, Vec<Hit>) {
        (src.to_owned(), hits)
    }

    fn sn(title: &str, url: &str, snippet: &str) -> Hit {
        let mut x = h(title, url);
        x.snippet = snippet.to_owned();
        x
    }

    #[test]
    fn merge_backfills_after_title_dedup_to_keep_max_results() {
        let bucket = vec![
            sn(
                "How async works in Python",
                "https://www.example.com/post",
                "snip",
            ),
            sn(
                "How async works in Python",
                "https://amp.example.com/post",
                "snip",
            ),
            sn(
                "Totally different unique article",
                "https://other.com/a",
                "snip",
            ),
            sn(
                "Another distinct article here",
                "https://third.com/b",
                "snip",
            ),
        ];
        let out = fuse(&[b("e1", bucket)], &Weighting::default(), 3);
        assert_eq!(out.len(), 3);
        let urls: Vec<&str> = out.iter().map(|x| x.url.as_str()).collect();
        assert_eq!(urls.iter().filter(|u| u.contains("example.com")).count(), 1);
        assert!(urls.contains(&"https://third.com/b"));
    }

    #[test]
    fn merge_never_exceeds_max_results() {
        let bucket = (0..10)
            .map(|i| {
                h(
                    &format!("Unique title {i}"),
                    &format!("https://site{i}.com/x"),
                )
            })
            .collect();
        assert_eq!(fuse(&[b("e1", bucket)], &Weighting::default(), 4).len(), 4);
    }

    fn dated(url: &str, snippet: &str, date: Option<&str>, src: Option<&str>) -> Hit {
        let mut x = sn("Same story", url, snippet);
        x.date = date.map(str::to_owned);
        x.date_source = src.map(str::to_owned);
        x
    }

    #[test]
    fn merge_keeps_the_trusted_date_when_another_engine_has_a_longer_snippet() {
        let structured = dated(
            "https://example.com/story",
            "short",
            Some("2026-08-27"),
            Some("api"),
        );
        let scraped = dated(
            "https://example.com/story",
            "a considerably longer snippet body here",
            None,
            None,
        );
        let out = fuse(
            &[b("gdelt", vec![structured]), b("duckduckgo", vec![scraped])],
            &Weighting::default(),
            5,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].date.as_deref(), Some("2026-08-27"));
        assert_eq!(out[0].date_source.as_deref(), Some("structured"));
        assert_eq!(out[0].snippet, "a considerably longer snippet body here");
    }

    #[test]
    fn merge_prefers_a_structured_date_over_a_scraped_one() {
        let scraped = dated(
            "https://example.com/story",
            "long snippet from the web page",
            Some("3 days ago"),
            Some("snippet"),
        );
        let structured = dated(
            "https://example.com/story",
            "s",
            Some("2026-08-27"),
            Some("rss"),
        );
        let out = fuse(
            &[
                b("duckduckgo", vec![scraped]),
                b("googlenews", vec![h("x", "https://z.org"), structured]),
            ],
            &Weighting::default(),
            5,
        );
        let story = out.iter().find(|x| x.url.ends_with("/story")).unwrap();
        assert_eq!(story.date.as_deref(), Some("2026-08-27"));
        assert_eq!(story.date_source.as_deref(), Some("structured"));
    }

    #[test]
    fn each_result_says_where_its_date_came_from() {
        // tests/test_freshness_signals.py::test_each_result_says_where_its_date_came_from
        let out = fuse(
            &[b(
                "googlenews",
                vec![
                    dated(
                        "https://a.example/feed",
                        "s",
                        Some("2 days ago"),
                        Some("rss"),
                    ),
                    dated("https://b.example/prose", "s", Some("Mar 3, 2021"), None),
                    dated("https://c.example/nothing", "s", None, None),
                ],
            )],
            &Weighting::default(),
            10,
        );
        let ds: Vec<Option<&str>> = out.iter().map(|x| x.date_source.as_deref()).collect();
        // Titles are identical on different hosts, so all three survive dedup.
        assert_eq!(ds, [Some("structured"), Some("snippet"), None]);
    }

    #[test]
    fn a_structured_date_wins_over_a_scraped_one_for_the_same_page() {
        let out = fuse(
            &[
                b(
                    "bing",
                    vec![dated("https://a.example/x", "s", Some("Jan 1, 2020"), None)],
                ),
                b(
                    "googlenews",
                    vec![dated(
                        "https://a.example/x",
                        "s",
                        Some("3 hours ago"),
                        Some("rss"),
                    )],
                ),
            ],
            &Weighting::default(),
            10,
        );
        assert_eq!(out[0].date.as_deref(), Some("3 hours ago"));
        assert_eq!(out[0].date_source.as_deref(), Some("structured"));
    }

    fn natives(n: &[&str]) -> Weighting {
        Weighting {
            native: n.iter().map(|s| (*s).to_owned()).collect(),
            direct: HashSet::new(),
        }
    }

    #[test]
    fn a_native_engine_hit_ties_two_general_engines_agreeing() {
        let out = fuse(
            &[
                b("arxiv", vec![h("T", "https://arxiv.org/abs/1")]),
                b("duckduckgo", vec![h("T", "https://blog.example/post")]),
                b("bing", vec![h("T", "https://blog.example/post")]),
            ],
            &natives(&["arxiv"]),
            5,
        );
        let score = |u: &str| out.iter().find(|x| x.url == u).unwrap().score;
        assert_eq!(
            score("https://arxiv.org/abs/1"),
            score("https://blog.example/post")
        );
    }

    #[test]
    fn a_native_engine_outranks_a_single_general_engine() {
        let out = fuse(
            &[
                b("arxiv", vec![h("T", "https://arxiv.org/abs/1")]),
                b("duckduckgo", vec![h("T", "https://blog.example/post")]),
            ],
            &natives(&["arxiv"]),
            5,
        );
        assert_eq!(out[0].url, "https://arxiv.org/abs/1");
    }

    #[test]
    fn the_boost_needs_a_category_to_apply_to() {
        let out = fuse(
            &[
                b("arxiv", vec![h("T", "https://arxiv.org/abs/1")]),
                b("duckduckgo", vec![h("T", "https://blog.example/post")]),
            ],
            &Weighting::default(),
            5,
        );
        assert_eq!(out[0].score, out[1].score);
        // Ties keep first-seen order.
        assert_eq!(out[0].url, "https://arxiv.org/abs/1");
    }

    #[test]
    fn an_exclusive_category_reweights_everything_equally() {
        let out = fuse(
            &[b(
                "openverse",
                vec![
                    h("Kitten", "https://img.example/a"),
                    h("Puppy", "https://img.example/b"),
                ],
            )],
            &natives(&["openverse"]),
            5,
        );
        assert_eq!(
            out.iter().map(|x| x.url.as_str()).collect::<Vec<_>>(),
            ["https://img.example/a", "https://img.example/b"]
        );
    }

    #[test]
    fn a_direct_record_leads_and_names_the_value() {
        // One looked-up record outranks three general engines agreeing (4/61 < 5/61),
        // and its title replaces the page title the web engines saw.
        let page = || h("fastapi · PyPI", "https://pypi.org/project/fastapi/");
        let out = fuse(
            &[
                b(
                    "duckduckgo",
                    vec![h("FastAPI", "https://fastapi.tiangolo.com"), page()],
                ),
                b(
                    "bing",
                    vec![h("FastAPI", "https://fastapi.tiangolo.com"), page()],
                ),
                b(
                    "seznam",
                    vec![h("FastAPI", "https://fastapi.tiangolo.com"), page()],
                ),
                b(
                    "pypi",
                    vec![h(
                        "fastapi 0.141.1 on PyPI",
                        "https://pypi.org/project/fastapi",
                    )],
                ),
            ],
            &Weighting {
                native: HashSet::new(),
                direct: ["pypi".to_owned()].into(),
            },
            5,
        );
        assert_eq!(out[0].url, "https://pypi.org/project/fastapi");
        assert_eq!(out[0].title, "fastapi 0.141.1 on PyPI");
        assert_eq!(out[0].sources, ["bing", "duckduckgo", "pypi", "seznam"]);
        assert_eq!(out[0].source, "duckduckgo", "first sighting");
    }

    #[test]
    fn http_and_https_sightings_merge_and_emit_https() {
        let out = fuse(
            &[
                b(
                    "openalex",
                    vec![h(
                        "Deep Residual Learning",
                        "http://arxiv.org/abs/1512.03385",
                    )],
                ),
                b(
                    "duckduckgo",
                    vec![h(
                        "Deep Residual Learning",
                        "https://www.arxiv.org/abs/1512.03385/",
                    )],
                ),
            ],
            &Weighting::default(),
            5,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://www.arxiv.org/abs/1512.03385");
        assert_eq!(out[0].sources.len(), 2);
    }

    #[test]
    fn digit_guard_uses_decimal_digits_only() {
        // Superscripts / fractions / Roman numerals are NOT `\d` in Python's `_NUM_RE`.
        assert!(digit_runs("m² efficiency").is_empty());
        assert!(digit_runs("part ½ and Ⅱ").is_empty());
        assert_eq!(digit_runs("Python 3.13 release"), ["3", "13"]);
        let out = dedup_by_title(vec![
            h("Area in m² guide for builders", "https://www.example.com/a"),
            h("Area in m³ guide for builders", "https://amp.example.com/b"),
        ]);
        assert_eq!(
            out.len(),
            1,
            "non-decimal superscripts do not trigger the digit guard"
        );
    }
}
