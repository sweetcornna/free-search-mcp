//! The exec boundary: classify the script's result and enforce rendering
//! guardrails 2 and 3 (`SPEC.md` §2.5.2) on the Rust side.
//!
//! Guardrail 2 ("no silent field loss") is checked against the `fsm-core`
//! serde types themselves: the value must deserialize into the type for its
//! [`RenderKind`] and serialize back without losing any key or changing any
//! value (`null` counts as absent). That rejects custom keys at any depth
//! (`p.myNote = 1`, `brief.pages[0].x = 1`), missing required fields and
//! wrong types, and guarantees the renderer can `from_value` what it gets.
//! Guardrail 3 (element-level arrays) applies the same check to every element
//! of a non-empty array; the JS prelude already required every element to
//! carry the same host tag.

use crate::engine::ExecFailure;
use fsm_core::{
    AskResponse, BinaryHandle, CallCx, CatalogHit, CitationGraph, DocSlice, FsmError,
    FulltextResult, InlineImageMeta, PageResult, RenderKind, ResearchBrief, ScholarlyWork,
    SearchResponse, WorkResult,
};
use serde::Serialize;
use serde::de::value::BorrowedStrDeserializer;
use serde::de::{self, DeserializeOwned, DeserializeSeed, Visitor};
use serde_json::Value;

/// What the JS side handed back: the result's JSON text (`None` for
/// `undefined`, functions, symbols), its candidate render tag and image id.
#[derive(Debug, Default)]
pub(crate) struct RawValue {
    pub json: Option<String>,
    pub kind: Option<String>,
    pub image: Option<String>,
}

/// The script's result, classified for rendering.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ExecValue {
    /// The script returned nothing (`undefined`, a function, a symbol).
    Undefined,
    /// A top-level string: shown verbatim.
    Text(String),
    /// Anything else, shown as compact JSON.
    Json(Value),
    /// A host-tagged value that passed guardrails 1-3; render it as Markdown
    /// under `kind` (see [`ExecValue::render_input`]).
    Tagged { kind: RenderKind, value: Value },
    /// An inline image: metadata JSON plus the host-side bytes.
    Image { meta: Value, handle: BinaryHandle },
}

impl ExecValue {
    /// The raw structured JSON of the result (guardrail 4: what overflow stores,
    /// never the rendered Markdown).
    pub fn raw_json(&self) -> Value {
        match self {
            Self::Undefined => Value::Null,
            Self::Text(s) => Value::String(s.clone()),
            Self::Json(v) | Self::Tagged { value: v, .. } => v.clone(),
            Self::Image { meta, .. } => meta.clone(),
        }
    }

    /// The renderer's input for [`ExecValue::Tagged`]: `(kind, json)`. A lone
    /// work or catalog hit (there is no single-item kind) becomes a one-element
    /// `WorkList` / `CatalogList` array.
    pub fn render_input(&self) -> Option<(RenderKind, Value)> {
        match self {
            Self::Tagged { kind, value } => match (kind, value) {
                (RenderKind::WorkList | RenderKind::CatalogList, Value::Object(_)) => {
                    Some((*kind, Value::Array(vec![value.clone()])))
                }
                _ => Some((*kind, value.clone())),
            },
            Self::Image { meta, .. } => Some((RenderKind::InlineImage, meta.clone())),
            _ => None,
        }
    }
}

