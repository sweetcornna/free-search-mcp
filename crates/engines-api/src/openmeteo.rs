//! Open-Meteo weather forecast engine (geocoding + 3-day forecast, keyless JSON API).

use crate::common::{
    CallBudget, clip, collapse_ws, make_hit, parse_search_params, search_op, send_json_keyless,
};
use crate::facts::query_is_chinese;
use async_trait::async_trait;
use fsm_core::{
    AuthMode, Capability, FsmError, Hit, HitKind, HttpRequest, PoolRole, ProbeSpec, Source,
    SourceDecl, SourceDeps, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

const GEOCODE_ENDPOINT: &str = "https://geocoding-api.open-meteo.com/v1/search";
const FORECAST_ENDPOINT: &str = "https://api.open-meteo.com/v1/forecast";

static WEATHER_WORDS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(weather|forecast|temperature|temperatures|rain|snow|wind|humidity|today|tomorrow|tonight|this week|weekend|in|at|for|the|what|is|will|it|be|like|of|hourly|daily|current|now)\b|天气|预报|气温|温度|降雨|下雨|下雪|风力|湿度|今天|明天|后天|今晚|本周|周末|未来|几天|一周|怎么样|如何|查|一下|的|吗|现在",
    )
    .expect("regex")
});

static PUNCT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[?？,，。!！]").expect("regex"));

static WEATHER_ASK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(weather|forecast|temperature|rain|snow|humidity|wind speed)\b|天气|预报|气温|温度|下雨|下雪|湿度|风力",
    )
    .expect("regex")
});

fn wmo_sky(code: Option<&Value>, zh: bool) -> String {
    let parsed = code.and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    });
    let pair = match parsed {
        Some(0) => Some(("clear sky", "晴")),
        Some(1) => Some(("mainly clear", "大部晴朗")),
        Some(2) => Some(("partly cloudy", "局部多云")),
        Some(3) => Some(("overcast", "阴")),
        Some(45) => Some(("fog", "雾")),
        Some(48) => Some(("rime fog", "雾（结霜）")),
        Some(51) => Some(("light drizzle", "小毛毛雨")),
        Some(53) => Some(("drizzle", "毛毛雨")),
        Some(55) => Some(("dense drizzle", "大毛毛雨")),
        Some(61) => Some(("light rain", "小雨")),
        Some(63) => Some(("rain", "中雨")),
        Some(65) => Some(("heavy rain", "大雨")),
        Some(66) => Some(("freezing rain", "冻雨")),
        Some(67) => Some(("heavy freezing rain", "强冻雨")),
        Some(71) => Some(("light snow", "小雪")),
        Some(73) => Some(("snow", "中雪")),
        Some(75) => Some(("heavy snow", "大雪")),
        Some(77) => Some(("snow grains", "米雪")),
        Some(80) => Some(("light showers", "小阵雨")),
        Some(81) => Some(("showers", "阵雨")),
        Some(82) => Some(("violent showers", "强阵雨")),
        Some(85) => Some(("snow showers", "阵雪")),
        Some(86) => Some(("heavy snow showers", "强阵雪")),
        Some(95) => Some(("thunderstorm", "雷暴")),
        Some(96) => Some(("thunderstorm with hail", "雷暴伴冰雹")),
        Some(99) => Some(("thunderstorm with heavy hail", "雷暴伴大冰雹")),
        _ => None,
    };
    match pair {
        Some((en, cn)) => (if zh { cn } else { en }).to_owned(),
        None => match code {
            Some(Value::Null) | None => "code None".to_owned(),
            Some(Value::String(s)) => format!("code {s}"),
            Some(other) => format!("code {other}"),
        },
    }
}

