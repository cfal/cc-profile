#!/usr/bin/env python3
"""Offline stand-in for the pinned Antigravity subprocess. Never uses credentials."""
import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
if args == ["--version"]:
    print("1.5.14")
    sys.exit()
home = Path(os.environ["HOME"])
rt = Path(os.environ["XDG_RUNTIME_DIR"])
assert rt.is_dir() and rt.stat().st_mode & 0o077 == 0 and not (rt / "bus").exists()
assert "DBUS_SESSION_BUS_ADDRESS" not in os.environ
assert "XDG_CONFIG_HOME" not in os.environ
assert os.environ["AGY_ACP_FORCE_FILE_STORAGE"] == "1"
assert home.is_absolute()
obs = {"proxy": [os.getenv(k) for k in ("HTTP_PROXY","HTTPS_PROXY","http_proxy","https_proxy")],
       "runtime": str(rt), "home": str(home), "cwd": os.getcwd()}
(home / "last-run").write_text(json.dumps(obs))
if not args:
    print("fixture Antigravity TUI")
    sys.exit()
if args == ["models"]:
    print("Fetching available models...", file=sys.stderr)
    if not (home/".gemini/antigravity-cli/antigravity-oauth-token").exists():
        sys.exit("Error: Please sign in to view available models.")
    print("fixture-model\tFixture Model")
    sys.exit()
if args[0].split("=")[0] == "--conversation":
    print(json.dumps({"home": str(home), "args": args}))
    sys.exit()
if args == ["-p", "/usage"]:
    assert Path.cwd() == home
    print("Model quotas: weekly remaining 76%" if not (home / "bad-report").exists()
          else "Model quotas unavailable: Bearer sensitive-fixture-value")
    sys.exit()
assert args[0] == "acp" and args[-4:] == ["--gemini-dir",str(home/".gemini"),"--app-data-dir","antigravity-cli"], args
assert "--for-session" not in args
if "--login" in args:
    token = home/".gemini/antigravity-cli/antigravity-oauth-token"
    token.parent.mkdir(parents=True,exist_ok=True)
    token.write_text("inert fixture marker")
    sys.exit()
held = None
for line in sys.stdin:
    r = json.loads(line)
    if "method" not in r or "id" not in r:
        continue
    params = r.get("params", {})
    if r["method"] == "initialize":
        result = {"protocolVersion": 1, "authMethods": [{"id": "terminal-login"}]}
    elif r["method"] == "session/new":
        result = {"sessionId":params["fixtureId"]}
    else:
        result = {"home":str(home)}
    with (home / "calls").open("a") as f:
        print(r["method"], file=f)
    reply = {"jsonrpc":"2.0", "id":r["id"], "result":result}
    if params.get("invalidReply"):
        reply["error"] = None
    if params.get("hold"):
        held = reply
        continue
    if params.get("permission"):
        print(json.dumps({"jsonrpc":"2.0","id":r["id"],"method":"session/request_permission","params":{}}))
    print(json.dumps(reply),flush=True)
    if held:
        print(json.dumps(held),flush=True)
        held = None
