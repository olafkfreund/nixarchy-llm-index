"""Assertions of the `fixture` check. Usage: check_fixture.py <llm-index> <config> <root>"""
import html
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from mcp_client import call_json  # noqa: E402

exe, config, root = sys.argv[1:4]


def search(query, **kw):
    return call_json(exe, config, "public", "search", {"query": query, "mode": "words", **kw})["hits"]


# denied, ignored, symlinked and bomb content never reached the index
for marker in ["envleakmarker", "netrcleakmarker", "ignoredmarker", "bombmarker"]:
    assert not search(marker), f"{marker} was indexed"
listing = call_json(exe, config, "public", "recent", {"days": 36500})["files"]
paths = [f["path"] for f in listing]
for hidden in [".env", ".netrc", "ignored.txt", "build-output", "link-to-env"]:
    assert not any(hidden in p for p in paths), f"{hidden} listed: {paths}"
if any("bomb.docx" in p for p in paths):
    print("note: bomb.docx is listed in recent (a failed row)")

# the long-line HTML page is present
assert any(h["path"].endswith("web/longline.html") for h in search("harbourlighthouse"))

# a code hit's lines, passed to get, return the whole function
hits = search("compute_checksum", type="code")
hit = next(h for h in hits if h["path"].endswith("code/checksum.rs"))
a, b = hit["lines"]
got = call_json(exe, config, "public", "get", {"path": hit["path"], "start": a, "end": b})["text"]
got = html.unescape(got)
assert "pub fn compute_checksum" in got and "(b << 16) | a" in got, got
print("fixture assertions ok")
