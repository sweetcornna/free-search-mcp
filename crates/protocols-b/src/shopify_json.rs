//! Shopify Storefront JSON protocol adapter (`shopify_json`, A1 §E #17).

use crate::util::{
    check_allowed_dialect_keys, clean_snippet, dialect_str, norm_date, opt_int_range, opt_limit,
    params_map, parse_base_url, parse_json_body, req_str, resolve_http_url, send_request,
    unescape_html, unknown_op,
};
use fsm_core::{
    FsmError, HttpRequest, NetClient, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, SrcCx,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

const KIND: &str = "shopify_json";
const DEFAULT_SUGGEST_TYPES: &str = "product";

static OPS: LazyLock<Vec<OpDecl>> = LazyLock::new(|| {
    vec![
        OpDecl::new(
            "search",
            "Search products on a Shopify storefront (/search/suggest.json).",
            ReturnKind::Records,
        )
        .with_param(Param::new("query", ParamType::String, "Product search query").required())
        .with_param(
            Param::new("limit", ParamType::Int, "Max products returned (1..10)")
                .with_default(json!(10))
                .with_range(1.0, 10.0),
        ),
        OpDecl::new(
            "products",
            "List products on a Shopify storefront (/products.json).",
            ReturnKind::Records,
        )
        .with_param(
            Param::new("limit", ParamType::Int, "Max products per page (1..250)")
                .with_default(json!(20))
                .with_range(1.0, 250.0),
        )
        .with_param(
            Param::new("page", ParamType::Int, "1-based page number")
                .with_default(json!(1))
                .with_range(1.0, 1000.0),
        ),
    ]
});

pub struct ShopifyJsonAdapter {
    net: Arc<dyn NetClient>,
}

impl ShopifyJsonAdapter {
    pub fn new(net: Arc<dyn NetClient>) -> Self {
        Self { net }
    }

    fn suggest_types(dialect: &BTreeMap<String, Value>) -> Result<String, FsmError> {
        let raw = dialect_str(KIND, dialect, "suggest_types")?.unwrap_or(DEFAULT_SUGGEST_TYPES);
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(FsmError::invalid(
                "shopify_json dialect.suggest_types must not be empty",
            ));
        }
        Ok(trimmed.to_owned())
    }

    fn currency(dialect: &BTreeMap<String, Value>) -> Result<Option<String>, FsmError> {
        match dialect_str(KIND, dialect, "currency")? {
            None => Ok(None),
            Some(s) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
            Some(_) => Err(FsmError::invalid(
                "shopify_json dialect.currency must not be empty",
            )),
        }
    }

    fn project_suggest_product(p: &Value, base_url: &str, currency: Option<&str>) -> Value {
        let title = unescape_html(p.get("title").and_then(Value::as_str).unwrap_or(""));
        let raw_url = p.get("url").and_then(Value::as_str).unwrap_or("");
        let url = resolve_http_url(base_url, raw_url).unwrap_or_else(|| raw_url.to_owned());
        let price = p
            .get("price")
            .or_else(|| p.get("price_min"))
            .cloned()
            .unwrap_or(Value::Null);
        let image = p.get("image").and_then(Value::as_str).or_else(|| {
            p.get("featured_image")
                .and_then(|fi| fi.get("url"))
                .and_then(Value::as_str)
        });
        let raw_body = p.get("body").and_then(Value::as_str).unwrap_or("");
        let snippet = clean_snippet(raw_body, 400);

        let mut obj = json!({
            "id": p.get("id"),
            "title": title,
            "url": url,
            "price": price,
            "vendor": p.get("vendor"),
            "handle": p.get("handle"),
            "type": p.get("type"),
            "image": image,
            "snippet": snippet,
        });
        if let Some(curr) = currency
            && let Some(map) = obj.as_object_mut()
        {
            map.insert("currency".to_owned(), Value::String(curr.to_owned()));
        }
        obj
    }

    fn project_catalog_product(p: &Value, base_no_slash: &str, currency: Option<&str>) -> Value {
        let title = unescape_html(p.get("title").and_then(Value::as_str).unwrap_or(""));
        let handle = p.get("handle").and_then(Value::as_str).unwrap_or("");
        let url = if handle.is_empty() {
            base_no_slash.to_owned()
        } else {
            format!("{base_no_slash}/products/{handle}")
        };
        let price = p
            .get("variants")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .and_then(|v0| v0.get("price"))
            .cloned()
            .unwrap_or(Value::Null);
        let image = p
            .get("images")
            .and_then(Value::as_array)
            .and_then(|imgs| imgs.first())
            .and_then(|i0| i0.get("src"))
            .and_then(Value::as_str);
        let raw_body = p.get("body_html").and_then(Value::as_str).unwrap_or("");
        let snippet = clean_snippet(raw_body, 400);
        let date = norm_date(p.get("updated_at").and_then(Value::as_str))
            .or_else(|| norm_date(p.get("published_at").and_then(Value::as_str)));

        let mut obj = json!({
            "id": p.get("id"),
            "title": title,
            "url": url,
            "price": price,
            "vendor": p.get("vendor"),
            "handle": handle,
            "type": p.get("product_type"),
            "image": image,
            "snippet": snippet,
            "date": date,
        });
        if let Some(curr) = currency
            && let Some(map) = obj.as_object_mut()
        {
            map.insert("currency".to_owned(), Value::String(curr.to_owned()));
        }
        obj
    }
}

