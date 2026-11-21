"""Exercise the release boundary: real HTTP, Unix socket, exec, hook, and SQLite."""
import http.client
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[1]
DAEMON = Path(os.environ.get("CCPD_TEST_BIN", ROOT / "server/target/debug/ccpd"))
FAKE = r'''
import json, os, pathlib, shlex, subprocess, sys, uuid
t = pathlib.Path(sys.argv[0]).name
if sys.argv[1:] == ["--version"]:
    print("codex-cli 0.171.0" if t == "codex" else "2.1.329 (Claude Code)")
    sys.exit()
home = pathlib.Path(os.environ["CODEX_HOME" if t == "codex" else "CLAUDE_CONFIG_DIR"])
if sys.argv[1:2] == ["app-server"]:
    for line in sys.stdin:
        r = json.loads(line)
        if "id" not in r: continue
        result = {}
        if r["method"] == "hooks/list":
            entry = json.loads((home / "hooks.json").read_text())["hooks"]["SessionStart"][0]["hooks"][0]
            project = pathlib.Path(r["params"]["cwds"][0]) / ".codex/config.toml"
            enabled = not project.exists() or project.read_text() != "hooks=false"
            result = {"data": [{"errors": [], "hooks": [dict(eventName="sessionStart", handlerType="command", command=entry["command"], enabled=enabled, matcher="startup", source="user", sourcePath=str(home / "hooks.json"),trustStatus="trusted",currentHash="test")]}]}
        if r["method"] == "account/rateLimits/read":
            with (home / "quota-count").open("a") as f: f.write("fetch\n")
            result = {"rateLimits": {"planType":"pro", "primary":{"windowDurationMins":300,"usedPercent":0,"resetsAt":2000000000}}}
        print(json.dumps({"id": r["id"], "result": result}), flush=True)
    sys.exit()
native = None
if os.environ.get("CCP_LAUNCH_ID"):
    native = str(uuid.uuid4())
    entry = json.loads((home / "hooks.json").read_text())["hooks"]["SessionStart"][0]["hooks"][0]
    subprocess.run(shlex.split(entry["command"]), input=json.dumps({"hook_event_name":"SessionStart","source":"startup","session_id":native}), text=True, check=True)
print(json.dumps({"argv":sys.argv[1:],"home":str(home),"id":native,"proxy":os.getenv("HTTPS_PROXY")}))
'''

