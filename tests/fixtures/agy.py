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
if not args or (args[0].startswith("-") and args[0] not in ("--settings","--output-format","-p")):
    if (home/"interactive-test").exists():
        import signal
        def interrupted(*_):
            with (home/"interrupts").open("a") as f:
                f.write("SIGINT\n")
        signal.signal(signal.SIGINT, interrupted)
        (home/"tui-ready").touch()
        print(input(), flush=True)
        sys.exit()
    print(json.dumps({"home": str(home), "args": args}) if args else "fixture Antigravity TUI")
    sys.exit()
if args == ["models"]:
    print("Fetching available models...", file=sys.stderr)
    if not (home/".gemini/antigravity-cli/antigravity-oauth-token").exists():
        sys.exit("Error: Please sign in to view available models.")
    print("fixture-model\tFixture Model")
    sys.exit()
if "/usage" in args:
    usage = ["--settings", '{"disableSlashCommands":false}', "-p", "/usage"]
    with (home / "usage-calls").open("a") as f:
        print(json.dumps(args), file=f)
    assert Path.cwd() == home
    if args == usage:
        print("Gemini Models\tWeekly Limit Remaining\t76%\t2030-01-01T00:00:00Z")
        sys.exit()
    assert args == ["--output-format", "json", *usage], args
    assert not sys.stdin.read()
    if (home / "report.json").exists():
        sys.stdout.buffer.write((home / "report.json").read_bytes())
        sys.exit(1 if (home / "fail-report").exists() else 0)
    import time
    while (home / "hold-report").exists():
        time.sleep(.02)
    print(json.dumps({"status":"SUCCESS","num_turns":0,"conversation_id":"","result":"fixture-private",
                      "command":{
        "name":"usage","data":{"description":"fixture-private","groups":[
            {"name":"Gemini Models","buckets":[
                {"id":"gemini-weekly","name":"Weekly Limit Remaining","window":"weekly",
                 "remaining_fraction":0.76,"reset_time":"2030-01-01T01:00:00+01:00"},
                {"id":"gemini-5h","name":"Five Hour Limit Remaining","remaining_fraction":1,
                 "reset_time":"1970-01-01T00:00:00Z"},
                {"window":"weekly","disabled":True,"remaining_fraction":0.25},
                {"window":"weekly","remaining_amount":99},
                {"window":"weekly","remaining_fraction":1.2},
                {"window":"fixture-private","remaining_fraction":0.25}]},
            {"name":"Claude and GPT models","buckets":[{"id":"3p-weekly","remaining_fraction":0}]},
            {"name":"fixture-private","buckets":[{"window":"weekly","remaining_fraction":1}]}]}}}))
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
