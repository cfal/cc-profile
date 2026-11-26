"""Exercise the release boundary: real HTTP, Unix socket, exec, and SQLite."""
from contextlib import closing
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
DAEMON = Path(os.environ.get("CCPD_TEST_BIN", ROOT / "target/debug/ccpd"))
LAUNCHER = Path(os.environ.get("CCP_TEST_BIN", DAEMON.with_name("ccp")))
FAKE = r'''
import json, os, pathlib, sys, uuid
t = pathlib.Path(sys.argv[0]).name
if sys.argv[1:] == ["--version"]:
    print("codex-cli 0.171.0" if t == "codex" else "2.1.329 (Claude Code)")
    sys.exit()
home = pathlib.Path(os.environ["CODEX_HOME" if t == "codex" else "CLAUDE_CONFIG_DIR"])
if t == "claude" and sys.argv[1:] == ["auth","status","--json"]:
    path = home / "fixture-identity.json"
    print(path.read_text() if path.exists() else '{"loggedIn":false}')
    sys.exit(0 if path.exists() else 1)
if t == "claude" and "--input-format" in sys.argv:
    import time
    assert sys.argv[1:] == ["--print","--verbose","--input-format","stream-json","--output-format","stream-json",
        "--no-session-persistence","--setting-sources","","--settings",'{"disableAllHooks":true}',
        "--strict-mcp-config","--mcp-config",'{"mcpServers":{}}',"--tools",""]
    assert "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC" not in os.environ
    assert os.environ.get("DISABLE_TELEMETRY") == os.environ.get("DISABLE_ERROR_REPORTING") == "1"
    assert not any(k in os.environ for k in ["CLAUDE_CODE_OAUTH_TOKEN","ANTHROPIC_API_KEY","ANTHROPIC_AUTH_TOKEN"])
    messages = [json.loads(line) for line in sys.stdin]
    assert messages == [{"type":"control_request","request_id":tag,"request":request} for tag, request in [
        ("init",{"subtype":"initialize"}),("usage",{"subtype":"get_usage","skip_behaviors":True})]]
    (home/"quota-proxies").write_text(json.dumps([os.getenv(k) for k in ("HTTP_PROXY","HTTPS_PROXY","http_proxy","https_proxy")]))
    with (home/"quota-count").open("a") as stream: stream.write("fetch\n")
    while (home/"hold-fetch").exists(): time.sleep(.02)
    if (home/"raw-output").exists():
        sys.stdout.write((home/"raw-output").read_text())
        sys.exit()
    def reply(tag,v):
        print(json.dumps({"type":"control_response","response":{"request_id":tag,"subtype":"success","response":v}}))
    reply("init",{"commands":[]})
    fixture = home/"usage.json"
    limits = json.loads(fixture.read_text()) if fixture.exists() else {"five_hour":{"utilization":25,"resets_at":"2030-01-01T00:00:00Z"}}
    reply("usage",{"subscription_type":"max","rate_limits_available":True,"rate_limits":limits,
                   "behaviors":None,"session":{"private":"fixture-private"}})
    sys.exit()
if sys.argv[1:2] == ["app-server"]:
    if (home/"track-child").exists(): (home/"child-pid").write_text(str(os.getpid()))
    for line in sys.stdin:
        r = json.loads(line)
        if "id" not in r: continue
        result = {}
        if r["method"] == "initialize":
            result = {"userAgent":"fixture/0.171.0","codexHome":str(home),
                      "platformFamily":"unix","platformOs":"linux"}
            if (home/"wrong-capabilities").exists(): result["changedProtocol"]=True
        if r["method"] == "account/read":
            assert r["params"] == {"refreshToken":False}
            path = home / "fixture-identity.json"
            result = json.loads(path.read_text()) if path.exists() else {"account":None}
            checked_identity = True
        if r["method"] == "thread/list":
            source = home / "listed-fixture.json"
            rows = json.loads(source.read_text()) if source.is_file() else []
            result = {"data":rows, "nextCursor":None, "backwardsCursor":None}
        if r["method"] in ("thread/start","thread/resume","thread/fork","thread/read","unrelated"):
            assert r.get("params",{}).get("path") is None
            uid = r.get("params",{}).get("threadId",str(uuid.uuid4()))
            parent = uid if r["method"] == "thread/fork" else None
            if parent: uid = str(uuid.uuid4())
            result = {"thread":{"id":uid,"sessionId":uid,"ephemeral":r.get("params",{}).get("ephemeral",False),
                                "forkedFromId":parent,"cwd":os.getcwd()}, "home":str(home), "proxy":os.getenv("HTTPS_PROXY"),
                      "markers":[os.getenv("CCP_LAUNCH_ID"),os.getenv("CCP_OBSERVE_TICKET")]}
        if r["method"] == "account/login/start":
            assert r["params"] == {"type":"chatgptDeviceCode"}
            import threading, time
            (home / "login-env").write_text(json.dumps([str(home),os.getenv("HTTPS_PROXY")]))
            if (home / "login-fails").exists():
                print(json.dumps({"id":r["id"],"error":{"message":"fixture-private-detail"}}), flush=True)
                continue
            login_id = str(uuid.uuid4())
            result = {"type":"chatgptDeviceCode","loginId":login_id,"verificationUrl":"https://auth.openai.com/codex/device","userCode":"TEST-ONLY"}
            def complete():
                wrong = home / "unrelated-login"
                while not (home / "finish-login").exists():
                    if wrong.exists():
                        print(json.dumps({"method":"account/login/completed","params":{"loginId":"unrelated","success":False}}), flush=True)
                        wrong.unlink()
                    time.sleep(.02)
                print(json.dumps({"method":"account/login/completed","params":{"loginId":login_id,"success":True}}), flush=True)
            threading.Thread(target=complete,daemon=True).start()
        if r["method"] == "account/login/cancel":
            assert r["params"]["loginId"] == login_id
            (home/"login-cancelled").touch()
        if r["method"] == "account/rateLimits/read" and r["params"] == {"excludeResetCreditDetails":False}:
            checked_details = True
            result = json.loads((home/"fixture-resets.json").read_text())
        elif r["method"] == "account/rateLimits/read":
            assert r["params"] == {"excludeResetCreditDetails":True}
            with (home / "quota-count").open("a") as f: f.write("fetch\n")
            result = {"rateLimits": {"planType":"pro", "primary":{"windowDurationMins":300,"usedPercent":100 if (home/"empty-quota").exists() else 0,"resetsAt":2000000000}}}
            if (home/"fixture-windows.json").exists():
                result["rateLimits"] = json.loads((home/"fixture-windows.json").read_text())
            result["rateLimitResetCredits"] = {"availableCount":2,"credits":None}
            if (home/"hold-old-quota").exists() and not (home/"held-old-pid").exists():
                import time
                (home/"held-old-pid").write_text(str(os.getpid()))
                while (home/"hold-old-quota").exists(): time.sleep(.02)
        if r["method"] == "account/rateLimitResetCredit/consume":
            import time
            assert checked_details and checked_identity
            assert set(r["params"]) == {"creditId","idempotencyKey"} and r["params"]["creditId"]
            assert str(uuid.UUID(r["params"]["idempotencyKey"])) == r["params"]["idempotencyKey"]
            with (home/"reset-consumes.jsonl").open("a") as f:
                print(json.dumps(dict(params=r["params"],home=str(home),cwd=os.getcwd(),pid=os.getpid(),proxy=os.getenv("HTTPS_PROXY"))),file=f)
            while (home/"hold-reset").exists(): time.sleep(.02)
            if (home/"lose-reset-reply").exists(): sys.exit()
            setting = home/"reset-outcome"
            result = {"outcome":setting.read_text() if setting.exists() else "reset"}
        if r["method"] == "config/read":
            import tomllib
            assert r["params"] == {"cwd":os.getcwd(),"includeLayers":False}
            config = home / "config.toml"
            parsed = tomllib.loads(config.read_text()) if config.exists() else {}
            result = {"config":dict({"cli_auth_credentials_store":"file","profile":None,"project_root_markers":[".git"]},**parsed)}
            if (home/"incomplete-config-reply").exists(): del result["config"]["cli_auth_credentials_store"]
        print(json.dumps({"id": r["id"], "result": result}), flush=True)
    sys.exit()
print(json.dumps({"argv":sys.argv[1:],"home":str(home),"id":None,"proxy":os.getenv("HTTPS_PROXY")}))
'''

