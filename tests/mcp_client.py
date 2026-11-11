"""One-shot MCP client for the flake checks.
CLI:  mcp_client.py <llm-index> <config> <public|private> <tool> [json-args]
Prints the tool's text; exit 0 ok, 1 tool error (text still printed), 2 protocol failure.
Import:  call(exe, config, tier, tool, args) -> (is_error, text)."""
import json
import subprocess
import sys


class Session:
    """One long-lived MCP session."""

    def __init__(self, exe, config, tier):
        self.p = subprocess.Popen([exe, "mcp", "--tier", tier, "--config", config],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.id = 0
        self.rpc("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                                "clientInfo": {"name": "check", "version": "0"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, msg):
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()

    def rpc(self, method, params):
        self.id += 1
        self.send({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("mcp server closed")
            msg = json.loads(line)
            if msg.get("id") == self.id:
                return msg

    def call(self, tool, args=None):
        msg = self.rpc("tools/call", {"name": tool, "arguments": args or {}})
        if "error" in msg:
            return True, json.dumps(msg["error"])
        res = msg["result"]
        return bool(res.get("isError")), "".join(c.get("text", "") for c in res.get("content", []))

    def close(self):
        self.p.stdin.close()
        self.p.terminate()
        self.p.wait(timeout=30)


def call(exe, config, tier, tool, args=None):
    s = Session(exe, config, tier)
    try:
        return s.call(tool, args)
    finally:
        s.close()


def call_json(exe, config, tier, tool, args=None):
    err, text = call(exe, config, tier, tool, args)
    if err:
        raise RuntimeError(f"{tool} failed: {text}")
    return json.loads(text)


if __name__ == "__main__":
    exe, config, tier, tool = sys.argv[1:5]
    err, text = call(exe, config, tier, tool, json.loads(sys.argv[5]) if len(sys.argv) > 5 else {})
    print(text)
    sys.exit(1 if err else 0)
