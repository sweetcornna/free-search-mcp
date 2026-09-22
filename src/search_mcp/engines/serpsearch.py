"""Keyless Google SERP scraper, alias of the ``google`` engine."""

from __future__ import annotations

from .google import GoogleEngine


class SerpSearchEngine(GoogleEngine):
    name = "serpsearch"
    description = "Alias of `google`: the same Google web SERP under a second name."