fn json_scalar(val: Option<&Value>) -> String {
    match val {
        None | Some(Value::Null) => "None".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Remove weather filler words and punctuation to extract the place name.
pub fn place_from(query: &str) -> String {
    let stripped = WEATHER_WORDS_RE.replace_all(query, " ");
    let clean = PUNCT_RE.replace_all(&stripped, " ");
    collapse_ws(&clean)
}

/// Open-Meteo current conditions and 3-day weather forecast engine.
pub struct OpenMeteoEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl OpenMeteoEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "openmeteo",
            "Open-Meteo",
            "Open-Meteo: current conditions and a 3-day forecast for a named place.",
        )
        .with_scopes(&["reference"])
        .with_caps(&[Capability::Search, Capability::Lookup])
        .with_op(
            search_op(
                "Geocode a place and return current weather plus a 3-day forecast.",
                5,
                r#"source.call("openmeteo.search", { query: "weather in Berlin tomorrow" })"#,
            )
            .with_probe(ProbeSpec::new(
                json!({"query": "weather in Berlin tomorrow"}),
                1,
            )),
        );
        decl.aliases = vec![
            "weather".to_owned(),
            "forecast".to_owned(),
            "天气".to_owned(),
        ];
        decl.home_url = Some("https://open-meteo.com".to_owned());
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Plain;
        decl.pool_role = PoolRole::DirectClaimant;
        let mut limits = SourceLimits::new(2.0, 4, 2, 6000, 3600);
        limits.host_bucket = Some("open-meteo.com".to_owned());
        decl.limits = limits;
        Self { decl, deps }
    }

    /// Map a forecast payload + place metadata into hits.
    pub fn map_forecast(
        forecast: &Value,
        spot: &serde_json::Map<String, Value>,
        url: &str,
        zh: bool,
    ) -> Vec<Hit> {
        let Some(obj) = forecast.as_object() else {
            return Vec::new();
        };
        let name = ["name", "admin1", "country"]
            .into_iter()
            .filter_map(|k| spot.get(k).and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(", ");
        let current = obj.get("current").and_then(Value::as_object);
        let daily = obj.get("daily").and_then(Value::as_object);
        let issued = current
            .and_then(|c| c.get("time"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let tz = obj.get("timezone").and_then(Value::as_str).unwrap_or("");

        let mut bits = Vec::new();
        if let Some(c) = current
            && !c.is_empty()
        {
            let sky = wmo_sky(c.get("weather_code"), zh);
            let temp = json_scalar(c.get("temperature_2m"));
            let hum = json_scalar(c.get("relative_humidity_2m"));
            let wind = json_scalar(c.get("wind_speed_10m"));
            if zh {
                bits.push(format!(
                    "现在 {temp}°C，{sky}，湿度 {hum}%，风速 {wind} km/h（{issued} {tz}）"
                ));
            } else {
                bits.push(format!(
                    "Now {temp}°C, {sky}, humidity {hum}%, wind {wind} km/h (observed {issued} {tz})"
                ));
            }
        }

        if let Some(d) = daily
            && let Some(days) = d.get("time").and_then(Value::as_array)
        {
            let codes = d.get("weather_code").and_then(Value::as_array);
            let highs = d.get("temperature_2m_max").and_then(Value::as_array);
            let lows = d.get("temperature_2m_min").and_then(Value::as_array);
            let probs = d
                .get("precipitation_probability_max")
                .and_then(Value::as_array);
            let sums = d.get("precipitation_sum").and_then(Value::as_array);
            for (i, day_val) in days.iter().take(3).enumerate() {
                let Some(day) = day_val.as_str() else {
                    continue;
                };
                let (Some(code), Some(hi), Some(lo), Some(prob), Some(mm)) = (
                    codes.and_then(|a| a.get(i)),
                    highs.and_then(|a| a.get(i)),
                    lows.and_then(|a| a.get(i)),
                    probs.and_then(|a| a.get(i)),
                    sums.and_then(|a| a.get(i)),
                ) else {
                    continue;
                };
                let sky = wmo_sky(Some(code), zh);
                let lo_s = json_scalar(Some(lo));
                let hi_s = json_scalar(Some(hi));
                let prob_s = json_scalar(Some(prob));
                let mm_s = json_scalar(Some(mm));
                if zh {
                    bits.push(format!(
                        "{day}：{sky}，{lo_s}到{hi_s}°C，降水概率 {prob_s}%，降水量 {mm_s} mm"
                    ));
                } else {
                    bits.push(format!(
                        "{day}: {sky}, {lo_s} to {hi_s}°C, {prob_s}% chance of precipitation, {mm_s} mm"
                    ));
                }
            }
        }

        if bits.is_empty() {
            return Vec::new();
        }
        let title = if zh {
            format!("{name} 天气预报（Open-Meteo）")
        } else {
            format!("Weather forecast for {name} (Open-Meteo)")
        };
        let issued_day: String = issued.chars().take(10).collect();
        vec![make_hit(
            "openmeteo",
            title,
            if url.is_empty() {
                FORECAST_ENDPOINT.to_owned()
            } else {
                url.to_owned()
            },
            clip(&bits.join(" · "), 700),
            (!issued_day.is_empty()).then_some(issued_day),
            Some("api"),
            HitKind::Record,
        )]
    }
}

#[async_trait]
impl Source for OpenMeteoEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        if WEATHER_ASK_RE.is_match(query) && !place_from(query).is_empty() {
            1.0
        } else {
            0.0
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params("openmeteo", op, &params, 5)?;
        let place = place_from(query);
        if place.is_empty() {
            return Ok(SrcOutput::Hits(Vec::new()));
        }
        let zh = query_is_chinese(query);
        let lang = if zh { "zh" } else { "en" };
        let budget = CallBudget::from_cx(cx, &self.decl);

        let geo_req = HttpRequest::get(GEOCODE_ENDPOINT)
            .header("Accept", "application/json")
            .query_param("name", &place)
            .query_param("count", "1")
            .query_param("language", lang)
            .for_source("openmeteo");

        let Some(geo) = send_json_keyless(
            self.deps.net.as_ref(),
            geo_req,
            self.decl.transport,
            &self.decl,
            cx,
            &budget,
        )
        .await?
        else {
            return Ok(SrcOutput::Hits(Vec::new()));
        };

        let Some(spot) = geo
            .get("results")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(Value::as_object)
        else {
            return Ok(SrcOutput::Hits(Vec::new()));
        };

        let (Some(lat), Some(lon)) = (
            spot.get("latitude").and_then(Value::as_f64),
            spot.get("longitude").and_then(Value::as_f64),
        ) else {
            return Ok(SrcOutput::Hits(Vec::new()));
        };

        let forecast_url = format!(
            "{FORECAST_ENDPOINT}?latitude={lat:.4}&longitude={lon:.4}&current=temperature_2m,relative_humidity_2m,weather_code,wind_speed_10m&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,precipitation_sum&timezone=auto&forecast_days=3"
        );
        let forecast_req = HttpRequest::get(&forecast_url)
            .header("Accept", "application/json")
            .for_source("openmeteo");

        let Some(forecast) = send_json_keyless(
            self.deps.net.as_ref(),
            forecast_req,
            self.decl.transport,
            &self.decl,
            cx,
            &budget,
        )
        .await?
        else {
            return Ok(SrcOutput::Hits(Vec::new()));
        };

        let mut results = Self::map_forecast(&forecast, spot, &forecast_url, zh);
        results.truncate(limit);
        Ok(SrcOutput::Hits(results))
    }
}
