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
if sys.argv[1:2] == ["app-server"]:
    if (home/"track-child").exists(): (home/"child-pid").write_text(str(os.getpid()))
    for line in sys.stdin:
        r = json.loads(line)
        if "id" not in r: continue
        result = {}
        if r["method"] == "account/read":
            assert r["params"] == {"refreshToken":False}
            path = home / "fixture-identity.json"
            result = json.loads(path.read_text()) if path.exists() else {"account":None}
        if r["method"] in ("thread/start","thread/resume","unrelated"):
            uid = r.get("params",{}).get("threadId",str(uuid.uuid4()))
            result = {"thread":{"id":uid,"sessionId":uid,"ephemeral":r.get("params",{}).get("ephemeral",False),
                                "cwd":os.getcwd()}, "home":str(home), "proxy":os.getenv("HTTPS_PROXY"),
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
        if r["method"] == "account/rateLimits/read":
            with (home / "quota-count").open("a") as f: f.write("fetch\n")
            result = {"rateLimits": {"planType":"pro", "primary":{"windowDurationMins":300,"usedPercent":100 if (home/"empty-quota").exists() else 0,"resetsAt":2000000000}}}
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
        curl = paths / "curl"
        curl.write_text("#!" + sys.executable + r'''
import json, os, pathlib, sys, time
home = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"])
config = sys.stdin.read()
assert 'proxy =' not in config  # curl uses the launch environment, not a second proxy policy.
with (home / "quota-proxies").open("w") as f:
    json.dump([os.getenv(k) for k in ("HTTP_PROXY","HTTPS_PROXY","http_proxy","https_proxy")],f)
assert 'Bearer private-test-token' in config and not any('private-test-token' in a for a in sys.argv)
assert 'https://api.anthropic.com/api/oauth/usage' in config
with (home / "quota-count").open("a") as stream: stream.write("fetch\n")
while (home / "hold-fetch").exists(): time.sleep(.02)
if (home / "throttled").exists():
    print('{"error":"fixture"}\n429\n600',end="")
else:
    fixture = home / "usage.json"
    usage = fixture.read_text() if fixture.exists() else '{"five_hour":{"utilization":25,"resets_at":"2030-01-01T00:00:00Z"}}'
    print(usage+'\n200\n', end="")
''')
        curl.chmod(0o700)
        self.env = {"PATH": str(paths) + ":/usr/bin:/bin", "HOME": str(self.root),
                    "XDG_RUNTIME_DIR": str(self.root / "run"), "XDG_STATE_HOME": str(self.root / "state")}
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

    def api(self, method, route, body=None, extra=None, *, connect_host="127.0.0.1"):
        conn = http.client.HTTPConnection(connect_host, self.port, timeout=8)
        headers = {"Origin": "http://127.0.0.1:%s" % self.port, "Content-Type": "application/json",
                   "X-CCP-Session": self.session}
        headers.update(extra or {})
        try:
            conn.request(method, "/api/v1/" + route, json.dumps(body) if body is not None else None, headers)
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

    def test_creating_and_signing_in_a_second_claude_profile(self):
        work = self.account("claude", "work")
        self.assertEqual(self.api("PUT", "current/claude", {"config_id": work["id"]})[0], 200)
        create = {"tool":"claude","label":"Personal","create": True,
                  "proxy_mode":"custom","proxy_url":"http://fixture.invalid:3128"}
        status, personal, _ = self.api("POST", "accounts", create)
        self.assertEqual(status, 201, personal)
        home = Path(personal["home"])
        self.assertTrue(home.is_relative_to(self.root/".local/share/cc-profile/profiles/claude"))
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
            wire = "".join(json.dumps({"id":i,"method":method,"params":params})+"\n"
                           for i,(method,params) in enumerate(calls))
            p=subprocess.run([LAUNCHER,"codex","app-server",*args],input=wire,
                             env=self.env,cwd=self.root,capture_output=True,text=True,timeout=8)
            self.assertEqual(p.returncode,0,p.stderr)
            return [json.loads(line)["result"] for line in p.stdout.splitlines()]
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

    def test_hook_free_child_cleanup_and_startup_failure(self):
        home=Path(self.account("codex","fixture")["home"])
        (home/"track-child").touch()
        with subprocess.Popen([LAUNCHER,"codex","app-server"],cwd=self.root,env=self.env,
                              stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE) as p:
            try:
                for _ in range(100):
                    if (home/"child-pid").exists():break
                    time.sleep(.03)
                pid=int((home/"child-pid").read_text())
            finally:
                p.terminate()
            self.assertEqual(p.wait(timeout=5),143)
            with self.assertRaises(ProcessLookupError):os.kill(pid,0)
        # This stdio-only fixture exits instead of opening the requested socket.
        tui=self.cli("codex")
        self.assertNotEqual(tui.returncode,0)
        self.assertIn("app-server exited",tui.stderr)
        self.assertFalse(list((self.root/"run/cc-profile").glob("c-*")))
        self.assertEqual(self.api("GET","sessions")[1]["items"],[])
        self.assertEqual(self.api("GET","startups")[1]["items"][0]["state"],"exec_failed")

    def test_managed_profile_under_relocated_data_home(self):
        data = self.root / "relocated-share"
        data.mkdir()
        share = self.root / ".local/share"
        share.parent.mkdir(exist_ok=True)
        share.symlink_to(data, target_is_directory=True)
        status, c, _ = self.api("POST", "accounts", {"tool":"claude","label":"Personal","create":True})
        self.assertEqual(status, 201, c)
        home = Path(c["home"])
        self.assertEqual(home.parent, data / "cc-profile/profiles/claude")
        self.assertTrue(c["present"])
        alias = share / "cc-profile/profiles/claude" / home.name
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
        with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
            client.settimeout(2)
            client.connect(str(self.root/"run/cc-profile/launch.sock"))
            client.sendall(b"POST /v1/launch HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
            self.assertIn(b"404",client.recv(100))
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
        self.assertEqual(self.api("GET","sessions")[1]["items"][0]["config_id"], work["id"])

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
        self.assertEqual(self.api("GET","settings")[:2],(200,{"routing_mode":"round_robin"}))
        for mode in ["most_remaining","least_remaining","round_robin","priority"]:
            body = {"routing_mode":mode}
            self.assertEqual(self.api("PUT","settings",body)[:2],(200,body))
        for body in [{},{"routing_mode":None},{"routing_mode":"invalid"}]:
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
        p = self.cli("codex","app-server")
        self.assertEqual(p.returncode,0,p.stderr)
        self.assertIn(high["home"],p.stderr)
        self.assertEqual(self.api("GET","startups")[1]["items"][0]["reason"],"priority")

    def test_remaining_modes_warm_only_this_provider_without_waiting_for_quota(self):
        profiles = [Path(self.account("claude",name)["home"]) for name in ("nearly-empty","roomy")]
        codex = Path(self.account("codex","other-provider")["home"])
        for home, used in zip(profiles, [99.9,20]):
            (home/".credentials.json").write_text('{"claudeAiOauth":{"accessToken":"private-test-token"}}')
            (home/"usage.json").write_text(json.dumps({"five_hour":{"utilization":used}}))
            (home/"hold-fetch").touch()
        self.assertEqual(self.api("PUT","settings",{"routing_mode":"most_remaining"})[0],200)
        try:
            # First admission (unknown quota) must complete while both fetches are held.
            first = self.cli("claude","chat")
            self.assertEqual(first.returncode,0,first.stderr)
            self.assertEqual(json.loads(first.stdout)["home"],str(profiles[0]))
            for _ in range(200):
                if all((home/"quota-count").exists() for home in profiles): break
                time.sleep(.02)
            self.assertTrue(all((home/"quota-count").exists() for home in profiles))
            self.assertEqual(self.cli("claude","second").returncode,0)
        finally:
            for home in profiles: (home/"hold-fetch").unlink()
        for _ in range(200):
            if all(c["quota"]["status"]=="fresh" for c in self.api("GET","accounts?tool=claude")[1]["items"]): break
            time.sleep(.02)
        else:
            self.fail("background quota refresh did not finish")
        for mode, index in [("most_remaining",1),("least_remaining",0)]:
            self.assertEqual(self.api("PUT","settings",{"routing_mode":mode})[0],200)
            for _ in range(2):
                p = self.cli("claude","chat")
                self.assertEqual(p.returncode,0,p.stderr)
                self.assertEqual(json.loads(p.stdout)["home"],str(profiles[index]))
            self.assertEqual(self.api("GET","startups")[1]["items"][0]["reason"],mode)
        for home in profiles:
            self.assertEqual((home/"quota-count").read_text(),"fetch\n")
        self.assertFalse((codex/"quota-count").exists())

        import sqlite3
        for home in profiles:
            (home/"usage.json").write_text('{"seven_day":{"utilization":100}}')
            (home/"hold-fetch").touch()
        try:
            with closing(sqlite3.connect(self.root/"state/cc-profile/router.sqlite3")) as db, db:
                db.execute("UPDATE quota_cache SET success_ms=0,attempt_ms=0,retry_ms=0")
            self.assertEqual(self.cli("claude","stale").returncode,0)
        finally:
            for home in profiles: (home/"hold-fetch").unlink()
        for _ in range(200):
            quotas = [c["quota"] for c in self.api("GET","accounts?tool=claude")[1]["items"]]
            if all(not q["stale"] and q["snapshot"]["windows"][0]["used_pct"]==100 for q in quotas): break
            time.sleep(.02)
        else:
            self.fail("new admission did not replace stale quota")
        self.assertEqual(self.api("PUT","settings",{"routing_mode":"round_robin"})[0],200)
        refused = self.cli("claude","new")
        self.assertNotEqual(refused.returncode,0)
        self.assertIn("exhausted",refused.stderr)
        self.assertEqual(self.api("GET","startups")[1]["items"][0]["reason"],"quota_exhausted")
        uid = json.loads(first.stdout)["argv"][1]
        self.assertEqual(json.loads(self.cli("claude","--resume",uid).stdout)["home"],str(profiles[0]))

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

    def test_throttled_background_refresh_keeps_last_good_and_does_not_block_launch(self):
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
        (home/"throttled").touch()
        hold=home/"hold-fetch"
        hold.touch()
        try:
            self.assertEqual(self.api("POST",route,{})[1]["queued"],1)
            self.assertEqual(self.api("POST",route,{})[1]["queued"],0)
            self.assertEqual(self.cli("claude","-p","independent").returncode,0)
            self.assertEqual(self.api("PUT","current/claude",{"config_id":c["id"]})[0],200)
        finally:
            hold.unlink()
        q=wait_status("rate_limited")
        self.assertTrue(q["stale"])
        self.assertEqual(q["snapshot"]["windows"][0]["used_pct"],25)
        self.assertGreaterEqual(q["retry_ms"]-q["attempt_ms"],600000)
        self.assertEqual(self.api("PUT","current/claude",{"config_id":None})[0],200)
        def edit(**fields):
            account=self.api("GET","accounts?tool=claude")[1]["items"][0]
            self.assertEqual(self.api("PATCH","accounts/%d" % c["id"],
                                      dict(fields,revision=account["revision"]))[0],200)
        edit(label="renamed",enabled=False)
        self.assertEqual(wait_status("rate_limited"),q)
        self.assertEqual(self.api("POST",route,{})[1]["queued"],0)
        edit(proxy_mode="direct")
        hidden=wait_status("rate_limited")
        self.assertFalse(hidden.get("snapshot"))
        self.assertEqual(hidden["retry_ms"],q["retry_ms"])
        self.assertEqual(self.api("POST",route,{})[1]["queued"],0)
        edit(proxy_mode="inherit")
        (home/".credentials.json").write_text('{"claudeAiOauth":{"accessToken":"different"}}')
        self.assertFalse(wait_status("rate_limited").get("snapshot"))
        self.assertEqual(self.api("POST",route,{})[1]["queued"],0)
        self.assertEqual((home/"quota-count").read_text(),"fetch\n"*2)

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
            p = self.cli("codex","app-server")
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
            self.assertEqual(self.api("PUT","settings",{"routing_mode":"most_remaining"})[0],200)
            self.assertEqual(self.cli("claude","test").returncode,0)
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
