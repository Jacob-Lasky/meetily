"""Checks strip_links in refresh-deepgram-knowledge.py: python3 scripts/check-refresh-deepgram-knowledge.py"""
import importlib.util
spec = importlib.util.spec_from_file_location("r", __import__("pathlib").Path(__file__).with_name("refresh-deepgram-knowledge.py"))
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
cases = {
    "[Deepgram [docs]](https://e.com)": "Deepgram [docs]",
    "[a [b](https://u)](https://v)": "a b",
    "![arch](https://e.com/a.png) after": " after",
    "![Deepgram [logo]](https://e.com/logo.png) after": " after",
    "[Wiki](https://e.com/a_(b)) end": "Wiki end",
    "Nova-3 ([docs](https://d.com/x)).": "Nova-3.",
    "plain [text] and (paren)": "plain [text] and (paren)",
}
bad = [(k, m.strip_links(k), v) for k, v in cases.items() if m.strip_links(k) != v]
assert not bad, bad
print("ok")