pub(crate) async fn classify(raw: RawValue, cx: &CallCx) -> Result<ExecValue, ExecFailure> {
    let Some(text) = raw.json else {
        return Ok(ExecValue::Undefined);
    };
    let value: Value = serde_json::from_str(&text).map_err(|e| {
        let why = if e.to_string().contains("recursion limit") {
            "nests deeper than 128 levels".to_owned()
        } else {
            format!("is not valid JSON ({e})")
        };
        ExecFailure::new(FsmError::invalid(format!(
            "the exec result {why}; return flatter data or a summary"
        )))
    })?;
    if let Value::String(s) = value {
        return Ok(ExecValue::Text(s));
    }
    let Some(kind) = raw.kind.as_deref().and_then(RenderKind::from_tag) else {
        return Ok(ExecValue::Json(value));
    };
    if kind == RenderKind::InlineImage {
        if let Some(id) = raw.image
            && validate_tagged(kind, &value)
            && let Some(handle) = cx.images.lock().await.get(&id).cloned()
        {
            return Ok(ExecValue::Image {
                meta: value,
                handle,
            });
        }
        return Ok(ExecValue::Json(value));
    }
    Ok(if validate_tagged(kind, &value) {
        ExecValue::Tagged { kind, value }
    } else {
        ExecValue::Json(value)
    })
}

/// Guardrails 2 and 3 for a value claiming `kind`: it (or every element of a
/// non-empty array, for list kinds) must round-trip losslessly through the
/// `fsm-core` type of that kind. `WorkList` and `CatalogList` also accept a
/// single object.
pub fn validate_tagged(kind: RenderKind, value: &Value) -> bool {
    match kind {
        RenderKind::PageResult => lossless::<PageResult>(value),
        RenderKind::PageList => list::<PageResult>(value, false),
        RenderKind::DocSlice => lossless::<DocSlice>(value),
        RenderKind::FulltextResult => lossless::<FulltextResult>(value),
        RenderKind::SearchResponse => lossless::<SearchResponse>(value),
        RenderKind::WorkResult => lossless::<WorkResult>(value),
        RenderKind::WorkList => list::<ScholarlyWork>(value, true),
        RenderKind::ResearchBrief => lossless::<ResearchBrief>(value),
        RenderKind::CitationGraph => lossless::<CitationGraph>(value),
        RenderKind::CatalogList => list::<CatalogHit>(value, true),
        RenderKind::InlineImage => lossless::<InlineImageMeta>(value),
        RenderKind::AskResponse => lossless::<AskResponse>(value),
        _ => false,
    }
}

fn list<T: DeserializeOwned + Serialize>(value: &Value, single_ok: bool) -> bool {
    match value {
        Value::Array(items) => !items.is_empty() && items.iter().all(lossless::<T>),
        Value::Object(_) if single_ok => lossless::<T>(value),
        _ => false,
    }
}

fn lossless<T: DeserializeOwned + Serialize>(value: &Value) -> bool {
    if !value.is_object() {
        return false;
    }
    let Ok(typed) = T::deserialize(Strict(value)) else {
        return false;
    };
    serde_json::to_value(&typed).is_ok_and(|back| covered(value, &back))
}

/// A `&Value` deserializer that refuses unknown struct fields at any depth.
/// The fsm-core types do not `deny_unknown_fields`, and derived `Deserialize`
/// skips an unknown field through `deserialize_ignored_any`, so that is where
/// it is refused. Without this, a custom key set to `null` would pass the
/// round trip (`null` = absent) and vanish from the render.
struct Strict<'de>(&'de Value);

impl<'de> de::Deserializer<'de> for Strict<'de> {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Array(items) => visitor.visit_seq(StrictSeq(items.iter())),
            Value::Object(map) => visitor.visit_map(StrictMap {
                iter: map.iter(),
                value: None,
            }),
            other => other.deserialize_any(visitor),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // The rendered types only hold unit enums (`FetchMethod`, `HitKind`).
        self.0.deserialize_enum(name, variants, visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(
        self,
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom("unknown field"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map struct identifier
    }
}

struct StrictSeq<'de>(std::slice::Iter<'de, Value>);

impl<'de> de::SeqAccess<'de> for StrictSeq<'de> {
    type Error = serde_json::Error;

    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, Self::Error> {
        self.0
            .next()
            .map(|v| seed.deserialize(Strict(v)))
            .transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

struct StrictMap<'de> {
    iter: serde_json::map::Iter<'de>,
    value: Option<&'de Value>,
}

