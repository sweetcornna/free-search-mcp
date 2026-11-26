//! Extension point of lane `W1a` — Search Kernel & `eval-replay` (`fsm-search`).
//!
//! Only that lane edits this file.
//!
//! # Wall convention (engines → search kernel)
//!
//! `Source::call` returns hits or an error and has no side channel for "the
//! upstream served a captcha / consent / login wall instead of results", and
//! [`FsmError`] is frozen. So a wall travels as an ordinary
//! [`FsmError::Upstream`] whose message starts with [`WALL_PREFIX`]:
//!
//! * engines build it with [`wall_error`] (`"wall:<reason>"`), or with the
//!   `"<id> was <reason>-gated"` message `fsm-engines-serp` uses;
//! * the search kernel reads either back with [`wall_reason`], benches the
//!   source (one strike for a deliberate wall, `SPEC.md` §5) and reports it
//!   in `SearchResponse.notes`.
//!
//! A wall is declared **only when the page parsed to zero results** (Spike N
//! §4.8: Naver's normal results page carries `h-captcha`, Seznam's
//! `CONF_RECAPTCHA_KEY`, Brave's i18n strings "before you continue"). Use
//! [`detect_gate`], which enforces that and checks the engine's own markers
//! before the shared ones.

use crate::error::FsmError;

/// Prefix of an [`FsmError::Upstream`] message that reports a wall.
pub const WALL_PREFIX: &str = "wall:";

/// Shared wall markers (lower-case substrings), checked after an engine's
/// own; the first reason whose marker matches wins. Ported from
/// `engines/base.py` `_GATE_MARKERS`, plus the walls Spike N recorded
/// (360 `请输入验证码`, Yandex `showcaptcha`, Quark `_____tmd_____`).
pub const GATE_MARKERS: &[(&str, &[&str])] = &[
    (
        "captcha",
        &[
            "/sorry/index",
            "unusual traffic",
            "/recaptcha/",
            "g-recaptcha",
            "h-captcha",
            "captcha-delivery",
            "px-captcha",
            "are you a robot",
            "verify you are a human",
            "anomaly-modal",
            "made by a human",
            "captcha-wrap",
            "altcha",
            "verification required",
            "complete the challenge",
            "请输入验证码",
            "showcaptcha",
            "_____tmd_____",
        ],
    ),
    (
        "javascript",
        &[
            "if you are not redirected",
            "please click here if you are not",
            "enable javascript to continue",
            "javascript is required",
        ],
    ),
    (
        "consent",
        &[
            "consent.google.com",
            "consent.youtube.com",
            "before you continue",
            "consent.bing.com",
        ],
    ),
    (
        "login",
        &[
            "请登录",
            "登录知乎",
            "signflow",
            "sign in to continue",
            "you must log in",
            "please log in to continue",
        ],
    ),
];

/// The wall reason of a page, or `None`.
///
/// Returns `None` whenever `parsed_results > 0`: a page that yielded results
/// is never a wall, whatever markers its markup happens to contain.
/// `engine_markers` (the engine's own `(reason, markers)` pairs) are checked
/// before [`GATE_MARKERS`]. Markers are matched case-insensitively.
pub fn detect_gate(
    body: &str,
    parsed_results: usize,
    engine_markers: &[(&str, &[&str])],
) -> Option<String> {
    if parsed_results > 0 || body.is_empty() {
        return None;
    }
    let low = body.to_lowercase();
    engine_markers
        .iter()
        .chain(GATE_MARKERS.iter())
        .find(|(_, markers)| markers.iter().any(|m| low.contains(&m.to_lowercase())))
        .map(|(reason, _)| (*reason).to_owned())
}

/// The error an engine returns when it was served a wall instead of results.
///
/// `reason` is one word (`captcha`, `consent`, `login`, `javascript`,
/// `region`, or an engine-specific one); `status` is the HTTP status of the
/// wall page when known.
pub fn wall_error(source_id: &str, reason: &str, status: Option<u16>) -> FsmError {
    FsmError::Upstream {
        source_id: source_id.to_owned(),
        status,
        message: format!("{WALL_PREFIX}{reason}"),
    }
}

/// Suffix of the `"<id> was <reason>-gated"` wall message.
pub const GATED_SUFFIX: &str = "-gated";

/// The wall reason an [`FsmError::Upstream`] carries: `"wall:captcha"` and
/// `"wall:captcha: detail"` (built by [`wall_error`]) and
/// `"bing was captcha-gated"` all give `"captcha"`.
pub fn wall_reason(err: &FsmError) -> Option<&str> {
    let FsmError::Upstream { message, .. } = err else {
        return None;
    };
    let reason = match message.strip_prefix(WALL_PREFIX) {
        Some(rest) => rest.split([':', ' ']).next(),
        None => message
            .trim_end()
            .strip_suffix(GATED_SUFFIX)
            .and_then(|head| head.rsplit_once(" was "))
            .map(|(_, r)| r),
    };
    reason
        .map(str::trim)
        .filter(|r| !r.is_empty() && !r.contains(char::is_whitespace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walls_only_on_zero_results_and_engine_markers_first() {
        let page = "<div class=\"h-captcha\"></div> before you continue";
        assert_eq!(detect_gate(page, 0, &[]).as_deref(), Some("captcha"));
        assert_eq!(detect_gate(page, 3, &[]), None, "results beat markers");
        let own: &[(&str, &[&str])] = &[("region", &["before you continue"])];
        assert_eq!(detect_gate(page, 0, own).as_deref(), Some("region"));
        assert_eq!(detect_gate("", 0, &[]), None);
        assert_eq!(detect_gate("<p>no results for rust</p>", 0, &[]), None);
        assert_eq!(
            detect_gate("访问异常页面 请输入验证码", 0, &[]).as_deref(),
            Some("captcha")
        );
    }

    #[test]
    fn wall_errors_round_trip() {
        let e = wall_error("bing", "captcha", Some(200));
        assert_eq!(wall_reason(&e), Some("captcha"));
        assert!(matches!(
            e,
            FsmError::Upstream {
                status: Some(200),
                ..
            }
        ));
        let detailed = FsmError::upstream("ddg", Some(202), "wall:captcha: anomaly-modal");
        assert_eq!(wall_reason(&detailed), Some("captcha"));
        assert_eq!(wall_reason(&FsmError::upstream("ddg", None, "boom")), None);
        assert_eq!(wall_reason(&FsmError::upstream("ddg", None, "wall:")), None);
        assert_eq!(wall_reason(&FsmError::Timeout(5)), None);
    }

    #[test]
    fn the_serp_engines_gated_message_is_a_wall_too() {
        let e = FsmError::upstream("so360", Some(200), "so360 was captcha-gated");
        assert_eq!(wall_reason(&e), Some("captcha"));
        let e = FsmError::upstream("quark", Some(200), "quark was x5sec-gated");
        assert_eq!(wall_reason(&e), Some("x5sec"));
        let e = FsmError::upstream("bing", Some(503), "bing: HTTP 503");
        assert_eq!(wall_reason(&e), None);
        let e = FsmError::upstream("x", None, "x was -gated");
        assert_eq!(wall_reason(&e), None);
    }
}