@unittest.skipUnless(DAEMON.is_file(), "build ccpd first")
class Service(unittest.TestCase):
    password = "a disposable panel password"
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
        if not getattr(self, "native", False):
            shutil.copyfile(ROOT / "tests/fixtures/agy.py", paths / "agy")
            (paths / "agy").chmod(0o700)
        self.env = {"PATH": str(paths) + ":/usr/bin:/bin", "HOME": str(self.root),
                    "XDG_RUNTIME_DIR": str(self.root / "run"), "CC_PROFILE_DATA_PATH": str(self.root / "state/cc-profile"),
                    "TMPDIR":str(self.root/"tmp")}
        (self.root/"tmp").mkdir()
        self.scratch = self.root/"tmp"/f"cc-profile-{os.getuid()}"
        if getattr(self, "native", False):
            dirs = [str(Path(p).parent) for p in (shutil.which(t) for t in ("claude", "codex", "node")) if p]
            self.env["PATH"] = ":".join(dirs + ["/usr/bin", "/bin"])
            self.env["HTTP_PROXY"] = self.env["HTTPS_PROXY"] = "http://127.0.0.1:9"
            self.env["NO_PROXY"] = "localhost,127.0.0.1"
        self.start()
        self.addCleanup(self.stop)
        self.session = ""
        if getattr(self, "claim", True):
            status, data, headers = self.api("POST", "setup", {"password": self.password})
            self.assertEqual(status, 200, data)
            self.assertNotIn("set-cookie",headers)
            self.session = data["session"]

    def start(self, *args, warning=None, listen=None):
        if listen is not None:
            args = ("--listen", listen, *args)
        self.p = subprocess.Popen([str(getattr(self, "binary", DAEMON)), "--port", "0", *args], env=self.env, cwd=self.root, stderr=subprocess.PIPE, text=True)
        if warning is not None:
            self.assertIn(warning, self.p.stderr.readline())
        line = self.p.stderr.readline()
        host = listen or "127.0.0.1"
        self.assertIn(f"http://{'[' + host + ']' if ':' in host else host}:", line, line)
        self.port = int(re.search(r":(\d+)$", line.strip())[1])

    def stop(self):
        self.p.terminate()
        self.p.wait(timeout=5)
        self.p.stderr.close()

    def api(self, method, route, body=None, extra=None, *, connect_host="127.0.0.1", raw=False):
        conn = http.client.HTTPConnection(connect_host, self.port, timeout=8)
        headers = {"Origin": "http://127.0.0.1:%s" % self.port, "Content-Type": "application/json",
                   "X-CCP-Session": self.session}
        headers.update(extra or {})
        try:
            payload = body if raw or body is None else json.dumps(body)
            conn.request(method, "/api/v1/" + route, payload, headers)
            r = conn.getresponse()
            body = r.read()
            data = json.loads(body) if r.headers.get_content_type()=="application/json" else body.decode()
            return r.status, data, {k.lower(): v for k, v in r.getheaders()}
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
        return subprocess.run([str(LAUNCHER), *args], env=self.env,
                              text=True, capture_output=True, stdin=subprocess.DEVNULL, cwd=self.root,
                              timeout=30 if getattr(self,"native",False) else 8)

    def test_default_account_messages_preserve_api_and_history_codes(self):
        profile = self.account("claude", "work")
        route = "current/claude"

        def maintenance():
            return self.cli("claude", "auth", "status")

        denied = maintenance()
        self.assertNotEqual(denied.returncode, 0)
        self.assertIn("Set a default account", denied.stderr)
        self.assertEqual(self.api("PUT", route, {"config_id": profile["id"]})[:2], (200, {}))
        self.assertTrue(self.api("GET", "accounts?tool=claude")[1]["items"][0]["current"])
        self.assertEqual(maintenance().returncode, 0)
        Path(profile["home"]).rename(self.root / "moved")
        missing = maintenance()
        self.assertNotEqual(missing.returncode, 0)
        self.assertIn("default account directory", missing.stderr)
        status, error, _ = self.api("PUT", route, {"config_id": profile["id"]})
        self.assertEqual((status, error["error"]), (409, "not_eligible"))
        self.assertIn("default account", error["message"])
        self.assertEqual(self.api("PUT", route, {"config_id": None})[:2], (200, {}))
        self.assertFalse(self.api("GET", "accounts?tool=claude")[1]["items"][0]["current"])
        rows = self.api("GET", "startups?tool=claude")[1]["items"]
        self.assertEqual([r["reason"] for r in rows], ["current_missing", "current", "no_current"])

    def test_provider_aliases_route_with_distinct_daemon_and_shell_paths(self):
        for tool in ("codex", "claude"):
            c = self.account(tool, tool + "-work")
            self.api("PATCH", "accounts/"+str(c["id"]), {"revision":c["revision"],
                     "proxy_mode":"custom", "proxy_url":"http://proxy.invalid:8888"})
            self.assertEqual(self.api("PUT", "current/"+tool, {"config_id":c["id"]})[0], 200)
        self.stop()
        libexec = self.root / "libexec"
        libexec.mkdir()
        self.binary = shutil.copy2(DAEMON, libexec / "ccpd")
        package = self.root / "package"
        package.mkdir()
        codex = self.root / "bin/codex"
        codex.rename(package / "codex")
        entry = package / "entry.sh"
        entry.write_text('#!/bin/sh\nexec "${0%/*}/codex" "$@"\n')
        entry.chmod(0o700)
        codex.symlink_to(entry)
        aliases = self.root / "aliases"
        aliases.mkdir()
        (aliases / "codex").symlink_to(LAUNCHER)
        (aliases / "claude").symlink_to(LAUNCHER)
        (self.root / "bin/ccp").symlink_to(LAUNCHER)
        path = self.env["PATH"]
        self.env["PATH"] = f"{aliases}:{path}"
        self.start()
        def run(args, tool, expected):
            p = subprocess.run(args, env=self.env, cwd=self.root, capture_output=True, text=True, timeout=5)
            self.assertEqual(p.returncode, 0, p.stderr)
            result = json.loads(p.stdout)
            self.assertEqual((result["argv"],result["proxy"],result["home"]),
                             (expected, "http://proxy.invalid:8888/", str(self.root/(tool+"-work"))))
        run(["codex", "login", "status"], "codex", ["login", "status"])
        self.env["PATH"] = path
        run(["/bin/sh", "-c", "alias claude='ccp claude'\nclaude auth status"], "claude", ["auth", "status"])

    def test_explicit_native_binary_never_searches_past_a_mismatch(self):
        c = self.account("codex", "work")
        self.api("PUT", "current/codex", {"config_id":c["id"]})
        self.stop()
        self.binary = shutil.copy2(DAEMON, self.root / "ccpd")
        front = self.root / "front"
        front.mkdir()
        candidate = front / "codex"
        candidate.write_text('#!/bin/sh\ntouch "$HOME/first-candidate-ran"\necho "codex-cli older"\n')
        candidate.chmod(0o700)
        native = self.root / "bin/codex"
        path = self.env["PATH"]
        self.env["PATH"] = f"{front}:{path}"
        self.start("--codex-bin", native)
        self.assertIn("Native tool", self.cli("codex","login","status").stderr)
        self.assertFalse((self.root / "first-candidate-ran").exists())
        self.env["PATH"] = path
        self.assertEqual(self.cli("codex","login","status").returncode, 0)
        self.stop()
        self.env["PATH"] = f"{front}:{path}"
        self.start()
        self.assertTrue((self.root / "first-candidate-ran").exists())
        self.env["PATH"] = path
        self.assertIn("Native tool", self.cli("codex","login","status").stderr)
        candidate.unlink()
        candidate.symlink_to(LAUNCHER)
        self.env["PATH"] = f"{front}:{path}"
        not_executable = self.root / "not-executable"
        not_executable.touch(mode=0o600)
        for binary, warning in [(candidate, "launcher alias"), ("codex", "launcher alias"),
                                (front/"missing", None), (not_executable, None)]:
            with self.subTest(binary=binary):
                self.stop()
                self.start("--codex-bin", binary, warning=warning)
                self.assertIn("Native tool", self.cli("codex","login","status").stderr)

    def test_forwarding_script_version_probe_cannot_register_the_launcher(self):
        c = self.account("claude", "work")
        self.api("PUT", "current/claude", {"config_id":c["id"]})
        self.stop()
        front = self.root / "front"
        front.mkdir()
        wrapper = front / "claude"
        wrapper.write_text('#!/bin/sh\nexec ccp claude "$@"\n')
        wrapper.chmod(0o700)
        (front / "ccp").symlink_to(LAUNCHER)
        self.env["PATH"] = f"{front}:{self.env['PATH']}"
        self.start(warning="invokes the launcher")
        p = self.cli("claude","auth","status")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("Native tool", p.stderr)

    def test_refresh_binary_versions_reprobes_unchanged_entry_point_without_accounts(self):
        entry = self.root / "bin/codex"
        version = entry.with_name("installed-version")
        version.write_text("codex-cli 1.2.3\n")
        entry.write_text('#!/bin/sh\n[ "$1" = --version ] && cat "$(dirname "$0")/installed-version"\n')
        def read(method, extra=None):
            return self.api(method, "versions", {} if method == "POST" else None, extra=extra)
        initial = read("GET")[1]
        self.assertEqual(set(initial), {"claude","codex","agy"})
        self.assertEqual(initial["codex"], {"version":"codex-cli 1.2.3","supported":True})
        version.write_text("codex-cli 1.2.4\n")
        self.assertEqual(read("POST", {"X-CCP-Session":""})[0], 403)
        self.assertEqual(read("GET")[1]["codex"], initial["codex"])
        self.assertEqual(read("POST")[1]["codex"]["version"], "codex-cli 1.2.4")
        entry.unlink()
        self.assertEqual(read("GET")[1]["codex"], {"version":"","supported":False})
        self.assertEqual(self.api("GET","startups")[1]["items"], [])

    def test_versioned_install_symlink_upgrades_without_restart(self):
        for tool, previous, version, args in [
            ("claude", "2.1.329", "3.0.0", ["-p", "hello"]),
            ("codex", "0.171.0", "1.0.0-beta.2", ["exec", "--ephemeral", "hello"]),
        ]:
            with self.subTest(tool=tool):
                c = self.account(tool, "upgrade-" + tool)
                entry = self.root/"bin"/tool
                old = self.root/"previous"/tool
                old.parent.mkdir(exist_ok=True)
                entry.rename(old)
                entry.symlink_to(old)
                self.assertEqual(self.cli(tool, *args).returncode, 0)
                new = self.root/version/tool
                new.parent.mkdir()
                source = old.read_text().replace(previous, version)
                new.write_text(source.replace('if sys.argv[1:] == ["--version"]:',
                    'if sys.argv[1:] == ["--version"]:\n    with pathlib.Path(__file__).with_name("probes").open("a") as f: f.write("v\\n")'))
                new.chmod(0o700)
                replacement = entry.with_suffix(".new")
                replacement.symlink_to(new)
                replacement.replace(entry)  # The old target still exists and is executable.
                p = self.cli(tool, *args)
                self.assertEqual(p.returncode, 0, p.stderr)
                self.assertEqual(json.loads(p.stdout)["home"], c["home"])
                data = self.api("GET", "accounts?tool=" + tool)[1]
                self.assertTrue(data["supported"])
                self.assertIn(version, data["version"])
                self.assertEqual(new.with_name("probes").read_text(), "v\n")
                new.write_text('#!/bin/sh\nexit 1\n')
                self.assertFalse(self.api("GET", "accounts?tool=" + tool)[1]["supported"])
                self.assertNotEqual(self.cli(tool, *args).returncode, 0)
                new.write_text(source)
                self.assertEqual(self.cli(tool, *args).returncode, 0)

    def test_creating_and_signing_in_a_second_claude_profile(self):
        work = self.account("claude", "work")
        self.assertEqual(self.api("PUT", "current/claude", {"config_id": work["id"]})[0], 200)
        create = {"tool":"claude","label":"Personal","create": True,
                  "proxy_mode":"custom","proxy_url":"http://fixture.invalid:3128"}
        status, personal, _ = self.api("POST", "accounts", create)
        self.assertEqual(status, 201, personal)
        home = Path(personal["home"])
        self.assertEqual(home.parent, Path(self.env["CC_PROFILE_DATA_PATH"])/"accounts/claude")
        self.assertEqual(home.stat().st_mode & 0o777, 0o700)
        self.assertEqual(list(home.iterdir()), [])
        self.assertFalse(personal["enabled"] or personal["current"])
        self.assertEqual(self.api("POST", "accounts", create)[0], 409)
        self.assertEqual(list(home.parent.iterdir()), [home])
        other = self.api("POST", "accounts", dict(create, label="Another"))[1]
        self.assertNotEqual(other["home"], personal["home"])
        self.assertEqual(self.api("POST", "accounts", dict(create, home=work["home"]))[0], 400)
        for options, op in [([], "login"), (["--status"], "status")]:
            p = self.cli("login", str(personal["id"]), *options)
            self.assertEqual(p.returncode, 0, p.stderr)
            result = json.loads(p.stdout)
            self.assertEqual((result["home"],result["argv"],result["proxy"]),
                             (str(home),["auth",op],"http://fixture.invalid:3128/"))
        self.assertEqual(self.cli("login", str(personal["id"]), "chat").returncode, 2)
        self.assertEqual(json.loads(self.cli("claude","auth","status").stdout)["home"],work["home"])
        self.assertEqual(self.api("GET","sessions")[1]["items"], [])

    def test_hook_free_stdio_rotates_per_server_and_resumes_by_owner(self):
        a,b = [self.account("codex",label) for label in ("first","second")]
        self.api("PATCH", "accounts/"+str(a["id"]), {"revision":a["revision"],
                 "proxy_mode":"custom", "proxy_url":"http://proxy.invalid:8888"})
        self.env["CCP_LAUNCH_ID"] = self.env["CCP_OBSERVE_TICKET"] = "inherited"
        def server(args, calls):
            wire = '{"id":"hello","method":"initialize","params":{}}\n{"method":"initialized"}\n'
            wire += "".join(json.dumps({"id":i,"method":method,"params":params})+"\n"
                           for i,(method,params) in enumerate(calls))
            p=subprocess.run([LAUNCHER,"codex","app-server",*args],input=wire,
                             env=self.env,cwd=self.root,capture_output=True,text=True,timeout=8)
            self.assertEqual(p.returncode,0,p.stderr)
            return [json.loads(line)["result"] for line in p.stdout.splitlines()[1:]]
        one,two,ephemeral,noise = server([], [("thread/start",{}),("thread/start",{}),
                                               ("thread/start",{"ephemeral":True}),("unrelated",{})])
        self.assertEqual((one["home"],two["home"],two["proxy"],one["markers"]),
                         (a["home"],a["home"],"http://proxy.invalid:8888/",[None,None]))
        ids={r["thread"]["id"] for r in (one,two)}
        self.assertEqual({r["native_id"] for r in self.api("GET","sessions")[1]["items"]},ids)
        self.assertEqual(server([], [("thread/start",{})])[0]["home"], b["home"])
        uid=one["thread"]["id"]
        resumed,=server(["--for-session",uid],[("thread/resume",{"threadId":uid})])
        self.assertEqual(resumed["home"],a["home"])
        self.assertFalse((Path(a["home"])/"hooks.json").exists())
        self.assertNotEqual(self.cli("codex","app-server","--for-session",str(uuid.uuid4())).returncode,0)
        self.assertNotEqual(self.cli("codex","app-server","--listen","ws://0.0.0.0:3456").returncode,0)
        self.assertNotEqual(self.cli("codex","--hook-free").returncode,0)

    def test_explicit_server_cleanup_and_direct_tui_startup(self):
        home=Path(self.account("codex","fixture")["home"])
        (home/"track-child").touch()
        with subprocess.Popen([LAUNCHER,"codex","app-server"],cwd=self.root,env=self.env,
                              stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE) as p:
            try:
                p.stdin.write(b'{"id":0,"method":"initialize","params":{}}\n{"method":"initialized"}\n'
                              b'{"id":1,"method":"thread/start","params":{"ephemeral":true}}\n')
                p.stdin.flush()
                for _ in range(100):
                    if (home/"child-pid").exists():break
                    time.sleep(.03)
                pid=int((home/"child-pid").read_text())
            finally:
                p.terminate()
            self.assertEqual(p.wait(timeout=5),143)
            with self.assertRaises(ProcessLookupError):os.kill(pid,0)
        # The same stdio-only fixture works as a plain TUI: no CCP socket is required.
        tui=self.cli("codex")
        self.assertEqual(tui.returncode,0,tui.stderr)
        self.assertEqual(json.loads(tui.stdout)["argv"],[])
        self.assertFalse(list((self.root/"run/cc-profile").glob("c-*")))
        self.assertEqual(self.api("GET","sessions")[1]["items"],[])
        log = self.api("GET","startups")[1]["items"][0]
        self.assertEqual((log["state"],log["native_id"]),("routed",None))

    def test_saved_codex_file_routes_native_and_explicit_server_without_claiming_a_start(self):
        a,b = [self.account("codex", name) for name in ("first","second")]
        uid = str(uuid.uuid4())
        relative = Path("sessions/2026/11/23") / f"rollout-2026-11-23T12-00-00-{uid}.jsonl"
        saved = Path(b["home"])/relative
        saved.parent.mkdir(parents=True)
        saved.write_bytes(b"not a transcript")
        deadline = time.monotonic()+8
        while not (rows := self.api("GET","sessions?tool=codex")[1]["items"]) and time.monotonic() < deadline:
            time.sleep(.05)
        row, = rows
        self.assertEqual((row["config_id"],row["native_id"],row["evidence"],row["workspace"],row["last_routed_ms"]),
                         (b["id"],uid,"saved_file",None,None))
        self.assertEqual(self.api("GET","startups")[1]["items"],[])
        resume = self.cli("codex","resume",uid)
        self.assertEqual(resume.returncode,0,resume.stderr)
        self.assertEqual(json.loads(resume.stdout)["home"],b["home"])
        args=[LAUNCHER,"codex","app-server","--for-session",uid]
        wire=json.dumps({"id":1,"method":"thread/resume","params":{"threadId":uid}})+"\n"
        reply=subprocess.run(args,input=wire,env=self.env,cwd=self.root,text=True,capture_output=True,timeout=8)
        self.assertEqual(reply.returncode,0,reply.stderr)
        self.assertEqual(json.loads(reply.stdout)["result"]["home"],b["home"])
        self.assertEqual(self.api("GET","sessions?tool=codex")[1]["items"][0]["evidence"],"saved_file")
        copy=Path(a["home"])/relative
        copy.parent.mkdir(parents=True)
        copy.write_bytes(b"duplicate")
        self.assertNotEqual(self.cli("codex","resume",uid).returncode,0)
        self.assertEqual(self.api("GET","startups")[1]["items"][0]["reason"],"rollout_conflict")
        copy.unlink()
        saved.unlink()
        self.assertNotEqual(self.cli("codex","resume",uid).returncode,0)
        self.assertEqual(self.api("GET","startups")[1]["items"][0]["reason"],"rollout_pruned")
        self.assertEqual(self.api("GET","sessions?tool=codex")[1]["items"][0]["config_id"],b["id"])

    def test_old_data_home_symlink_is_not_used_for_new_profiles(self):
        data = self.root / "relocated-share"
        data.mkdir()
        share = self.root / ".local/share"
        share.parent.mkdir(exist_ok=True)
        share.symlink_to(data, target_is_directory=True)
        status, c, _ = self.api("POST", "accounts", {"tool":"claude","label":"Personal","create":True})
        self.assertEqual(status, 201, c)
        home = Path(c["home"])
        self.assertEqual(home.parent, Path(self.env["CC_PROFILE_DATA_PATH"]) / "accounts/claude")
        self.assertTrue(c["present"])
        self.assertEqual(list(data.iterdir()), [])
        alias = data / "alias"
        alias.symlink_to(home, target_is_directory=True)
        status, error, _ = self.api("POST", "accounts", {"tool":"claude","label":"Existing","home":str(alias)})
        self.assertEqual((status, error["error"]), (400, "bad_home"))

    def test_profile_device_sign_in_lifecycle(self):
        def create(label):
            status, c, _ = self.api("POST", "accounts", {"tool":"codex","label":label,"create":True,
                                      "proxy_mode":"custom","proxy_url":"http://fixture.invalid:3128"})
            self.assertEqual(status, 201)
            return c, "accounts/%s/device-login" % c["id"]
        def wait(path, state):
            for _ in range(100):
                response = self.api("GET",path)[1]
                if response["state"] == state: return response
                time.sleep(.03)
            self.fail(response)
        personal, path = create("Personal")
        other, other_path = create("Work")
        body = {"revision":personal["revision"]}
        start = self.api("POST",path,body)[1]
        waiting = wait(path, "waiting")
        self.assertEqual(waiting["code"], "TEST-ONLY")
        self.assertEqual(start["attempt"], self.api("POST", path, body)[1]["attempt"])
        self.assertEqual(self.api("GET", other_path)[1], {"state":"idle"})
        self.assertEqual(self.api("POST", other_path, {"revision":other["revision"]})[0], 409)
        self.assertEqual(self.api("POST", path+"/cancel", {"attempt":"stale"})[0], 409)
        home = Path(personal["home"])
        self.assertEqual(json.loads((home / "login-env").read_text()),[str(home),"http://fixture.invalid:3128/"])
        self.api("POST", path+"/cancel", {"attempt":start["attempt"]})
        self.assertNotIn("code", wait(path, "cancelled"))
        self.assertTrue((home/"login-cancelled").exists())
        self.api("POST",path,body)
        wait(path, "waiting")
        wrong = home/"unrelated-login"
        wrong.touch()
        for _ in range(100):
            if not wrong.exists(): break
            time.sleep(.03)
        self.assertFalse(wrong.exists(), "fixture did not emit the unrelated completion")
        self.assertEqual(self.api("GET",path)[1]["state"], "waiting")
        (home/"finish-login").touch()
        self.assertNotIn("code", wait(path, "completed"))
        (home/"login-fails").touch()
        self.api("POST",path,body)
        self.assertNotIn("fixture-private-detail", str(wait(path, "failed")))
        accounts = self.api("GET","accounts?tool=codex")[1]["items"]
        self.assertFalse(any(c["enabled"] or c["current"] for c in accounts))
        self.assertEqual(self.api("GET","startups")[1]["items"], [])
        self.assertEqual(self.api("GET","sessions")[1]["items"], [])

    def test_codex_terminal_setup_and_status_use_named_profile_proxy(self):
        work = self.account("codex", "Work")
        self.assertEqual(self.api("PUT", "current/codex", {"config_id":work["id"]})[0], 200)
        self.env["HTTPS_PROXY"] = "http://from-shell.invalid:8888"
        for mode, expected in [("custom","http://profile.invalid:3128/"), ("inherit",self.env["HTTPS_PROXY"]), ("direct",None)]:
            data = {"create":True,"tool":"codex","label":mode,"proxy_mode":mode}
            if mode == "custom": data["proxy_url"] = "http://profile.invalid:3128"
            status, c, _ = self.api("POST", "accounts", data)
            self.assertEqual(status, 201, c)
            self.assertFalse(c["enabled"] or c["current"])
            for options, argv in [([], ["login"]), (["--status"], ["login", "status"])]:
                p = self.cli("login", str(c["id"]), *options)
                self.assertEqual(p.returncode, 0, p.stderr)
                native = json.loads(p.stdout)
                self.assertEqual((native["home"],native["argv"],native["proxy"],native["id"]),
                                 (c["home"],argv,expected,None))
        self.assertEqual(json.loads(self.cli("codex","login","status").stdout)["home"],work["home"])
        self.assertEqual(self.api("GET","sessions")[1]["items"], [])
        rows = self.api("GET","startups")[1]["items"]
        self.assertTrue(all(r["intent"] == "misc" and r["state"] == "routed" for r in rows))

    def test_claude_exec_stickiness_and_fail_closed_controls(self):
        c = self.account("claude", "one")
        self.account("claude", "two")
        p = self.cli("claude", "private prompt")
        self.assertEqual(p.returncode, 0, p.stderr)
        out = json.loads(p.stdout)
        uid = out["argv"][1]
        self.assertEqual(out["argv"][2:], ["private prompt"])
        self.assertEqual(out["home"], c["home"])
        self.assertEqual(json.loads(self.cli("claude", "-r", uid).stdout)["home"], c["home"])
        resume = ["-p", "--resume", uid, "--output-format", "json", "continue"]
        out = json.loads(self.cli("claude", *resume).stdout)
        self.assertEqual((out["home"], out["argv"]), (c["home"], resume))
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

    def test_edit_directory_keeps_the_original_session_owner_after_rename(self):
        c = self.account("claude", "work")
        other = self.account("claude", "other")
        self.assertEqual(self.api("PUT", "current/claude", {"config_id": c["id"]})[0], 200)
        c = next(a for a in self.api("GET","accounts?tool=claude")[1]["items"] if a["id"] == c["id"])
        route = f"accounts/{c['id']}"
        sid = json.loads(self.cli("claude", "hello").stdout)["argv"][1]
        original = Path(c["home"])
        moved = self.root/"new parent"/"moved account"
        moved.parent.mkdir()
        original.rename(moved)
        self.assertNotEqual(self.cli("claude","--resume",sid).returncode,0)
        symlink = self.root/"alias"
        symlink.symlink_to(moved)
        copy = self.root/"copied home"
        shutil.copytree(moved, copy)
        for path, status in [(copy,409), (other["home"],409), (symlink,400), (original,400)]:
            self.assertEqual(self.api("PATCH",route,{"revision":c["revision"],"home":str(path)})[0], status)
        self.assertEqual(self.api("PATCH",route,{"revision":c["revision"]-1,"home":str(moved)})[0],409)
        status, updated, _ = self.api("PATCH",route,{"revision":c["revision"],"home":str(moved)})
        self.assertEqual(status,200,updated)
        self.assertEqual((updated["id"],updated["label"],updated["enabled"],updated["current"],
                          updated["home"],updated["revision"]),
                         (c["id"],c["label"],True,True,str(moved),c["revision"]+1))
        self.stop()
        self.start()
        self.assertEqual(json.loads(self.cli("claude","--resume",sid).stdout)["home"],str(moved))

    def test_recreating_a_home_does_not_inherit_its_session(self):
        home=Path(self.account("claude","recycled")["home"])
        launched=self.cli("claude","first")
        uid=json.loads(launched.stdout)["argv"][1]
        home.rmdir()
        home.mkdir()
        refused=self.cli("claude","--resume",uid)
        self.assertNotEqual(refused.returncode,0)
        self.assertIn("replaced",refused.stderr)
        replacement=self.root/"new-path"
        home.rename(replacement)
        status,new,_=self.api("POST","accounts",{"tool":"claude","label":"replacement","home":str(replacement)})
        self.assertEqual(status,201,new)
        self.stop()
        self.start()
        self.assertNotEqual(self.cli("claude","--resume",uid).returncode,0)

    def test_codex_readiness_does_not_require_or_change_hooks(self):
        c=self.account("codex","work")
        path=Path(c["home"])/"hooks.json"
        path.write_text('{"hooks":{"Stop":[]}}')
        status,data,_=self.api("GET","accounts?tool=codex")
        self.assertEqual(status,200)
        self.assertEqual(data["items"][0]["readiness"],"ready")
        self.assertNotIn("observer",data)
        for op in ("install","check"):
            self.assertEqual(self.api("POST",f"accounts/{c['id']}/observer/{op}",{})[0],404)
        self.assertEqual(path.read_text(),'{"hooks":{"Stop":[]}}')
        callback=subprocess.run([DAEMON,"observe"],env=self.env,cwd=self.root,
                                stdin=subprocess.DEVNULL,capture_output=True,timeout=3)
        self.assertNotEqual(callback.returncode,0)
        import socket
        for version, code in [(1,b"404"),(2,b"426")]:
            with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
                client.settimeout(2)
                client.connect(str(self.root/"run/cc-profile/ccpd.sock"))
                client.sendall(f"POST /v{version}/launch HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n".encode())
                self.assertIn(code,client.recv(100))
        self.assertEqual(self.api("GET","startups")[1]["items"],[])

    def test_claude_bridge_aliases_do_not_allocate_session_ids(self):
        c=self.account("claude","work")
        self.api("PUT","current/claude",{"config_id":c["id"]})
        for cmd in ["remote-control","rc","remote","sync","bridge"]:
            for flags in [[], ["--help"]]:
                p=self.cli("claude",cmd,*flags)
                self.assertNotEqual(p.returncode,0,cmd)
                self.assertEqual(p.stdout,"")
        for command in ["rc","remote-control","update","doctor"]:
            p=self.cli("claude","--",command)
            self.assertNotEqual(p.returncode,0,command)
        self.assertFalse(self.api("GET","sessions")[1]["items"])
        logs=self.api("GET","startups")[1]["items"]
        self.assertEqual([row["reason"] for row in logs],["unsupported_invocation"]*14)

    def test_invalid_claude_permission_prompts_do_not_allocate_or_rotate(self):
        work = self.account("claude", "work")
        self.account("claude", "other")
        for args in [["--permission-prompts=off"], ["--permission-prompts", ""], ["--permission-prompts="]]:
            p = self.cli("claude", "-p", *args, "hello")
            self.assertNotEqual(p.returncode, 0, args)
            self.assertEqual(p.stdout, "")
        self.assertEqual(self.api("GET","sessions")[1]["items"], [])
        self.assertEqual(self.cli("claude","-p","--permission-prompts","host","hello").returncode, 0)
        self.assertEqual(self.api("GET","sessions")[1]["items"], [])
        self.assertEqual(self.api("GET","startups")[1]["items"][0]["config_id"], work["id"])

    def test_native_identity_is_profile_scoped_and_only_exposes_display_fields(self):
        for tool in ("claude","codex"):
            c = self.account(tool,tool+"-account")
            path = f"accounts/{c['id']}/identity"
            self.assertEqual(self.api("GET",path,extra={"X-CCP-Session":""})[0],403)
            self.assertEqual(self.api("GET",path)[:2],(200,{"state":"signed_out","email":None}))
            raw = {"type":"chatgpt","email":tool+"@example.test","ignored":"private-test-token"}
            path_on_disk = Path(c["home"]) / "fixture-identity.json"
            payload = dict(raw,loggedIn=True) if tool == "claude" else {"account":raw}
            path_on_disk.write_text(json.dumps(payload))
            self.assertEqual(self.api("GET",path)[:2],(200,{"state":"signed_in","email":raw["email"]}))
            raw["email"] = None
            payload = dict(raw,loggedIn=True) if tool == "claude" else {"account":raw}
            path_on_disk.write_text(json.dumps(payload))
            self.assertEqual(self.api("GET",path)[:2],(200,{"state":"signed_in","email":None}))
            path_on_disk.write_text("{invalid")
            self.assertEqual(self.api("GET",path)[:2],(200,{"state":"unavailable","email":None}))
        self.assertFalse(self.api("GET","sessions")[1]["items"])
        self.assertFalse(self.api("GET","startups")[1]["items"])

    def test_web_authenticates_reads_and_mutations(self):
        self.assertEqual(self.api("GET","accounts?tool=codex",extra={"X-CCP-Session":""})[0],403)
        for extra in [{"X-CCP-Session":""}, {"Sec-Fetch-Site":"cross-site"}, {"Sec-Fetch-Site":"same-site"},
                      {"Content-Type":"text/plain"}]:
            with self.subTest(extra=extra):
                status, _, _ = self.api("POST", "accounts", {}, extra)
                self.assertEqual(status, 403)

    def test_retired_config_links_are_not_changed_on_upgrade_or_selection(self):
        import sqlite3
        first = self.account("codex", "first")
        second = self.account("codex", "second")
        self.assertEqual(self.api("PUT","current/codex",{"config_id":first["id"]})[0], 200)
        self.stop()
        root = self.root / "profiles"
        provider = root / "codex"
        entry = provider / ".registered" / str(first["id"])
        entry.mkdir(parents=True)
        (entry / "config").symlink_to(os.path.relpath(first["home"], entry))
        (provider / "current").symlink_to(entry.relative_to(provider))
        outer = self.root / ".codex"
        outer.symlink_to("profiles/codex/current/config")
        db_path = self.root / "state/cc-profile/router.sqlite3"
        with closing(sqlite3.connect(db_path)) as db, db:
            for name in ("link_root", "outer_path", "projected"):
                db.execute(f"ALTER TABLE tools ADD COLUMN {name} TEXT")
            db.execute("UPDATE tools SET cursor=?,link_root=?,outer_path=?,projected=? WHERE tool='codex'",
                       (first["id"], str(root), str(outer), str(entry.relative_to(provider))))
            db.execute("UPDATE settings SET routing_mode='round_robin'")
            db.execute("PRAGMA user_version=8")
        self.start()
        self.session = self.api("POST","login",{"password":self.password})[1]["session"]
        with closing(sqlite3.connect(db_path)) as db:
            self.assertEqual([col[1] for col in db.execute("PRAGMA table_info(tools)")], ["tool","cursor"])
            self.assertEqual(db.execute("SELECT cursor FROM tools WHERE tool='codex'").fetchone(), (first["id"],))
        self.assertNotIn("compatibility", self.api("GET","accounts?tool=codex")[1])
        self.assertEqual(self.api("PUT","compatibility/codex",{})[0], 404)
        self.assertEqual(self.api("POST","current/codex/repair",{})[0], 404)
        self.assertEqual(self.api("PUT","current/codex",{"config_id":second["id"]})[:2], (200,{}))
        self.assertEqual(json.loads(self.cli("codex","login","status").stdout)["home"], second["home"])
        self.assertEqual(outer.resolve(strict=True), Path(first["home"]))

    def test_routing_settings_and_account_priorities_survive_restart(self):
        for method in ["GET", "PUT"]:
            self.assertEqual(self.api(method,"settings",{"routing_mode":"priority"},{"X-CCP-Session":""})[0],403)
        self.assertEqual(self.api("GET","settings")[:2],(200,{"routing_mode":"earliest_expiry_most_remaining"}))
        for mode in ["most_remaining","least_remaining","earliest_expiry_most_remaining","earliest_expiry_least_remaining","round_robin","priority"]:
            body = {"routing_mode":mode}
            self.assertEqual(self.api("PUT","settings",body)[:2],(200,body))
        for body in [{},{"routing_mode":None},{"routing_mode":"invalid"},{"routing_mode":"earliest_expiry"}]:
            self.assertEqual(self.api("PUT","settings",body)[0],400)
        first = self.account("codex","first")
        self.assertEqual(first["priority"],0)
        create = {"tool":"codex","label":"high","create":True,"priority":100}
        for bad in [-1,101,0.5,"10",True,None]:
            self.assertEqual(self.api("POST","accounts",dict(create,priority=bad))[0],400)
        status, high, _ = self.api("POST","accounts",create)
        self.assertEqual((status,high["priority"]),(201,100))
        route = f"accounts/{high['id']}"
        self.assertEqual(self.api("PATCH",route,{"revision":high["revision"],"priority":101})[0],400)
        status, high, _ = self.api("PATCH",route,{"revision":high["revision"],"enabled":True,"priority":99})
        self.assertEqual((status,high["priority"]),(200,99))
        self.stop()
        self.start()
        status, login, _ = self.api("POST","login",{"password":self.password})
        self.assertEqual(status,200)
        self.session = login["session"]
        self.assertEqual(self.api("GET","settings")[1],{"routing_mode":"priority"})
        self.assertEqual([c["priority"] for c in self.api("GET","accounts?tool=codex")[1]["items"]],[0,99])
        # Exercise the real client's validation of the new-session decision.
        p = self.cli("codex")
        self.assertEqual(p.returncode,0,p.stderr)
        row = self.api("GET","startups")[1]["items"][0]
        self.assertEqual((row["config_id"], row["reason"]), (high["id"], "priority"))

    def test_concurrent_codex_launches_wait_for_one_refresh_before_choosing(self):
        from concurrent.futures import ThreadPoolExecutor
        empty, ready = [Path(self.account("codex", n)["home"]) for n in ("empty", "ready")]
        for home in (empty, ready):
            (home/"auth.json").write_text("{}")
        (empty/"empty-quota").touch()
        hold = empty/"hold-old-quota"
        hold.touch()
        with ThreadPoolExecutor(2) as pool:
            launches = [pool.submit(self.cli, "codex") for _ in range(2)]
            try:
                for _ in range(200):
                    if (empty/"held-old-pid").exists(): break
                    time.sleep(.02)
                self.assertTrue((empty/"held-old-pid").exists())
                self.assertEqual(self.api("GET", "startups")[1]["items"], [])
                self.assertFalse(any(p.done() for p in launches))
            finally:
                hold.unlink()
            for p in launches:
                result = p.result()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(json.loads(result.stdout)["home"], str(ready))
        for home in (empty, ready):
            self.assertEqual((home/"quota-count").read_text(), "fetch\n")

    def test_split_expiry_settings_choose_different_codex_profiles_on_launch(self):
        early = int(time.time()) + 600
        profiles = []
        for label, used, reset in [("late", 0, early+300), ("soon-low", 80, early), ("soon-high", 10, early)]:
            c = self.account("codex",label)
            profiles.append(c)
            home = Path(c["home"])
            (home/"auth.json").write_text("{}")
            (home/"fixture-windows.json").write_text(json.dumps({"primary":{"usedPercent":used,"resetsAt":reset}}))
        self.assertEqual(self.api("POST","quota/refresh",{})[1]["queued"],3)
        for _ in range(200):
            rows = self.api("GET","accounts?tool=codex")[1]["items"]
            if all(r["quota"]["status"]=="fresh" for r in rows): break
            time.sleep(.02)
        else: self.fail("fixture windows did not load")
        most, least = "earliest_expiry_most_remaining", "earliest_expiry_least_remaining"
        self.assertEqual(self.api("GET","settings")[1]["routing_mode"],most)
        for mode, profile in [(most, profiles[2]), (least, profiles[1])]:
            if mode == least:
                self.assertEqual(self.api("PUT","settings",{"routing_mode":mode})[:2], (200,{"routing_mode":mode}))
            p = self.cli("codex","exec","--ephemeral","fixture")
            self.assertEqual(p.returncode,0,p.stderr)
            self.assertEqual(json.loads(p.stdout)["home"], profile["home"])
            row = self.api("GET","startups?tool=codex")[1]["items"][0]
            self.assertEqual((row["reason"],row["config_id"]),(mode,profile["id"]))

    def test_native_claude_account_usage_guides_quota_modes(self):
        profiles = [Path(self.account("claude", n)["home"]) for n in ("roomy", "scarce")]
        for home, used in zip(profiles, (20, 90)):
            # Opaque to ccpd: only the native process interprets the credential store.
            (home/".credentials.json").write_text("not JSON")
            (home/"usage.json").write_text(json.dumps({"five_hour":{"utilization":used}}))
        for mode, expected in [("least_remaining", [1]), ("most_remaining", [0]),
                               ("earliest_expiry_least_remaining", [1]), ("earliest_expiry_most_remaining", [0]),
                               ("round_robin", [1, 0]), ("priority", [1, 0])]:
            self.assertEqual(self.api("PUT","settings",{"routing_mode":mode})[0],200)
            for i in expected:
                self.assertEqual(Path(json.loads(self.cli("claude","chat").stdout)["home"]),profiles[i])
        for home in profiles:
            self.assertEqual((home/"quota-count").read_text(),"fetch\n")

    def test_failed_quota_checks_rotate_without_retrying_or_reusing_stale_scores(self):
        import sqlite3
        a, b = [Path(self.account("claude", n)["home"]) for n in ("a", "b")]
        for home, used in [(a, 10), (b, 90)]:
            (home/".credentials.json").write_text("disposable")
            (home/"usage.json").write_text(json.dumps({"five_hour": {"utilization": used}}))
        self.api("PUT", "settings", {"routing_mode": "least_remaining"})
        def launch():
            result = self.cli("claude", "chat")
            self.assertEqual(result.returncode, 0, result.stderr)
            return Path(json.loads(result.stdout)["home"])
        self.assertEqual(launch(), b)
        for home in (a, b):
            (home/"raw-output").write_text("unavailable")
        with closing(sqlite3.connect(self.root/"state/cc-profile/router.sqlite3")) as db, db:
            db.execute("UPDATE quota_cache SET success_ms=success_ms-120000, retry_ms=0")
        self.assertEqual([launch(), launch()], [a, b])
        for home in (a, b):
            self.assertEqual((home/"quota-count").read_text(), "fetch\n" * 2)

    def reset_fixture(self, name="reset-profile"):
        c = self.account("codex", name)
        route = f"accounts/{c['id']}"
        status,c,_ = self.api("PATCH",route,{"revision":c["revision"],"proxy_mode":"custom","proxy_url":"http://fixture.invalid:8111"})
        self.assertEqual(status,200)
        home = Path(c["home"])
        (home/"fixture-identity.json").write_text(json.dumps({"account":{"type":"chatgpt"},"userId":"private-user","workspaceRouting":{"chatgptAccountId":"private-account"}}))
        grants = [dict(id="opaque-credit-"+t,resetType="codexRateLimits",status="available",title=t,description="private detail",
                       grantedAt=1700000000,expiresAt=None) for t in ("One","Two")]
        summary = {"availableCount":4,"credits":grants}
        def publish(account="private-account"):
            (home/"fixture-resets.json").write_text(json.dumps({"accountId":account,"rateLimitResetCredits":summary}))
        publish()
        return c,home,route+"/resets/",summary,publish

    def reset_list(self, route):
        status,v,_ = self.api("POST",route+"list",{})
        self.assertEqual(status,200,v)
        return v

    def test_reset_list_and_explicit_confirmation(self):
        import sqlite3
        c,h,r,summary,publish = self.reset_fixture()
        def state():
            with closing(sqlite3.connect(self.root/"state/cc-profile/router.sqlite3")) as db:
                return [list(db.execute("SELECT * FROM "+table)) for table in ("configs","tools","settings","sessions","launches")]
        before = state()
        selected = summary["credits"][1]
        selected["expiresAt"] = 4102444800
        summary["credits"].extend(dict(selected,id="bad-"+str(i),**diff) for i,diff in enumerate([
            {"status":"redeeming"},{"resetType":"unknown"},{"expiresAt":1},{"expiresAt":"bad"}]))
        summary["credits"].extend([dict(selected,id="duplicate")]*2)
        publish()
        for endpoint in ("list","redeem"):
            self.assertEqual(self.api("POST",r+endpoint,{},{"X-CCP-Session":""})[0],403)
            self.assertEqual(self.api("POST",r+endpoint,{},{"Sec-Fetch-Site":"cross-site"})[0],403)
            self.assertEqual(self.api("GET",r+endpoint)[0],405)
        v = self.reset_list(r)
        self.assertEqual((v["available_count"],v["details_available"]), (4,True))
        self.assertEqual([g["title"] for g in v["grants"]], ["One","Two"])
        self.assertEqual(v["grants"][1]["expires_at_ms"],4102444800000)
        handle = v["grants"][1]["handle"]
        for g in v["grants"]:
            self.assertEqual(set(g), {"title","handle","expires_at_ms"})
            uuid.UUID(g["handle"])
        self.assertEqual(self.reset_list(r),v)
        self.assertFalse((h/"reset-consumes.jsonl").exists())
        (h/"track-child").touch()
        for body in ({}, [handle], {"creditId":selected["id"]}, {"handle":handle,"idempotencyKey":"client-key"}):
            status,data,_ = self.api("POST",r+"redeem",body)
            self.assertEqual((status,data["error"]),(400,"bad_request"))
        duplicate = '{"handle":"%s","handle":"%s"}' % (v["grants"][0]["handle"],handle)
        status,data,_ = self.api("POST",r+"redeem",duplicate,raw=True)
        self.assertEqual((status,data.get("error")),(400,"bad_request"))
        self.assertFalse((h/"child-pid").exists())
        self.assertFalse((h/"reset-consumes.jsonl").exists())
        self.assertEqual(self.api("POST",r+"redeem",{"handle":handle})[:2], (200,{"outcome":"reset"}))
        def mutations():
            return [json.loads(row) for row in (h/"reset-consumes.jsonl").read_text().splitlines()]
        [actual] = mutations()
        self.assertEqual((actual["params"]["creditId"],actual["home"],actual["proxy"],actual["cwd"]),
                          (selected["id"],str(h),"http://fixture.invalid:8111/",str(self.root/"state/cc-profile")))
        self.assertEqual([g["title"] for g in self.reset_list(r)["grants"]],["One"])
        summary["credits"] = None
        publish()
        self.assertEqual(self.reset_list(r),{"available_count":4,"details_available":False,"grants":[]})
        self.assertEqual(self.api("POST",r+"redeem",{"handle":handle})[:2],(200,{"outcome":"reset"}))
        summary.update(availableCount=0,credits=[])
        publish()
        self.assertEqual(self.reset_list(r),{"available_count":0,"details_available":True,"grants":[]})
        (h/"fixture-resets.json").write_text('{"accountId":"private-account"}')
        self.assertEqual(self.reset_list(r),{"available_count":None,"details_available":False,"grants":[]})
        self.assertEqual(len(mutations()),1)
        self.assertEqual(state(),before)
        with closing(sqlite3.connect(self.root/"state/cc-profile/router.sqlite3")) as db:
            stored = str(list(db.execute("SELECT * FROM reset_grants")))
            for raw in ("opaque-credit-","private-account","private detail","private-user"):
                self.assertNotIn(raw,stored)

    def test_reset_refuses_changed_identity_grant_profile_and_binary(self):
        c,h,r,summary,publish = self.reset_fixture()
        other,_,other_route,_,_ = self.reset_fixture("other")
        handle = self.reset_list(r)["grants"][0]["handle"]
        def error(code,route=r):
            status,v,_ = self.api("POST",route+"redeem",{"handle":handle})
            self.assertEqual((status,v["error"]),(409,code),v)
        error("stale_config",other_route)
        login = h/"fixture-identity.json"
        original = login.read_text()
        for who in [None,{"account":{"type":"apiKey"}},{"account":{"type":"chatgpt"},"userId":"different"}]:
            login.write_text(json.dumps(who))
            error("stale_config")
        login.write_text(original)
        publish("other-workspace")
        error("stale_config")
        login.write_text(json.dumps({"account":{"type":"chatgpt"},"userId":"private-user"}))
        error("stale_config")
        login.write_text(original)
        publish(None)
        error("stale_config")
        grant = summary["credits"][0]
        for field,val in [("title","changed"),("expiresAt",1),("status","redeemed"),("resetType","unknown")]:
            old=grant[field]
            grant[field]=val
            publish()
            error("stale_grant")
            grant[field]=old
        summary["credits"] = [summary["credits"][1]]
        publish()
        error("stale_grant")
        self.assertEqual(self.api("PATCH",f"accounts/{c['id']}",{"revision":c["revision"],"label":"changed"})[0],200)
        error("stale_config")
        handle = self.reset_list(r)["grants"][0]["handle"]
        (self.root/"bin/codex").touch()
        error("stale_config")
        self.assertNotEqual(self.reset_list(r)["grants"][0]["handle"], handle)
        self.assertFalse((h/"reset-consumes.jsonl").exists())

    def test_reset_retries_share_durable_keys_and_serialize_consumes(self):
        from concurrent.futures import ThreadPoolExecutor
        c,h,r,summary,publish = self.reset_fixture()
        _,other,r2,_,_ = self.reset_fixture("shared")
        a,b = self.reset_list(r)["grants"]
        other_a = self.reset_list(r2)["grants"][0]
        def redeem(route, grant):
            return self.api("POST",route+"redeem",{"handle":grant["handle"]})[:2]
        (h/"hold-reset").touch()
        (h/"lose-reset-reply").touch()
        with ThreadPoolExecutor(1) as pool:
            pending = pool.submit(redeem,r,a)
            try:
                for _ in range(150):
                    if (h/"reset-consumes.jsonl").exists(): break
                    time.sleep(.02)
                self.assertTrue((h/"reset-consumes.jsonl").exists())
                self.assertEqual(redeem(r,b)[1]["error"],"busy")
                self.assertEqual(redeem(r2,other_a)[1]["error"],"busy")
            finally:
                (h/"hold-reset").unlink()
            self.assertEqual(pending.result()[1]["error"],"outcome_unknown")
        self.stop()
        self.start()
        self.session = self.api("POST","login",{"password":self.password})[1]["session"]
        self.assertEqual(self.reset_list(r)["grants"][0],a)
        grant=summary["credits"].pop(0)
        publish()
        self.assertEqual(redeem(r,a)[1]["error"],"stale_grant")
        summary["credits"].insert(0,grant)
        publish()
        self.assertEqual(self.reset_list(r)["grants"][0],a)
        self.assertEqual(self.reset_list(r2)["grants"][0],other_a)
        (other/"reset-outcome").write_text("alreadyRedeemed")
        self.assertEqual(redeem(r2,other_a),(200,{"outcome":"alreadyRedeemed"}))
        self.assertEqual(json.loads((h/"reset-consumes.jsonl").read_text())["params"],
                         json.loads((other/"reset-consumes.jsonl").read_text())["params"])
        self.assertEqual(redeem(r,a),(200,{"outcome":"alreadyRedeemed"}))
        (h/"lose-reset-reply").unlink()
        for outcome in ("nothingToReset","noCredit"):
            (h/"reset-outcome").write_text(outcome)
            b = self.reset_list(r)["grants"][0]
            self.assertEqual(b["title"],"Two")
            self.assertEqual(redeem(r,b),(200,{"outcome":outcome}))
            self.assertEqual(redeem(r,b),(200,{"outcome":outcome}))
        attempts=[json.loads(line)["params"] for line in (h/"reset-consumes.jsonl").read_text().splitlines()]
        self.assertEqual([p["creditId"] for p in attempts],["opaque-credit-One","opaque-credit-Two","opaque-credit-Two"])
        self.assertEqual(len({p["idempotencyKey"] for p in attempts}),3)

    def test_reset_supersedes_in_flight_quota_fetch(self):
        c,h,r,_,_ = self.reset_fixture()
        (h/"empty-quota").touch()
        (h/"hold-old-quota").touch()
        self.assertEqual(self.api("POST",f"accounts/{c['id']}/quota/refresh",{})[1],{"queued":1})
        def wait(check):
            for _ in range(200):
                if check(): return
                time.sleep(.02)
            self.fail("timed out waiting for quota observation")
        def used():
            q=self.api("GET","accounts?tool=codex")[1]["items"][0]["quota"]
            return q["snapshot"]["windows"][0]["used_pct"] if q["status"]=="fresh" else None
        try:
            wait(lambda: (h/"held-old-pid").exists())
            handle=self.reset_list(r)["grants"][1]["handle"]
            (h/"empty-quota").unlink()
            self.assertEqual(self.api("POST",r+"redeem",{"handle":handle})[0],200)
            wait(lambda: used()==0)
        finally:
            (h/"hold-old-quota").unlink()
        wait(lambda: not Path("/proc/"+(h/"held-old-pid").read_text()).exists())
        self.assertEqual(used(),0)
        self.assertEqual((h/"quota-count").read_text(),"fetch\nfetch\n")

    def test_unknown_reset_fences_aliases_and_registers_refresh_before_reply(self):
        import sqlite3
        c,h,r,_,_ = self.reset_fixture()
        alias,a,ar,summary,publish = self.reset_fixture("same-account")
        # A capped list need not include the selected credit on the alias.
        summary["credits"] = [summary["credits"][1]]
        publish()
        self.reset_list(ar)
        selected=self.reset_list(r)["grants"][0]
        def quotas():
            return {x["id"]:x["quota"] for x in self.api("GET","accounts?tool=codex")[1]["items"]}
        def wait(check):
            for _ in range(200):
                if check(): return
                time.sleep(.02)
            self.fail("timed out waiting for quota job")
        for profile in (c,alias):
            (Path(profile["home"])/"empty-quota").touch()
            self.assertEqual(self.api("POST",f"accounts/{profile['id']}/quota/refresh",{})[1],{"queued":1})
        wait(lambda: all(q["status"]=="fresh" and not q["loading"] for q in quotas().values()))
        with closing(sqlite3.connect(self.root/"state/cc-profile/router.sqlite3")) as db, db:
            db.execute("UPDATE quota_cache SET retry_ms=0 WHERE config_id=?",[alias["id"]])
        (a/"hold-old-quota").touch()
        (h/"hold-old-quota").touch()
        try:
            self.api("POST",f"accounts/{alias['id']}/quota/refresh",{})
            wait(lambda: (a/"held-old-pid").exists())
            self.assertEqual(self.api("PATCH",f"accounts/{alias['id']}",{"label":"renamed","revision":alias["revision"]})[0],200)
            self.assertEqual(quotas()[alias["id"]]["snapshot"]["windows"][0]["used_pct"],100)
            (h/"lose-reset-reply").touch()
            (h/"empty-quota").unlink()
            status,error,_=self.api("POST",r+"redeem",{"handle":selected["handle"]})
            self.assertEqual((status,error["error"]),(503,"outcome_unknown"))
            q=quotas()
            self.assertEqual((q[c["id"]]["status"],q[c["id"]]["loading"]),("not_checked",True))
            self.assertEqual((q[alias["id"]]["status"],q[alias["id"]]["loading"]),("not_checked",False))
        finally:
            (a/"hold-old-quota").unlink()
            (h/"hold-old-quota").unlink()
        wait(lambda: quotas()[c["id"]]["status"]=="fresh")
        wait(lambda: not Path("/proc/"+(a/"held-old-pid").read_text()).exists())
        self.assertEqual(quotas()[c["id"]]["snapshot"]["windows"][0]["used_pct"],0)
        self.assertEqual(quotas()[alias["id"]]["status"],"not_checked")
        self.assertEqual((a/"quota-count").read_text(),"fetch\nfetch\n")
        self.assertEqual(self.reset_list(r)["grants"][0],selected)
        self.assertEqual(len((h/"reset-consumes.jsonl").read_text().splitlines()),1)

    def test_confirmed_reset_registers_only_verifiable_alias_refreshes(self):
        c,h,r,_,_=self.reset_fixture()
        file_alias,f,fr,_,_=self.reset_fixture("file-alias")
        keyring,k,kr,_,_=self.reset_fixture("keyring-alias")
        (k/"config.toml").write_text('cli_auth_credentials_store="keyring"')
        for profile,home,route in ((file_alias,f,fr),(keyring,k,kr)):
            (home/"auth.json").write_text("{}")
            self.reset_list(route)
            self.api("POST",f"accounts/{profile['id']}/quota/refresh",{})
        def quotas():
            return {p["id"]:p["quota"] for p in self.api("GET","accounts?tool=codex")[1]["items"]}
        for _ in range(200):
            q=quotas()
            if all(q[p["id"]]["status"]=="fresh" and not q[p["id"]]["loading"] for p in (file_alias,keyring)): break
            time.sleep(.02)
        else: self.fail("initial alias observations failed")
        selected=self.reset_list(r)["grants"][1]
        for home in (h,f): (home/"hold-old-quota").touch()
        try:
            self.assertEqual(self.api("POST",r+"redeem",{"handle":selected["handle"]})[:2], (200,{"outcome":"reset"}))
            q=quotas()
            for profile in (c,file_alias):
                self.assertEqual((q[profile["id"]]["status"],q[profile["id"]]["loading"]),("not_checked",True))
            self.assertEqual((q[keyring["id"]]["status"],q[keyring["id"]]["loading"]),("not_checked",False))
            self.assertEqual((k/"quota-count").read_text(),"fetch\n")
        finally:
            for home in (h,f): (home/"hold-old-quota").unlink()
        for _ in range(200):
            if not any(q["loading"] for q in quotas().values()): break
            time.sleep(.02)
        else: self.fail("post-reset checks did not finish")
        self.assertEqual(quotas()[keyring["id"]]["status"],"not_checked")
        self.assertEqual((k/"quota-count").read_text(),"fetch\n")

    def test_quota_is_cached_without_consuming_a_rotation_or_starting_a_chat(self):
        cl = self.account("claude", "one")
        cx = self.account("codex", "two")
        self.assertEqual(self.api("PATCH", "accounts/%d"%cl["id"], {"revision":cl["revision"],
            "proxy_mode":"custom","proxy_url":"http://fixture.invalid:3128"})[0],200)
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
        self.assertEqual(json.loads((Path(cl["home"])/"quota-proxies").read_text()),["http://fixture.invalid:3128/"]*4)
        self.assertEqual(json.loads(self.cli("claude","hello").stdout)["proxy"],"http://fixture.invalid:3128/")

    def wait_for_quota_success(self, profile, after=0):
        import sqlite3
        with closing(sqlite3.connect(self.root/"state/cc-profile/router.sqlite3")) as db:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                row = db.execute("SELECT success_ms FROM quota_cache WHERE config_id=?",
                                 (profile["id"],)).fetchone()
                if row and row[0] and row[0] > after:
                    return row[0]
                time.sleep(.02)
        self.fail("no new quota reading for " + profile["label"])

    def test_daily_quota_catches_up_without_a_dashboard_or_launch(self):
        claude = self.account("claude", "daily-claude")
        ch = Path(claude["home"])
        (ch/".credentials.json").write_text("fixture login")
        codex = self.account("codex", "daily-codex")
        _, agy, _ = self.api("POST", "accounts", {"tool":"agy", "label":"daily-agy", "create":True})
        self.assertEqual(self.api("PATCH", f"accounts/{agy['id']}",
                                  {"revision":agy["revision"], "enabled":True})[0], 200)
        token = Path(agy["home"])/".gemini/antigravity-cli/antigravity-oauth-token"
        token.parent.parent.mkdir(mode=0o700)
        token.parent.mkdir(mode=0o700)
        token.touch(mode=0o600)
        token.write_text("fixture login")
        disabled = self.account("codex", "daily-disabled")
        self.assertEqual(self.api("PATCH", f"accounts/{disabled['id']}",
                                  {"revision":disabled["revision"], "enabled":False})[0], 200)

        self.stop()
        hold = ch/"hold-fetch"
        hold.touch()
        self.start()
        try:
            self.wait_for_quota_success(codex)
            self.wait_for_quota_success(agy)
            self.assertTrue((ch/"quota-count").exists())
            self.session = self.api("POST", "login", {"password":self.password})[1]["session"]
            self.assertEqual(self.api("POST", "quota/refresh", {"stale_only":True})[1]["queued"], 0)
        finally:
            hold.unlink()
        self.wait_for_quota_success(claude)
        self.assertEqual((ch/"quota-count").read_text(), "fetch\n")
        self.assertFalse((Path(disabled["home"])/"quota-count").exists())
        self.assertEqual(self.api("GET", "startups")[1]["items"], [])

    def test_dashboard_refresh_reschedules_only_the_refreshed_profile(self):
        import sqlite3
        panel, idle = [self.account("codex", name) for name in ("panel", "idle")]
        self.assertEqual(self.api("POST", "quota/refresh", {"stale_only":True})[1]["queued"], 2)
        for profile in (panel, idle):
            self.wait_for_quota_success(profile)
        self.stop()
        # Both daily checks come due together; only the idle profile is in backoff until then.
        due = int(time.time() * 1000) + 3000
        last_success = due - 24 * 3600 * 1000
        with closing(sqlite3.connect(self.root/"state/cc-profile/router.sqlite3")) as db, db:
            db.execute("UPDATE quota_cache SET success_ms=?,retry_ms=CASE WHEN config_id=? THEN ? ELSE 0 END",
                       (last_success, idle["id"], due))
        self.start()
        self.session = self.api("POST", "login", {"password":self.password})[1]["session"]
        self.assertEqual(self.api("POST", "quota/refresh", {"stale_only":True})[1]["queued"], 1)
        panel_success = self.wait_for_quota_success(panel, last_success)
        self.assertLess(panel_success, due)
        # The daily scheduler must skip this profile even outside the short post-fetch cooldown.
        with closing(sqlite3.connect(self.root/"state/cc-profile/router.sqlite3")) as db, db:
            db.execute("UPDATE quota_cache SET retry_ms=0 WHERE config_id=?", (panel["id"],))
        self.assertGreaterEqual(self.wait_for_quota_success(idle, last_success), due)
        for profile in (panel, idle):
            self.assertEqual((Path(profile["home"])/"quota-count").read_text(), "fetch\n" * 2)

    def test_unavailable_claude_refresh_keeps_last_good_and_does_not_block_launch(self):
        import sqlite3
        c=self.account("claude","usage")
        home=Path(c["home"])
        (home/".credentials.json").write_text('{"claudeAiOauth":{"accessToken":"private-test-token"}}')
        route="accounts/%d/quota/refresh" % c["id"]
        self.api("POST",route,{})
        def wait_status(want):
            for _ in range(150):
                q=self.api("GET","accounts?tool=claude")[1]["items"][0]["quota"]
                if not q["loading"] and q["status"]==want: return q
                time.sleep(.02)
            self.fail("quota did not reach "+want)
        wait_status("fresh")
        with closing(sqlite3.connect(self.root/"state/cc-profile/router.sqlite3")) as db, db:
            db.execute("UPDATE quota_cache SET retry_ms=0")
        (home/"raw-output").write_text("fixture-private malformed native output")
        hold=home/"hold-fetch"
        hold.touch()
        try:
            self.assertEqual(self.api("POST",route,{})[1]["queued"],1)
            self.assertEqual(self.api("POST",route,{})[1]["queued"],0)
            self.assertEqual(self.cli("claude","-p","independent").returncode,0)
            self.assertEqual(self.api("PUT","current/claude",{"config_id":c["id"]})[0],200)
        finally:
            hold.unlink()
        q=wait_status("unavailable")
        self.assertTrue(q["stale"])
        self.assertEqual(q["snapshot"]["windows"][0]["used_pct"],25)
        self.assertGreaterEqual(q["retry_ms"]-q["attempt_ms"],30000)
        self.assertNotIn("fixture-private",str(q))
        self.assertEqual(self.api("PUT","current/claude",{"config_id":None})[0],200)
        def edit(**fields):
            account=self.api("GET","accounts?tool=claude")[1]["items"][0]
            self.assertEqual(self.api("PATCH","accounts/%d" % c["id"],
                                      dict(fields,revision=account["revision"]))[0],200)
        edit(label="renamed",enabled=False)
        self.assertEqual(wait_status("unavailable"),q)
        self.assertEqual(self.api("POST",route,{})[1]["queued"],0)
        edit(proxy_mode="direct")
        hidden=wait_status("not_checked")
        self.assertFalse(hidden.get("snapshot"))
        self.assertEqual(hidden["retry_ms"],0)
        (home/"raw-output").unlink()
        self.assertEqual(self.api("POST",route,{})[1]["queued"],1)
        self.assertFalse(wait_status("fresh")["stale"])
        self.assertEqual((home/"quota-count").read_text(),"fetch\n"*3)

    def test_new_login_can_refresh_after_a_sign_in_error(self):
        c = self.account("claude","signed-out")
        route = "accounts/%d/quota/refresh" % c["id"]
        def refresh(want):
            self.assertEqual(self.api("POST",route,{})[1]["queued"],1)
            for _ in range(100):
                quota=self.api("GET","accounts?tool=claude")[1]["items"][0]["quota"]
                if not quota["loading"] and quota["status"]==want: return quota
                time.sleep(.02)
            self.fail("quota did not reach "+want)
        self.assertGreater(refresh("sign_in_required")["retry_ms"],0)
        self.assertEqual(self.api("POST",route,{})[1]["queued"],0)
        (Path(c["home"])/".credentials.json").write_text('{"claudeAiOauth":{"accessToken":"private-test-token"}}')
        self.assertEqual(refresh("fresh")["snapshot"]["windows"][0]["used_pct"],25)

    def test_claude_usage_rechecks_the_profile_login_and_native_build(self):
        c = self.account("claude","native-check")
        home, route = Path(c["home"]), f"accounts/{c['id']}/quota/refresh"
        login = home/".credentials.json"
        login.write_text("old fixture login")
        def check(predicate):
            for _ in range(200):
                q = self.api("GET","accounts?tool=claude")[1]["items"][0]["quota"]
                if predicate(q): return q
                time.sleep(.02)
            self.fail(str(q))
        hold = home/"hold-fetch"
        hold.touch()
        try:
            self.assertEqual(self.api("POST",route,{})[1]["queued"],1)
            check(lambda q: q["loading"] and (home/"quota-count").exists())
            login.write_text("new fixture login")
        finally:
            hold.unlink()
        self.assertFalse(check(lambda q:not q["loading"]).get("snapshot"))
        (home/"raw-output").write_text("x" * (1024*1024 + 1))
        self.assertEqual(self.api("POST",route,{})[1]["queued"],1)
        self.assertEqual(check(lambda q:not q["loading"])["status"],"unavailable")
        login.write_text("another login")
        with (self.root/"bin/claude").open("a") as f: f.write("\n# replaced\n")
        (home/"raw-output").unlink()
        self.assertEqual(self.api("POST",route,{})[1]["queued"],1)
        self.assertEqual(check(lambda q:not q["loading"])["status"],"fresh")
        self.assertEqual((home/"quota-count").read_text(),"fetch\n"*3)

    def test_codex_quota_follows_verified_store_not_a_leftover_auth_file(self):
        import sqlite3
        c = self.account("codex","bound")
        home = Path(c["home"])
        (home/"auth.json").write_text("{}")
        (home/"empty-quota").touch()
        config = home/"config.toml"
        def refresh():
            with closing(sqlite3.connect(self.root/"state/cc-profile/router.sqlite3")) as db, db:
                db.execute("UPDATE quota_cache SET retry_ms=0")
            route = f"accounts/{c['id']}/quota/refresh"
            self.assertEqual(self.api("POST",route,{})[1]["queued"],1)
            for _ in range(150):
                q = self.api("GET","accounts?tool=codex")[1]["items"][0]["quota"]
                if q["status"]=="fresh" and not q["loading"]: break
                time.sleep(.02)
            else:
                self.fail("no fresh quota")
            self.assertEqual(q["snapshot"]["windows"][0]["used_pct"],100)
            self.assertNotIn("file_auth_verified",q["snapshot"])
        def launch(exhausted):
            p = self.cli("codex")
            if exhausted:
                self.assertNotEqual(p.returncode,0)
                self.assertIn("exhausted",p.stderr)
            else:
                self.assertEqual(p.returncode,0,p.stderr)
        refresh()
        launch(True)
        for mode in ["keyring","auto","ephemeral"]:
            config.write_text(f'cli_auth_credentials_store = "{mode}"\n')
            launch(False)
            refresh()
            launch(False)
        config.write_text('model = "gpt-5"\n[projects."/work"]\ntrust_level = "trusted"\n')
        launch(False)  # The keyring/ephemeral observation cannot become file-bound.
        refresh()
        launch(True)
        project = self.root/".codex"
        project.mkdir()
        (project/"config.toml").write_text('cli_auth_credentials_store = "keyring"\n')
        launch(False)
        (project/"config.toml").unlink()
        (home/"incomplete-config-reply").touch()
        refresh()
        launch(False)  # Missing proof isn't the native "file" default.

    def test_automatic_job_rechecks_enabled_after_waiting_for_a_slot(self):
        def profile(name):
            c = self.account("claude",name)
            (Path(c["home"])/".credentials.json").write_text('{"claudeAiOauth":{"accessToken":"private-test-token"}}')
            return c
        held = [profile(n) for n in ("busy-one","busy-two")]
        for c in held:
            (Path(c["home"])/"hold-fetch").touch()
        def wait(check):
            for _ in range(150):
                if check(): return
                time.sleep(.02)
            self.fail("timed out waiting for quota job")
        try:
            self.assertEqual(self.api("POST","quota/refresh",{})[1]["queued"],2)
            wait(lambda: all((Path(c["home"])/"quota-count").exists() for c in held))
            c = profile("queued")
            def quota():
                return next(row["quota"] for row in self.api("GET","accounts?tool=claude")[1]["items"] if row["id"]==c["id"])
            self.assertEqual(self.api("POST","quota/refresh",{"stale_only":True})[1]["queued"],1)
            wait(lambda: quota()["loading"])
            route = f"accounts/{c['id']}"
            self.assertEqual(self.api("PATCH",route,{"revision":c["revision"],"enabled":False})[0],200)
            busy = held[0]
            self.assertEqual(self.api("PATCH",f"accounts/{busy['id']}",{"revision":busy["revision"],"enabled":False})[0],200)
        finally:
            for busy in held: (Path(busy["home"])/"hold-fetch").unlink()
        wait(lambda: not quota()["loading"])
        count = Path(c["home"])/"quota-count"
        self.assertFalse(count.exists())
        self.assertEqual(quota()["status"],"not_checked")
        def busy_finished():
            q = next(row["quota"] for row in self.api("GET","accounts?tool=claude")[1]["items"] if row["id"]==held[0]["id"])
            return not q["loading"] and q["status"]=="fresh"
        wait(busy_finished)
        self.assertEqual(self.api("POST",route+"/quota/refresh",{})[1]["queued"],1)
        wait(lambda: quota()["status"]=="fresh" and not quota()["loading"])
        self.assertEqual(count.read_text(),"fetch\n")


@unittest.skipUnless(DAEMON.is_file(), "build ccpd first")
class DaemonUpgrade(unittest.TestCase):
    def test_unlinking_running_daemon_keeps_accounts_accessible(self):
        with tempfile.TemporaryDirectory() as d:
            service = Service("test_web_authenticates_reads_and_mutations")
            service.binary = Path(shutil.copy2(DAEMON, Path(d)/"ccpd"))
            self.addCleanup(service.doCleanups)
            service.setUp()
            service.account("codex", "work")
            service.binary.unlink()
            for tool in ("claude","codex"):
                status, data, _ = service.api("GET","accounts?tool="+tool)
                self.assertEqual(status, 200)
                self.assertNotIn("observer",data)
            self.assertEqual(service.api("GET","accounts?tool=codex")[1]["items"][0]["readiness"],"ready")
