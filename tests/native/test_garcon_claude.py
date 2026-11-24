"""Optional: exercise Garcon's actual Bun runtime through CCP, with no provider or credentials."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_service as svc

GARCON = os.getenv("CCP_TEST_GARCON")
NATIVE = r'''
import json, os, pathlib, sys
args = sys.argv[1:]
if args == ["--version"]:
    print("2.1.329 (Claude Code)")
    sys.exit()
home = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"])
with (home/"calls").open("a") as f:
    print(json.dumps(args), file=f)
if args == ["auth", "status"]:
    print(json.dumps({"loggedIn":True, "email":home.name}))
    sys.exit()
if "--input-format" not in args:
    assert "--no-session-persistence" in args
    print(home.name)
    sys.exit()
selector = next((a for a in args if a.startswith(("--session-id=", "--resume="))), None)
if selector:
    assert "--no-session-persistence" not in args
    assert os.environ["CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS"] == "1"
    assert not os.getenv("CLAUDECODE")
    assert args[-2:] == ["-p", ""]
    flag, uid = selector.split("=")
    saved = home/uid
    if flag == "--session-id":
        saved.touch(exist_ok=False)
    else:
        assert saved.is_file(), "wrong resume owner"
def reply(msg):
    print(json.dumps(msg), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    if m["type"] == "control_request":
        reply({"type":"control_response","response":{"subtype":"success","request_id":m["request_id"],
               "response":{"commands":["fixture"]}}})
    elif m["type"] == "user":
        assert selector and m["session_id"] == uid
        with saved.open("a") as f: print(m["message"]["content"], file=f)
        reply({"type":"command_lifecycle","command_uuid":m["uuid"],"state":"started"})
        reply({"type":"result","subtype":"success","is_error":False,
               "result":home.name,"user_message_uuid":m["uuid"]})
        reply({"type":"system","subtype":"session_state_changed","state":"idle"})
'''


@unittest.skipUnless(GARCON and shutil.which("bun") and svc.DAEMON.is_file() and svc.LAUNCHER.is_file(),
                     "requires CCP_TEST_GARCON, Bun and a built CCP pair")
class GarconClaude(unittest.TestCase):
    def test_persistent_runtime_cold_reconnect_and_one_shot(self):
        s = svc.Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(s.doCleanups)
        s.setUp()
        (s.root/"bin/claude").write_text("#!" + sys.executable + "\n" + NATIVE)
        first = s.account("claude", "first")
        other = s.account("claude", "other")
        self.assertEqual(s.api("PUT", "current/claude", {"config_id":other["id"]})[0], 200)
        alias = s.root/"claude"
        alias.symlink_to(svc.LAUNCHER)
        env = dict(s.env, PATH=str(s.root) + ":" + s.env["PATH"], CLAUDECODE="test-nesting",
                   FIXTURE_SRC=str(Path(GARCON).resolve()/"server-agents/claude/src"),
                   FIXTURE_ALIAS=str(alias))
        p = subprocess.run([shutil.which("bun"), "--no-install", Path(__file__).with_name("garcon-claude.mjs")],
                           env=env, cwd=s.root, capture_output=True, text=True, timeout=30)
        self.assertEqual(p.returncode, 0, p.stderr)
        uid = json.loads(p.stdout)
        self.assertEqual((Path(first["home"])/uid).read_text().splitlines(), ["fresh", "warm", "cold"])
        rows = s.api("GET", "sessions?tool=claude")[1]["items"]
        self.assertEqual([(r["native_id"], r["config_id"], r["evidence"]) for r in rows],
                         [(uid, first["id"], "assigned")])
        rows = s.api("GET", "startups?tool=claude")[1]["items"]
        self.assertEqual((rows[0]["state"], rows[0]["reason"]), ("denied", "identity_override"))
        self.assertEqual([(r["intent"], r["config_id"]) for r in rows[1:] if r["intent"] != "misc"],
                         [("print", first["id"]), ("resume", first["id"]), ("print", other["id"]),
                          ("new", first["id"])])
        self.assertNotIn("Claude Code asks a person", str(rows))
        self.assertNotIn("fixture-only", str(rows))