#[async_trait::async_trait]
impl ProtocolAdapter for ShopifyJsonAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &OPS
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        check_allowed_dialect_keys(KIND, dialect, &["suggest_types", "currency"])?;
        Self::suggest_types(dialect)?;
        Self::currency(dialect)?;
        Ok(())
    }

    async fn call(
        &self,
        base_url: &str,
        op: &str,
        params: Value,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        self.validate_dialect(&cx.dialect)?;
        let (_, base_no_slash) = parse_base_url(self.net.as_ref(), base_url)?;
        let currency = Self::currency(&cx.dialect)?;
        let map = params_map(&params)?;

        match op {
            "search" => {
                let query = req_str(map, "query")?;
                let limit = opt_limit(map, 10, 1, 10)?;
                let suggest_types = Self::suggest_types(&cx.dialect)?;

                let req = HttpRequest::get(format!("{base_no_slash}/search/suggest.json"))
                    .header("Accept", "application/json")
                    .query_param("q", query)
                    .query_param("resources[type]", suggest_types)
                    .query_param("resources[limit]", limit.to_string());
                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let body = parse_json_body(&resp, KIND)?;

                let results = body
                    .get("resources")
                    .and_then(|r| r.get("results"))
                    .ok_or_else(|| {
                        FsmError::upstream(
                            KIND,
                            Some(resp.status),
                            "Shopify suggest.json missing resources.results object",
                        )
                    })?;

                let mut items = Vec::new();
                if let Some(products) = results.get("products").and_then(Value::as_array) {
                    for p in products.iter().take(limit) {
                        items.push(Self::project_suggest_product(
                            p,
                            &base_no_slash,
                            currency.as_deref(),
                        ));
                    }
                }
                for bucket in ["collections", "pages", "articles"] {
                    if items.len() >= limit {
                        break;
                    }
                    if let Some(rows) = results.get(bucket).and_then(Value::as_array) {
                        for row in rows {
                            if items.len() >= limit {
                                break;
                            }
                            items.push(Self::project_suggest_product(
                                row,
                                &base_no_slash,
                                currency.as_deref(),
                            ));
                        }
                    }
                }

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = items;
                Ok(out)
            }
            "products" => {
                let limit = opt_limit(map, 20, 1, 250)?;
                let page = opt_int_range(map, "page", 1, 1000)?.unwrap_or(1);

                let req = HttpRequest::get(format!("{base_no_slash}/products.json"))
                    .header("Accept", "application/json")
                    .query_param("limit", limit.to_string())
                    .query_param("page", page.to_string());
                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let body = parse_json_body(&resp, KIND)?;
                let products = body
                    .get("products")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        FsmError::upstream(
                            KIND,
                            Some(resp.status),
                            "Shopify /products.json missing 'products' array",
                        )
                    })?;

                let items: Vec<Value> = products
                    .iter()
                    .take(limit)
                    .map(|p| Self::project_catalog_product(p, &base_no_slash, currency.as_deref()))
                    .collect();

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                if items.len() == limit && page < 1000 {
                    out.next_cursor = Some(page.saturating_add(1).to_string());
                }
                out.items = items;
                Ok(out)
            }
            _ => Err(unknown_op(KIND, op, &["search", "products"])),
        }
    }
}
