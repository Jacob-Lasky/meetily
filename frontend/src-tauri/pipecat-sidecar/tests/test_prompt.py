import json
from pathlib import Path

import pytest

from meetily_sidecar.prompt import MAX_CARD_WORDS, Card, ParseError, normalize, parse_cards, user_prompt

FIXTURE = Path(__file__).parent / "fixtures" / "user_prompt.json"


@pytest.mark.parametrize("case", json.loads(FIXTURE.read_text())["cases"])
def test_user_prompt_matches_the_shared_fixture(case):
    lines = [tuple(pair) for pair in case["lines"]]
    assert user_prompt(lines, case["shown"]) == case["expected"]


def test_parses_the_documented_shape():
    out = '{"cards":[{"kind":"ask","text":"Average call length?","quote":"forty thousand calls"}]}'
    assert parse_cards(out) == [Card("ask", "Average call length?", "forty thousand calls")]


def test_empty_cards_are_valid():
    assert parse_cards('{"cards":[]}') == []
    assert parse_cards('{"cards":null}') == []
    assert parse_cards("[]") == []


def test_tolerates_fences_prose_and_a_bare_array():
    fenced = 'Here:\n```json\n{"cards":[{"kind":"todo","text":"Send pricing","quote":"q"}]}\n```\nbye'
    assert parse_cards(fenced) == [Card("todo", "Send pricing", "q")]
    bare = '[{"kind":"FACT","text":" 40k calls a day ","quote":"q"}]'
    assert parse_cards(bare) == [Card("fact", "40k calls a day", "q")]


def test_skips_bad_items_and_keeps_good_ones():
    out = """{"cards":[
        {"kind":"rumour","text":"bad kind","quote":"q"},
        {"kind":"ask","text":"   ","quote":"q"},
        {"kind":"ask","quote":"no text"},
        "not an object",
        {"kind":"answer","text":"Yes, batch works","quote":5},
        {"kind":"ask","text":"Which languages?"}
    ]}"""
    assert parse_cards(out) == [Card("answer", "Yes, batch works", ""), Card("ask", "Which languages?", "")]


@pytest.mark.parametrize(
    "malformed",
    ["", "I have no suggestions.", '{"suggestions": []}', '{"cards": "none"}', "42", '{"cards":[{"kind":'],
)
def test_malformed_output_is_a_parse_error(malformed):
    with pytest.raises(ParseError):
        parse_cards(malformed)


def test_truncated_json_after_a_good_object_still_parses_the_good_one():
    assert parse_cards('{"cards":[]} {"cards":[{"kind"') == []


def test_keeps_at_most_three_cards():
    item = '{"kind":"ask","text":"t","quote":"q"}'
    assert len(parse_cards('{"cards":[' + ",".join([item] * 5) + "]}")) == 3


def test_cuts_long_text_to_25_words():
    long = " ".join(f"w{i}" for i in range(1, 31))
    text = parse_cards(f'{{"cards":[{{"kind":"fact","text":"{long}","quote":"q"}}]}}')[0].text
    assert len(text.split()) == MAX_CARD_WORDS
    assert text.endswith("w25…")


def test_normalize_drops_punctuation_and_case():
    assert normalize("  Can you SEND pricing?! ") == "can you send pricing"
