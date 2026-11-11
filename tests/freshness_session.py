"""One mcp session, config unchanged: a file added and a file replaced by the indexer appear
in the next search. Usage: freshness_session.py <llm-index> <config> <root>"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mcp_client import Session  # noqa: E402

exe, config, root = sys.argv[1:4]
s = Session(exe, config, "private")


def hits(q):
    err, text = s.call("search", {"query": q, "mode": "words"})
    assert not err, text
    import json
    return [h["path"] for h in json.loads(text)["hits"]]


def wait(q, present, what):
    for _ in range(150):
        if bool(hits(q)) == present:
            return
        time.sleep(2)
    raise SystemExit(f"timeout: {what}")


assert not hits("freshaddedmarker")
with open(os.path.join(root, "added.md"), "w") as f:
    f.write("freshaddedmarker is new\n")
wait("freshaddedmarker", True, "added file never appeared")

path = os.path.join(root, "replaced.md")
assert hits("oldcontentmarker")
tmp = path + ".tmp"
with open(tmp, "w") as f:
    f.write("newcontentmarker replaces it\n")
os.replace(tmp, path)
wait("newcontentmarker", True, "replacement never appeared")
wait("oldcontentmarker", False, "old content still found")
print("freshness ok")
