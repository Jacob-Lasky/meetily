#!/usr/bin/env python3
"""Refresh AI Assist's built-in Deepgram knowledge (WORK-581).

Source: https://deepgram.com/llms.txt, Deepgram's own public summary of its
products, pricing tiers, solutions, benchmarks and trust facts. Markdown links
are reduced to their text (URLs cost tokens and a card cannot show them), and
a header records the source and the fetch date. Output:
frontend/src-tauri/src/live_assist/deepgram_public.md (compiled into the app).

Usage: python3 scripts/refresh-deepgram-knowledge.py
"""
import datetime
import pathlib
import re
import urllib.request

SOURCE = "https://deepgram.com/llms.txt"
OUT = pathlib.Path(__file__).resolve().parent.parent / "frontend/src-tauri/src/live_assist/deepgram_public.md"


def main() -> None:
    req = urllib.request.Request(SOURCE, headers={"User-Agent": "meetily-ai-assist-refresh"})
    text = urllib.request.urlopen(req, timeout=30).read().decode("utf-8")
    text = re.sub(r"\s*\(\[docs\]\([^)]*\)\)", "", text)  # "([docs](url))" asides
    text = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", text)  # [text](url) -> text
    text = re.sub(r"\n{3,}", "\n\n", text).strip()
    today = datetime.date.today().isoformat()
    header = f"Deepgram public overview, from {SOURCE} (fetched {today}). Public facts only; prices are list prices.\n\n"
    OUT.write_text(header + text + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({len(header + text)} chars)")


if __name__ == "__main__":
    main()