impl<'de> de::MapAccess<'de> for StrictMap<'de> {
    type Error = serde_json::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        let Some((key, value)) = self.iter.next() else {
            return Ok(None);
        };
        self.value = Some(value);
        seed.deserialize(BorrowedStrDeserializer::new(key.as_str()))
            .map(Some)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<S::Value, Self::Error> {
        let value = self
            .value
            .take()
            .ok_or_else(|| de::Error::custom("value without a key"))?;
        seed.deserialize(Strict(value))
    }
}

/// Every key and value of `orig` survives in `back` (`null` = absent,
/// numbers compare by value so `1` and `1.0` match).
fn covered(orig: &Value, back: &Value) -> bool {
    match (orig, back) {
        (Value::Object(o), Value::Object(b)) => o.iter().all(|(k, ov)| match b.get(k) {
            Some(bv) => covered(ov, bv),
            None => ov.is_null(),
        }),
        (Value::Array(o), Value::Array(b)) => {
            o.len() == b.len() && o.iter().zip(b).all(|(x, y)| covered(x, y))
        }
        (Value::Number(x), Value::Number(y)) => x == y || x.as_f64() == y.as_f64(),
        (x, y) => x == y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsm_core::{AuthorRef, FetchMethod, OaLocation, WorkIds};
    use serde_json::json;

    fn page() -> PageResult {
        let mut p = PageResult::default();
        p.url = "https://example.com/a".into();
        p.title = "A".into();
        p.content = "body".into();
        p.method = FetchMethod::Http;
        p.retrieved_at = "2026-11-25T00:00:00Z".into();
        p
    }

    /// Ties the guardrail to serde: a Default value serializes to exactly the
    /// required keys, a fully populated one to every key, and both pass.
    #[test]
    fn key_sets_come_from_serde() {
        let minimal = serde_json::to_value(PageResult::default()).unwrap();
        let keys: Vec<&str> = minimal
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "url",
                "title",
                "content",
                "method",
                "tokensEstimated",
                "totalChars",
                "truncated",
                "retrievedAt"
            ]
        );
        assert!(validate_tagged(RenderKind::PageResult, &minimal));
        let mut full = page();
        full.published_date = Some("2026-01-01".into());
        full.author = Some("x".into());
        full.sitename = Some("s".into());
        full.media_type = Some("text/html".into());
        full.bytes = Some(10);
        full.width = Some(1);
        full.height = Some(2);
        full.sha256 = Some("ab".into());
        full.cache_age_s = Some(3);
        let full = serde_json::to_value(full).unwrap();
        assert_eq!(full.as_object().unwrap().len(), 17);
        assert!(validate_tagged(RenderKind::PageResult, &full));
        // Every key the type knows passes, any other key fails.
        let mut extra = full.clone();
        extra["myNote"] = json!("x");
        assert!(!validate_tagged(RenderKind::PageResult, &extra));
    }

    #[test]
    fn guardrail_two_rejects_loss_and_type_changes() {
        let p = serde_json::to_value(page()).unwrap();
        let mut v = p.clone();
        v["content"] = json!("edited text is fine");
        assert!(validate_tagged(RenderKind::PageResult, &v));
        v["author"] = Value::Null; // null == absent
        assert!(validate_tagged(RenderKind::PageResult, &v));
        let mut missing = p.clone();
        missing.as_object_mut().unwrap().remove("title");
        assert!(!validate_tagged(RenderKind::PageResult, &missing));
        let mut wrong = p.clone();
        wrong["totalChars"] = json!("12");
        assert!(!validate_tagged(RenderKind::PageResult, &wrong));
        let mut bad_enum = p.clone();
        bad_enum["method"] = json!("HTTP");
        assert!(!validate_tagged(RenderKind::PageResult, &bad_enum));
        assert!(!validate_tagged(
            RenderKind::PageResult,
            &json!([p.clone()])
        ));
        assert!(!validate_tagged(RenderKind::PageResult, &json!("x")));
    }

    #[test]
    fn nested_custom_keys_and_lists() {
        let p = serde_json::to_value(page()).unwrap();
        let mut brief = serde_json::to_value(ResearchBrief::default()).unwrap();
        brief["pages"] = json!([p.clone(), p.clone()]);
        assert!(validate_tagged(RenderKind::ResearchBrief, &brief));
        brief["pages"][1]["myNote"] = json!(1);
        assert!(
            !validate_tagged(RenderKind::ResearchBrief, &brief),
            "nested custom key"
        );
        assert!(validate_tagged(
            RenderKind::PageList,
            &json!([p.clone(), p.clone()])
        ));
        assert!(!validate_tagged(RenderKind::PageList, &json!([])));
        assert!(
            !validate_tagged(RenderKind::PageList, &p),
            "a lone page is PageResult, not PageList"
        );
        let mut w = ScholarlyWork::default();
        w.title = "T".into();
        w.authors = vec![AuthorRef::default()];
        w.ids = WorkIds::default();
        w.oa = Some(OaLocation::default());
        w.url = "https://doi.org/10.1/x".into();
        let w = serde_json::to_value(w).unwrap();
        assert!(validate_tagged(RenderKind::WorkList, &w), "single work");
        assert!(validate_tagged(RenderKind::WorkList, &json!([w.clone()])));
        let mut hit = serde_json::to_value(CatalogHit::default()).unwrap();
        assert!(validate_tagged(RenderKind::CatalogList, &hit));
        hit["tier"] = json!(1.5);
        assert!(!validate_tagged(RenderKind::CatalogList, &hit), "u8 field");
        // Numbers compare by value: an integer where the type holds f64.
        let mut sr = serde_json::to_value(SearchResponse::default()).unwrap();
        sr["results"] = json!([{"title": "t", "url": "u", "score": 1}]);
        assert!(validate_tagged(RenderKind::SearchResponse, &sr));
    }

    /// Review round 1, I1: serde ignores unknown keys, so a custom key set to
    /// `null` used to pass the round trip and vanish from the render.
    #[test]
    fn r1_i1_custom_null_keys_are_not_silently_dropped() {
        let p = serde_json::to_value(page()).unwrap();
        let mut v = p.clone();
        v["myNote"] = Value::Null;
        assert!(!validate_tagged(RenderKind::PageResult, &v));
        let mut brief = serde_json::to_value(ResearchBrief::default()).unwrap();
        brief["pages"] = json!([p.clone()]);
        assert!(validate_tagged(RenderKind::ResearchBrief, &brief));
        brief["pages"][0]["extracted"] = Value::Null;
        assert!(!validate_tagged(RenderKind::ResearchBrief, &brief));
        let mut known = p.clone();
        known["author"] = Value::Null;
        assert!(validate_tagged(RenderKind::PageResult, &known));
        // Free-form JSON fields (Hit.extra) keep their nulls and stay valid.
        let mut sr = serde_json::to_value(SearchResponse::default()).unwrap();
        sr["results"] =
            json!([{"title": "t", "url": "u", "score": 1, "extra": {"k": null, "n": [1, null]}}]);
        assert!(validate_tagged(RenderKind::SearchResponse, &sr));
        let mut hit = serde_json::to_value(CatalogHit::default()).unwrap();
        hit["custom"] = Value::Null;
        assert!(!validate_tagged(RenderKind::CatalogList, &json!([hit])));
    }

    #[test]
    fn render_input_wraps_single_items() {
        let v = ExecValue::Tagged {
            kind: RenderKind::WorkList,
            value: json!({"title": "t"}),
        };
        assert_eq!(
            v.render_input(),
            Some((RenderKind::WorkList, json!([{"title": "t"}])))
        );
        assert_eq!(v.raw_json(), json!({"title": "t"}));
        assert_eq!(ExecValue::Undefined.render_input(), None);
        assert_eq!(ExecValue::Text("x".into()).raw_json(), json!("x"));
    }
}