@unittest.skipUnless(DAEMON.is_file(), "build ccpd first")
class Service(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="ccp-e2e-")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        paths = self.root / "bin"
        paths.mkdir()
        for tool in ["claude", "codex"]:
            bin = paths / tool
            bin.write_text("#!" + sys.executable + "\n" + FAKE)
            bin.chmod(0o700)
        curl = paths / "curl"
        curl.write_text("#!" + sys.executable + r'''
import json, os, pathlib, sys
home = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"])
config = sys.stdin.read()
assert 'Bearer private-test-token' in config and not any('private-test-token' in a for a in sys.argv)
assert 'https://api.anthropic.com/api/oauth/usage' in config
(home / "quota-count").write_text("fetch\n")
print('{"five_hour":{"utilization":25,"resets_at":"2030-01-01T00:00:00Z"}}\n200\n', end="")
''')
        curl.chmod(0o700)
        self.env = {"PATH": str(paths) + ":/usr/bin:/bin", "HOME": str(self.root),
                    "XDG_RUNTIME_DIR": str(self.root / "run"), "XDG_STATE_HOME": str(self.root / "state")}
        if getattr(self, "native", False):
            dirs = [str(Path(shutil.which(t)).parent) for t in ("claude", "codex", "node")]
            self.env["PATH"] = ":".join(dirs + ["/usr/bin", "/bin"])
            self.env["HTTP_PROXY"] = self.env["HTTPS_PROXY"] = "http://127.0.0.1:9"
            self.env["NO_PROXY"] = "localhost,127.0.0.1"
        self.p = subprocess.Popen([str(DAEMON), "--port", "0"], env=self.env, cwd=self.root, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.stop)
        line = self.p.stderr.readline()
        self.assertIn("http://127.0.0.1:", line, line)
        self.port = int(re.search(r":(\d+)$", line.strip())[1])
        self.cookie = self.csrf = ""
        token = (self.root / "run/cc-profile/pairing-token").read_text().strip()
        status, data, headers = self.api("POST", "login", {"token": token})
        self.assertEqual(status, 200)
        self.cookie = headers["set-cookie"].split(";")[0]
        self.csrf = data["csrf"]

    def stop(self):
        self.p.terminate()
        self.p.wait(timeout=5)
        self.p.stderr.close()

    def api(self, method, route, body=None, extra=None):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=8)
        headers = {"Origin": "http://127.0.0.1:%s" % self.port, "Content-Type": "application/json",
                   "Cookie": self.cookie, "X-CSRF-Token": self.csrf}
        headers.update(extra or {})
        try:
            conn.request(method, "/api/v1/" + route, json.dumps(body) if body is not None else None, headers)
            r = conn.getresponse()
            return r.status, json.loads(r.read() or "null"), {k.lower(): v for k, v in r.getheaders()}
        finally:
            conn.close()

    def account(self, tool, name):
        path = self.root / name
        path.mkdir()
        status, c, _ = self.api("POST", "accounts", dict(tool=tool, label=name, home=str(path)))
        self.assertEqual(status, 201, c)
        status, c, _ = self.api("PATCH", "accounts/" + str(c["id"]), {"enabled":True,"revision":c["revision"]})
        self.assertEqual(status, 200, c)
        return c

    def cli(self, *args):
        return subprocess.run([sys.executable, str(ROOT / "cc_profile.py"), *args], env=self.env,
                              text=True, capture_output=True, stdin=subprocess.DEVNULL, cwd=self.root,
                              timeout=30 if getattr(self,"native",False) else 8)

    def test_claude_exec_stickiness_and_fail_closed_controls(self):
        c = self.account("claude", "one")
        self.account("claude", "two")
        p = self.cli("claude", "-p", "private prompt")
        self.assertEqual(p.returncode, 0, p.stderr)
        out = json.loads(p.stdout)
        uid = out["argv"][1]
        self.assertEqual(out["argv"][2:], ["-p", "private prompt"])
        self.assertEqual(out["home"], c["home"])
        self.assertEqual(json.loads(self.cli("claude", "-r", uid).stdout)["home"], c["home"])
        code, _, _ = self.api("PATCH", "accounts/%d" % c["id"], {"enabled":False,"revision":c["revision"]})
        self.assertEqual(code, 200)
        p = self.cli("claude", "-r", uid)
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(p.stdout, "")
        self.assertIn("Re-enable", p.stderr)
        _, data, _ = self.api("GET", "startups")
        self.assertEqual(data["items"][0]["reason"], "owner_disabled")
        self.assertNotIn("private prompt", str(data))
        self.assertNotIn("ticket", str(data))

    def test_observer_enters_via_private_socket_and_can_resume(self):
        c = self.account("codex", "cx")
        self.assertNotEqual(self.cli("codex", "exec", "first").returncode, 0)
        for op in ["install", "check"]:
            status, data, _ = self.api("POST", "accounts/%s/observer/%s" % (c["id"],op), {"revision":c["revision"]})
            self.assertEqual(status, 200, data)
        p = self.cli("codex", "exec", "--json", "first")
        self.assertEqual(p.returncode, 0, p.stderr)
        uid = json.loads(p.stdout)["id"]
        _, data, _ = self.api("GET", "sessions?native_id=" + uid)
        self.assertEqual(data["items"][0]["evidence"], "observed")
        self.assertEqual(data["items"][0]["config_id"], c["id"])
        out = json.loads(self.cli("codex", "resume", uid).stdout)
        self.assertEqual(out["argv"], ["--no-daemon", "resume", uid])
        self.assertNotEqual(self.cli("codex", "resume", "--last").returncode, 0)
        self.assertNotEqual(self.cli("codex", "resume", str(uuid.uuid4())).returncode, 0)
        project=self.root/"workspace"
        layer=project/".codex/config.toml"
        layer.parent.mkdir(parents=True)
        layer.write_text("hooks=false")
        self.assertNotEqual(self.cli("codex","-C",str(project),"exec","first").returncode,0)
        layer.write_text("hooks=true")
        self.assertEqual(self.cli("codex","-C",str(project),"exec","first").returncode,0)

    def test_web_authenticates_reads_and_mutations(self):
        self.assertEqual(self.api("GET","accounts?tool=codex",extra={"Cookie":""})[0],403)
        for extra in [{"Cookie":""}, {"Host":"example.invalid"}, {"Origin":"https://example.invalid"}, {"X-CSRF-Token":""}]:
            with self.subTest(extra=extra):
                status, _, _ = self.api("POST", "accounts", {}, extra)
                self.assertEqual(status, 403)

    def test_quota_is_cached_without_consuming_a_rotation_or_starting_a_chat(self):
        cl = self.account("claude", "one")
        cx = self.account("codex", "two")
        (Path(cl["home"]) / ".credentials.json").write_text(json.dumps({"claudeAiOauth":{"accessToken":"private-test-token"}}))
        for _ in range(2):
            status, _, _ = self.api("POST", "quota/refresh", {"stale_only":True})
            self.assertEqual(status, 202)
        for c, want in [(cl,25),(cx,0)]:
            for _ in range(100):
                _, data, _ = self.api("GET", "accounts?tool=" + c["tool"])
                q = data["items"][0]["quota"]
                if not q["loading"] and q["status"] != "not_checked": break
                time.sleep(0.02)
            self.assertEqual(q["status"], "fresh", q)
            self.assertEqual(q["snapshot"]["windows"][0]["used_pct"], want)
            self.assertEqual(q["snapshot"]["windows"][0]["reset_at_ms"], 1893456000000 if c is cl else 2000000000000)
            _, data, _ = self.api("POST", "accounts/%d/quota/refresh" % c["id"], {})
            self.assertEqual(data["queued"], 0)
            self.assertEqual((Path(c["home"]) / "quota-count").read_text(), "fetch\n")
        self.assertEqual(self.api("GET", "startups")[1]["items"], [])
