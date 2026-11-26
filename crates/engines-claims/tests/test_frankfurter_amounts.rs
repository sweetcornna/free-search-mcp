//! Port of `tests/test_frankfurter_amounts.py`: currency amounts must survive
//! parsing and conversion without silent rescaling.

use fsm_core::fixture::{Exchange, Fixture, FixtureRequest, FixtureResponse, ReplayNet};
use fsm_core::{Config, NullStore, Source, SourceDeps, SrcCx, SrcOutput};
use fsm_engines_claims::frankfurter::{CURRENCIES, FrankfurterEngine, MORE_CURRENCIES, parse_pair};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test]
fn test_currency_code_is_not_a_thousands_suffix() {
    let k_codes: BTreeSet<&str> = CURRENCIES
        .iter()
        .chain(MORE_CURRENCIES.iter())
        .copied()
        .filter(|c| c.starts_with('K'))
        .collect();
    assert!(!k_codes.is_empty());
    for currency in k_codes {
        let q = format!("100 {currency} to USD");
        assert_eq!(
            parse_pair(&q),
            Some((currency.to_owned(), "USD".to_owned(), 100.0)),
            "{q}"
        );
    }
}

#[test]
fn test_currency_name_is_not_a_thousands_suffix() {
    for (name, expected) in [
        ("krona", "SEK"),
        ("kronor", "SEK"),
        ("krone", "NOK"),
        ("kroner", "NOK"),
    ] {
        let q = format!("100 {name} to USD");
        assert_eq!(
            parse_pair(&q),
            Some((expected.to_owned(), "USD".to_owned(), 100.0)),
            "{q}"
        );
    }
}

#[test]
fn test_grouped_amounts_and_explicit_multipliers() {
    for (amount, expected) in [
        ("1,000,000", 1_000_000.0),
        ("12,345,678.90", 12_345_678.90),
        ("1,000", 1_000.0),
        ("2.5k", 2_500.0),
        ("2.5 K", 2_500.0),
        ("2 thousand", 2_000.0),
        ("2 MILLION", 2_000_000.0),
        ("1万", 10_000.0),
        ("1亿", 100_000_000.0),
    ] {
        let q = format!("{amount} USD to CNY");
        assert_eq!(
            parse_pair(&q),
            Some(("USD".to_owned(), "CNY".to_owned(), expected)),
            "{q}"
        );
    }
}

#[test]
fn test_amount_boundaries_do_not_discard_value_or_sign() {
    for (query, expected) in [
        ("2kUSD to CNY", 2_000.0),
        ("2Kusd to CNY", 2_000.0),
        ("把100美元换成人民币", 100.0),
        ("0.00001 USD to CNY", 0.00001),
        ("-100 USD to CNY", -100.0),
        ("+100 USD to CNY", 100.0),
        (".5 USD to CNY", 0.5),
    ] {
        assert_eq!(
            parse_pair(query),
            Some(("USD".to_owned(), "CNY".to_owned(), expected)),
            "{query}"
        );
    }
}

#[test]
fn test_sentence_punctuation_is_not_part_of_the_amount() {
    for query in [
        "Convert USD to CNY: 100.",
        "Convert 100, please, from USD to CNY",
        "USD to CNY,100",
    ] {
        assert_eq!(
            parse_pair(query),
            Some(("USD".to_owned(), "CNY".to_owned(), 100.0)),
            "{query}"
        );
    }
}

#[test]
fn test_multiplier_can_touch_any_known_currency_code() {
    let all_except_cny: BTreeSet<&str> = CURRENCIES
        .iter()
        .chain(MORE_CURRENCIES.iter())
        .copied()
        .filter(|&c| c != "CNY")
        .collect();
    assert_eq!(all_except_cny.len(), 157);
    for currency in all_except_cny {
        let q = format!("2k{currency} to CNY");
        assert_eq!(
            parse_pair(&q),
            Some((currency.to_owned(), "CNY".to_owned(), 2_000.0)),
            "{q}"
        );
    }
}

#[test]
fn test_same_currency_and_prefixed_tokens_edge_cases() {
    assert_eq!(parse_pair("100 USD to USD"), None);
    assert_eq!(
        parse_pair("v2 100 USD to CNY"),
        Some(("USD".to_owned(), "CNY".to_owned(), 100.0))
    );
    assert_eq!(
        parse_pair("100 USD to EUR or CNY"),
        Some(("USD".to_owned(), "EUR".to_owned(), 100.0))
    );
    assert_eq!(
        parse_pair("500 人民币 换 美元"),
        Some(("CNY".to_owned(), "USD".to_owned(), 500.0))
    );
}

#[test]
fn test_invalid_or_nonfinite_amount_does_not_produce_a_direct_answer() {
    let huge = "9".repeat(400);
    for amount in ["1,23", "12,34,567", "1.2.3", huge.as_str()] {
        let q = format!("{amount} USD to CNY");
        assert_eq!(parse_pair(&q), None, "{q}");
    }
}

#[tokio::test]
async fn test_zero_conversion_keeps_zero_in_the_returned_answer() {
    let mut f = Fixture::new("frankfurter", "search", "2026-11-25T10:00:00Z");
    f.exchanges.push(Exchange::new(
        FixtureRequest::new(
            "GET",
            "https://api.frankfurter.dev/v1/latest?base=USD&symbols=CNY",
        ),
        FixtureResponse::from_bytes(
            200,
            vec![("content-type".into(), "application/json".into())],
            br#"{"base":"USD","date":"2026-01-01","rates":{"CNY":7.0}}"#,
        ),
    ));
    let net = Arc::new(ReplayNet::from_fixtures(vec![f]).unwrap());
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()));
    let engine = FrankfurterEngine::new(deps);
    let cx = SrcCx::new(CancellationToken::new(), Duration::from_secs(5));

    let SrcOutput::Hits(hits) = engine
        .call("search", json!({"query": "0 USD to CNY", "limit": 5}), &cx)
        .await
        .unwrap()
    else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].snippet.starts_with("0 USD = 0.0000 CNY · "),
        "{}",
        hits[0].snippet
    );
}

#[test]
fn test_missing_amount_still_defaults_to_one_unit() {
    let net = Arc::new(ReplayNet::from_fixtures(vec![]).unwrap());
    let deps = SourceDeps::new(net, Arc::new(NullStore::new()), Arc::new(Config::default()));
    let hits = FrankfurterEngine::new(deps)
        .map_results(&json!({"base": "USD", "date": "2026-01-01", "rates": {"CNY": 7.0}}));
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].snippet.starts_with("1 USD = 7 CNY · "),
        "{}",
        hits[0].snippet
    );
}
