#!/usr/bin/env python3
"""Refresh AI Assist's built-in public knowledge (WORK-581).

Sources, each the company's own llms.txt:
- https://deepgram.com/llms.txt: Deepgram's public summary of its products,
  pricing tiers, solutions, benchmarks and trust facts (whole file).
- https://docs.pipecat.ai/llms.txt: Pipecat's intro and component list (the
  text before the second "## " heading; the rest is a page index).
- https://docs.daily.co/llms.txt: Daily's welcome line and its guide pages
  (title: description), the product-level part of a large API index.
Markdown links are reduced to their text (URLs cost tokens and a card cannot
show them), and a header records the sources and the fetch date. Output:
frontend/src-tauri/src/live_assist/deepgram_public.md (compiled into the app).
Anything deeper is the research lane's job (the built-in docs servers).

Usage: python3 scripts/refresh-deepgram-knowledge.py
"""
import datetime
import pathlib
import re
import urllib.request

DEEPGRAM = "https://deepgram.com/llms.txt"
PIPECAT = "https://docs.pipecat.ai/llms.txt"
DAILY = "https://docs.daily.co/llms.txt"
OUT = pathlib.Path(__file__).resolve().parent.parent / "frontend/src-tauri/src/live_assist/deepgram_public.md"


#: A link target: (url), allowing one level of parentheses inside the URL.
_TARGET = r"\((?:[^()\s]|\([^()\s]*\))*\)"


def strip_links(text: str) -> str:
    """Images go, "([docs](url))" asides go, and [text](url) becomes text,
    innermost first, so a link inside link text reduces too."""
    text = re.sub(r"!\[(?:[^\[\]]|\[[^\[\]]*\])*\]" + _TARGET, "", text)
    text = re.sub(r"\s*\(\[docs\]" + _TARGET + r"\)", "", text)
    while True:
        # Link text may hold one level of brackets ("[Deepgram [docs]](url)").
        reduced = re.sub(r"\[((?:[^\[\]]|\[[^\[\]]*\])*)\]" + _TARGET, r"\1", text)
        if reduced == text:
            return text
        text = reduced


def fetch(url: str) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": "meetily-ai-assist-refresh"})
    return urllib.request.urlopen(req, timeout=30).read().decode("utf-8")


def pipecat_intro(text: str) -> str:
    """The text before the second "## " heading."""
    out, headings = [], 0
    for line in text.splitlines():
        if line.startswith("## "):
            headings += 1
            if headings == 2:
                break
        out.append(line)
    return "\n".join(out)


def daily_guides(text: str) -> str:
    """The title line, the welcome entry and the /docs/guides/ entries."""
    lines = text.splitlines()
    keep = [lines[0]] if lines and lines[0].startswith("# ") else ["# Daily"]
    keep += [l for l in lines if "docs.daily.co/index.md" in l or "docs.daily.co/docs/guides/" in l]
    return "\n".join(keep)


#: Size bounds per section (characters, as written: links stripped, blank
#: runs collapsed, trimmed). A source whose format changed (headings,
#: relative links) lands outside them and the refresh stops instead of
#: shipping an empty or index-sized section.
BOUNDS = {"Deepgram": (2_000, 20_000), "Pipecat": (500, 6_000), "Daily": (1_500, 12_000)}


def main() -> None:
    raw = [fetch(DEEPGRAM), pipecat_intro(fetch(PIPECAT)), daily_guides(fetch(DAILY))]
    # Bounds are checked on each section exactly as it is written.
    parts = [re.sub(r"\n{3,}", "\n\n", strip_links(p)).strip() for p in raw]
    for name, part in zip(BOUNDS, parts):
        low, high = BOUNDS[name]
        if not low <= len(part) <= high:
            raise SystemExit(f"{name} section is {len(part)} characters, outside {low}-{high}: its source format changed")
    text = "\n\n".join(parts)
    if "](" in text:
        raise SystemExit("a Markdown link survived; extend strip_links before shipping this text")
    today = datetime.date.today().isoformat()
    header = (
        f"Public overview of Deepgram and of Pipecat and Daily (Deepgram companies), from {DEEPGRAM}, "
        f"{PIPECAT} and {DAILY} (fetched {today}). Public facts only; prices are list prices.\n\n"
    )
    OUT.write_text(header + text + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({len(header + text)} chars)")


if __name__ == "__main__":
    main()
