"""Live-assist user prompt and card parsing.

A port of `src/live_assist/cards.rs` (`user_prompt`, `parse_cards`,
`normalize`). The SYSTEM prompt is not copied here: Rust sends
`cards::SYSTEM_PROMPT` in the assist config, so its wording has one home.
`tests/fixtures/user_prompt.json` is checked by both the Python and the Rust
tests, so the two user-prompt builders cannot drift apart.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Iterable

MAX_CARDS_PER_CALL = 3
MAX_CARD_WORDS = 25
KINDS = ("answer", "ask", "fact", "todo")

SIDE_PREFIX = {"me": "Me", "them": "Them", "unknown": "Speaker"}


def _one_line(text: str) -> str:
    return " ".join(text.split())


def user_prompt(lines: Iterable[tuple[str, str]], shown: Iterable[str]) -> str:
    """The context window (oldest first, `(side, text)`), then the cards Me
    already has or dismissed. Same bytes as `cards::user_prompt` in Rust."""
    out = ["Transcript, oldest first:\n"]
    for side, text in lines:
        out.append(f"{SIDE_PREFIX.get(side, 'Speaker')}: {_one_line(text)}\n")
    shown = [_one_line(t) for t in shown]
    if shown:
        out.append("\nCards Me already has or dismissed (do not repeat or rephrase these):\n")
        out.extend(f"- {t}\n" for t in shown)
    return "".join(out)


def normalize(text: str) -> str:
    """Lowercase words, punctuation dropped (`cards::normalize`)."""
    return " ".join("".join(c.lower() if c.isalnum() else " " for c in text).split())


@dataclass(frozen=True)
class Card:
    kind: str
    text: str
    quote: str

    def as_json(self) -> dict:
        return {"kind": self.kind, "text": self.text, "quote": self.quote}


class ParseError(ValueError):
    pass


def _first_json_value(s: str):
    decoder = json.JSONDecoder()
    for i, ch in enumerate(s):
        if ch not in "{[":
            continue
        try:
            value, _ = decoder.raw_decode(s, i)
        except ValueError:
            continue
        return value
    return None


def _clamp_words(text: str, max_words: int) -> str:
    words = text.split()
    if len(words) <= max_words:
        return " ".join(words)
    return " ".join(words[:max_words]) + "…"


def _card(item) -> Card | None:
    if not isinstance(item, dict):
        return None
    kind = item.get("kind")
    text = item.get("text")
    if not isinstance(kind, str) or kind.strip().lower() not in KINDS:
        return None
    if not isinstance(text, str):
        return None
    text = _clamp_words(text.strip(), MAX_CARD_WORDS)
    if not text:
        return None
    quote = item.get("quote")
    quote = quote.strip() if isinstance(quote, str) else ""
    return Card(kind=kind.strip().lower(), text=text, quote=quote)


def parse_cards(output: str) -> list[Card]:
    """Parse a model reply into at most MAX_CARDS_PER_CALL cards.

    Tolerant of fences, prose around the JSON, a bare array, unknown kinds and
    empty texts (bad items are skipped). Raises ParseError when nothing usable
    came back; the caller treats that turn as "no cards".
    """
    value = _first_json_value(output or "")
    if value is None:
        raise ParseError("model output held no JSON")
    if isinstance(value, list):
        items = value
    elif isinstance(value, dict) and "cards" in value:
        items = value["cards"]
        if items is None:
            return []
        if not isinstance(items, list):
            raise ParseError('model output JSON was not {"cards": [...]}')
    else:
        raise ParseError('model output JSON was not {"cards": [...]}')
    cards = [c for c in (_card(i) for i in items) if c is not None]
    return cards[:MAX_CARDS_PER_CALL]
