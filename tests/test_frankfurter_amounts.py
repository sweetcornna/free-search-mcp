"""Currency amounts must survive parsing and conversion without silent rescaling."""
from __future__ import annotations

import pytest

from search_mcp.engines.frankfurter import (
    CURRENCIES,
    MORE_CURRENCIES,
    FrankfurterEngine,
    parse_pair,
)


@pytest.mark.parametrize("currency", sorted(c for c in CURRENCIES | MORE_CURRENCIES if c[0] == "K"))
def test_currency_code_is_not_a_thousands_suffix(currency):
    assert parse_pair(f"100 {currency} to USD") == (currency, "USD", 100.0)


@pytest.mark.parametrize("name", ["krona", "kronor", "krone", "kroner"])
def test_currency_name_is_not_a_thousands_suffix(name):
    expected = "SEK" if name in {"krona", "kronor"} else "NOK"
    assert parse_pair(f"100 {name} to USD") == (expected, "USD", 100.0)


@pytest.mark.parametrize(
    ("amount", "expected"),
    [
        ("1,000,000", 1_000_000.0),
        ("12,345,678.90", 12_345_678.90),
        ("1,000", 1000.0),
        ("2.5k", 2500.0),
        ("2.5 K", 2500.0),
        ("2 thousand", 2000.0),
        ("2 MILLION", 2_000_000.0),
        ("1万", 10_000.0),
        ("1亿", 100_000_000.0),
    ],
)
def test_grouped_amounts_and_explicit_multipliers(amount, expected):
    assert parse_pair(f"{amount} USD to CNY") == ("USD", "CNY", expected)


@pytest.mark.parametrize(
    ("query", "expected"),
    [
        ("2kUSD to CNY", 2000.0),
        ("2Kusd to CNY", 2000.0),
        ("把100美元换成人民币", 100.0),
        ("0.00001 USD to CNY", 0.00001),
        ("-100 USD to CNY", -100.0),
        ("+100 USD to CNY", 100.0),
        (".5 USD to CNY", 0.5),
    ],
)
def test_amount_boundaries_do_not_discard_value_or_sign(query, expected):
    assert parse_pair(query) == ("USD", "CNY", expected)


@pytest.mark.parametrize(
    "query",
    [
        "Convert USD to CNY: 100.",
        "Convert 100, please, from USD to CNY",
        "USD to CNY,100",
    ],
)
def test_sentence_punctuation_is_not_part_of_the_amount(query):
    assert parse_pair(query) == ("USD", "CNY", 100.0)


@pytest.mark.parametrize("currency", sorted((CURRENCIES | MORE_CURRENCIES) - {"CNY"}))
def test_multiplier_can_touch_any_known_currency_code(currency):
    assert parse_pair(f"2k{currency} to CNY") == (currency, "CNY", 2000.0)


@pytest.mark.parametrize("amount", ["1,23", "12,34,567", "1.2.3", "9" * 400])
def test_invalid_or_nonfinite_amount_does_not_produce_a_direct_answer(amount):
    assert parse_pair(f"{amount} USD to CNY") is None


async def test_zero_conversion_keeps_zero_in_the_returned_answer(monkeypatch):
    async def response(url):
        return {"base": "USD", "date": "2026-01-01", "rates": {"CNY": 7.0}}

    engine = FrankfurterEngine()
    monkeypatch.setattr(engine, "_get_json", response)
    result, = await engine.fetch_results("0 USD to CNY", 5, None)

    assert result.snippet.startswith("0 USD = 0.0000 CNY · ")


def test_missing_amount_still_defaults_to_one_unit():
    result, = FrankfurterEngine().map_results(
        {"base": "USD", "date": "2026-01-01", "rates": {"CNY": 7.0}}
    )
    assert result.snippet.startswith("1 USD = 7 CNY · ")
