"""Opt-in smoke with installed providers, disposable homes, and no real credentials."""
import asyncio
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from queue import Queue
import re
import subprocess
import shutil
import sys
import threading
import time
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_service

class Native(unittest.TestCase):
    def setUp(self):
        self.s = test_service.Service("test_web_authenticates_reads_and_mutations")
        self.s.native = True
        self.addCleanup(self.s.doCleanups)
        self.s.setUp()

    def test_empty_profiles_report_signed_out(self):
        for tool in ("claude", "codex"):
            profile = self.s.account(tool, tool + "-empty")
            self.assertEqual(self.s.api("GET", f"accounts/{profile['id']}/identity")[:2],
                             (200, {"state":"signed_out", "email":None}), tool)
        self.assertEqual(self.s.api("GET", "startups")[1]["items"], [])

    def test_claude_delimiter_depends_on_print_mode(self):
        c = self.s.account("claude","delimiter")
        for verb in ("rc","remote-control"):
            args=["--permission-mode","default","--",verb]
            # This local validation error identifies dispatch without starting Remote Control.
            native=subprocess.run(["claude","--session-id",str(uuid.uuid4()),*args],
                                  env=dict(self.s.env,CLAUDE_CONFIG_DIR=c["home"]), cwd=self.s.root,
                                  stdin=subprocess.DEVNULL,capture_output=True,text=True,timeout=5)
            self.assertIn("before `remote-control` is not carried over",native.stderr)
            self.assertNotEqual(self.s.cli("claude",*args).returncode,0)
        self.assertFalse(self.s.api("GET","sessions")[1]["items"])
        result=self.s.cli("claude","-p","--output-format","json","--","rc")
        self.assertEqual(self.s.api("GET","sessions")[1]["items"],[])
        self.assertNotIn("Unsupported",result.stderr)
        self.assertEqual(json.loads(result.stdout)["type"], "result")

    def test_claude_assigns_exact_id_before_exec(self):
        c = self.s.account("claude","cc-home")
        self.s.cli("claude","Reply OK")
        row = self.s.api("GET","sessions?tool=claude")[1]["items"][0]
        uid = row["native_id"]
        self.assertEqual(row["evidence"],"assigned")
        self.assertTrue(list(Path(c["home"]).glob("projects/*/"+uid+".jsonl")))
        resumed = self.s.cli("claude","-p","--output-format","json","--resume",uid,"Continue")
        self.assertIn(uid,resumed.stdout,resumed.stderr)
        self.assertNotEqual(self.s.cli("claude","--resume",str(uuid.uuid4())).returncode,0)

    def rejecting_endpoint(self, models=None, posts=None):
        # A closed port triggers Codex's offline reconnect loop. A terminal HTTP
        # rejection lets the real CLI finish without sending any model traffic upstream.
        class Reject(BaseHTTPRequestHandler):
            def do_POST(self):
                if self.command == "POST":
                    payload = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                    # Distinguish this turn from delayed requests by a previously used daemon.
                    if posts is not None:
                        posts.extend((self.path, tag) for tag in re.findall(rb"ccp-fixture-[0-9a-f]+", payload))
                    if models is not None:
                        request = json.loads(payload)
                        if "model" in request:
                            models.append(request["model"])
                body=b'{"error":{"message":"fixture rejection","type":"invalid_request_error"}}'
                self.send_response(400)
                self.send_header("Content-Type","application/json")
                self.send_header("Content-Length",str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            do_GET=do_POST
            def log_message(self,*args):
                pass
        endpoint=ThreadingHTTPServer(("127.0.0.1",0),Reject)
        threading.Thread(target=endpoint.serve_forever,daemon=True).start()
        self.addCleanup(endpoint.server_close)
        self.addCleanup(endpoint.shutdown)
        return f"http://127.0.0.1:{endpoint.server_port}"

    def offline_codex(self, home, models=None, url=None):
        cfg = home / "config.toml"
        if url is None:
            url = self.rejecting_endpoint(models) + "/v1"
        cfg.write_text('model_provider="offline"\nmodel="offline"\n[model_providers.offline]\n'
                       f'name="Local fixture"\nbase_url="{url}"\nwire_api="responses"\n'
                       'request_max_retries=0\nstream_max_retries=0\n')
        return cfg

    def wait_for_owner(self, uid):
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            rows = self.s.api("GET", "sessions?native_id="+uid)[1]["items"]
            if rows:
                return rows[0]
            time.sleep(.1)
        self.fail("Saved thread was not discovered: " + uid)

    @unittest.skipUnless(os.getenv("CCP_TEST_CODEX_OFFLINE") == "1",
                         "requires the isolated native Codex fixture")
    def test_codex_deferred_and_explicit_native_handshakes(self):
        a = self.s.account("codex", "owner")
        self.s.account("codex", "other")
        init = {"clientInfo":{"name":"garcon","title":"Garcon","version":"0.1.0"},
                "capabilities":{"experimentalApi":True,"requestAttestation":False}}

        async def exchange(extra, method, params):
            proc = await asyncio.create_subprocess_exec(
                test_service.LAUNCHER, "codex", "app-server", "--listen", "stdio://", *extra,
                cwd=self.s.root, env=self.s.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL)
            frames = []
            def send(frame):
                proc.stdin.write(json.dumps(frame).encode() + b"\n")
            async def call(i, op, args):
                send({"id":i,"method":op,"params":args})
                while line := await proc.stdout.readline():
                    msg = json.loads(line)
                    frames.append(msg)
                    if msg.get("id") == i:
                        self.assertNotIn("error", msg)
                        return msg["result"]
                self.fail(f"Premature native EOF waiting for {op}")
            try:
                async with asyncio.timeout(25):
                    hello = await call(1, "initialize", init)
                    send({"method":"initialized"})
                    result = await call(2, method, params)
                    proc.stdin.close()
                    frames.extend(map(json.loads, (await proc.stdout.read()).splitlines()))
                    self.assertEqual(await proc.wait(), 0)
            finally:
                if proc.returncode is None:
                    proc.kill()
                    await proc.wait()
            self.assertEqual([f["id"] for f in frames if "result" in f or "error" in f], [1, 2])
            return hello, result

        public, new = asyncio.run(exchange([], "thread/start", {"ephemeral":False,"cwd":str(self.s.root)}))
        uid = new["thread"]["id"]
        self.assertEqual(self.wait_for_owner(uid)["config_id"], a["id"])
        rollout = Path(new["thread"]["path"])
        self.assertTrue(rollout.is_relative_to(Path(a["home"])/"sessions"))
        if not rollout.exists():
            # A no-turn start may have only reserved the filename.
            stamp = "2026-01-01T00:00:00Z"
            meta = dict(id=uid, timestamp=stamp, cwd=str(self.s.root), originator="codex_cli_rs",
                        cli_version="0.172.0", source="cli", model_provider="openai")
            rollout.parent.mkdir(exist_ok=True, parents=True)
            rollout.write_text(json.dumps(dict(timestamp=stamp, type="session_meta", payload=meta)) + "\n")
        private, resumed = asyncio.run(exchange(["--for-session", uid], "thread/resume", {"threadId":uid}))
        self.assertEqual((resumed["thread"]["id"], resumed["thread"]["path"]), (uid, str(rollout)))
        self.assertEqual(private["codexHome"], a["home"])
        public_home = Path(public["codexHome"])
        self.assertTrue(public_home.is_dir() and public_home.is_relative_to(Path(self.s.env["XDG_RUNTIME_DIR"])))
        self.assertEqual(public | {"codexHome":a["home"]}, private)
        self.assertRegex(public["userAgent"], r"^garcon/\d+\.\d+\.\d+\b")

    def test_default_codex_exec_and_stdio_agree_on_sticky_owner(self):
        c = self.s.account("codex","no-hooks")
        home = Path(c["home"])
        models = []
        self.offline_codex(home, models)
        refused = self.s.cli("codex","exec","--json","--","--skip-git-repo-check")
        self.assertNotEqual(refused.returncode,0)
        self.assertIn("--skip-git-repo-check", refused.stderr)
        self.assertFalse(self.s.api("GET","sessions")[1]["items"])
        work = self.s.root / "project"
        subprocess.run(["git","init","-q",str(work)],check=True)
        p = self.s.cli("codex","-m","fixture-model","-C",str(work),"exec","--sandbox","read-only","--json","Local fixture only")
        events = [json.loads(line) for line in p.stdout.splitlines()]
        uid = next((e["thread_id"] for e in events if e.get("type")=="thread.started"),None)
        self.assertIsNotNone(uid,p.stderr)
        self.assertNotEqual(p.returncode,0,"native fixture rejection must reach the caller")
        self.assertIn("fixture-model", models)
        self.s.account("codex","other")
        prior_requests = len(models)
        resumed=self.s.cli("codex","exec","--skip-git-repo-check","--json","resume",uid,"Local fixture only")
        events = [json.loads(line) for line in resumed.stdout.splitlines()]
        self.assertIn(uid, [e["thread_id"] for e in events if e["type"] == "thread.started"], resumed.stderr)
        self.assertGreater(len(models), prior_requests)
        owner = self.wait_for_owner(uid)
        self.assertEqual((owner["config_id"], owner["evidence"]), (c["id"],"saved_file"))
        features = {"goals":False, "wake_on_agent_messages":False, "default_mode_request_user_input":True}
        flags = [arg for k, v in features.items() for arg in ("--config", f"features.{k}={json.dumps(v)}")]
        with subprocess.Popen([test_service.LAUNCHER,"codex","app-server","--listen","stdio://",
                               *flags,"--for-session",uid],
                              env=self.s.env,cwd=self.s.root,stdin=subprocess.PIPE,stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE,text=True) as child:
            try:
                replies=Queue()
                def read_replies():
                    for line in child.stdout:
                        replies.put(json.loads(line))
                threading.Thread(target=read_replies,daemon=True).start()
                def call(num,method,params):
                    child.stdin.write(json.dumps({"id":num,"method":method,"params":params})+"\n")
                    child.stdin.flush()
                    while True:
                        msg=replies.get(timeout=8)
                        if msg.get("id")==num:
                            self.assertNotIn("error",msg)
                            return msg["result"]
                init = call(1,"initialize",{"clientInfo":{"name":"ccp-test","version":"1"}})
                self.assertEqual(init["codexHome"], str(home))
                child.stdin.write('{"method":"initialized"}\n')
                effective = call(4, "config/read", {"includeLayers":False})["config"]["features"]
                self.assertEqual({k:effective[k] for k in features}, features)
                self.assertEqual(call(2,"thread/resume",{"threadId":uid})["thread"]["id"],uid)
                new=call(3,"thread/start",{})["thread"]["id"]
                rows=self.s.api("GET","sessions?native_id="+new)[1]["items"]
                self.assertEqual([r["config_id"] for r in rows],[c["id"]])
            finally:
                child.terminate()
                child.wait(timeout=8)
        self.assertFalse((home/"hooks.json").exists())
        self.assertFalse(list((self.s.root/"run/cc-profile").glob("c-*")))

    def test_codex_ephemeral_exec_does_not_assign_or_persist_a_thread(self):
        c = self.s.account("codex", "ephemeral")
        home = Path(c["home"])
        models = []
        self.offline_codex(home, models)
        p = self.s.cli("codex", "-m", "fixture-model", "exec", "--skip-git-repo-check",
                       "--ephemeral", "Local fixture only", "--json")
        self.assertIn("fixture-model", models, p.stderr)
        self.assertNotEqual(p.returncode, 0)  # The loopback endpoint rejected the request.
        row, = self.s.api("GET", "startups?tool=codex")[1]["items"]
        self.assertEqual((row["intent"], row["native_id"]), ("print", None))
        self.assertEqual(self.s.api("GET", "sessions?tool=codex")[1]["items"], [])
        self.assertFalse(list(home.glob("sessions/**/rollout-*.jsonl")))

    def test_codex_exec_outside_git_with_explicit_opt_out(self):
        c=self.s.account("codex","exec")
        self.offline_codex(Path(c["home"]))
        p=self.s.cli("codex","exec","--skip-git-repo-check","--json","Local fixture only")
        self.assertIn("fixture rejection",p.stdout)
        self.assertNotEqual(p.returncode,0)
        uid = next(e["thread_id"] for e in map(json.loads,p.stdout.splitlines()) if e["type"]=="thread.started")
        self.assertEqual(self.wait_for_owner(uid)["config_id"], c["id"])

    def tui(self, work, *args):
        name="ccp-pty-"+uuid.uuid4().hex[:12]
        def h(*cmd, check=True):
            result = subprocess.run(["hty", cmd[0], name, *cmd[1:]], capture_output=True, text=True, timeout=18)
            if check and result.returncode:
                screen = h("snapshot", check=False) if cmd[0] != "snapshot" else ""
                self.fail(result.stderr + screen)
            return result.stdout
        subprocess.run(["hty","run","--name",name,"--cwd",str(work),"--cols","120","--rows","46","--",
                        "/usr/bin/env","-i","TERM=xterm-256color",*[f"{k}={v}" for k,v in self.s.env.items()],
                        str(test_service.LAUNCHER),"codex",*args], check=True, capture_output=True, timeout=18)
        self.addCleanup(h, "delete", check=False)
        self.addCleanup(h, "kill", check=False)
        return h

    def screen_contains(self, h, text, *, present=True):
        until=time.monotonic()+15
        while time.monotonic()<until:
            screen = h("snapshot")
            if (text in screen) == present: return
            time.sleep(.15)
        self.fail(f"Expected {text!r} present={present}: {screen}")

    def close_tui(self, h):
        h("send","--key","ctrl-c")
        time.sleep(.2)
        h("send","--key","ctrl-c",check=False)
        h("wait","--exit","--timeout","6000")
        self.assertFalse(list((self.s.root/"run/cc-profile").glob("c-*")))

    @unittest.skipUnless(os.getenv("CCP_TEST_CODEX_OFFLINE") == "1" and shutil.which("hty"),
                         "run tests/native/codex-offline.sh for isolated native daemon checks")
    def test_codex_native_proxy_reuse_and_explicit_no_daemon(self):
        work = self.s.root / "project"
        subprocess.run(["git", "init", "-q", str(work)], check=True)
        hits = [[], [], []]
        proxies = [self.rejecting_endpoint(posts=log) for log in hits]

        def update(account, **fields):
            code, updated, _ = self.s.api("PATCH", f"accounts/{account['id']}",
                                           {"revision": account["revision"], **fields})
            self.assertEqual(code, 200, updated)
            account.update(updated)

        def profile(name, proxy):
            account = self.s.account("codex", name)
            update(account, proxy_mode="custom", proxy_url=proxy)
            cfg = self.offline_codex(Path(account["home"]), url=f"http://192.0.2.1/{name}")
            with cfg.open("a") as f:
                f.write(f'\n[projects."{work}"]\ntrust_level="trusted"\n')
            return account

        def received(index, name, tag):
            request = (f"http://192.0.2.1/{name}/responses", tag.encode())
            deadline = time.monotonic() + 10
            while not any(request in log for log in hits) and time.monotonic() < deadline:
                time.sleep(.1)
            self.assertEqual([request in log for log in hits],
                             [i == index for i in range(len(hits))], hits)
            for log in hits:
                log.clear()

        def chat(index, name, *args):
            tag = "ccp-fixture-" + uuid.uuid4().hex
            h = self.tui(work, *args, tag)
            received(index, name, tag)
            self.screen_contains(h, "fixture rejection")
            self.close_tui(h)

        a = profile("a", proxies[0])
        chat(0, "a")  # No existing native daemon.
        update(a, proxy_url=proxies[1], proxy_mode="custom")
        chat(0, "a")  # Reused daemon retains its startup environment.
        update(a, enabled=False)
        b = profile("b", proxies[2])
        chat(2, "b")
        update(b, enabled=False)
        update(a, enabled=True)
        chat(1, "a", "--no-daemon")
        for proxy_index, args in [(0, []), (1, ["--no-daemon"])]:
            tag = "ccp-fixture-" + uuid.uuid4().hex
            result = self.s.cli("codex", "-C", str(work), "exec", "--json", tag)
            received(1, "a", tag)
            uid = next(e["thread_id"] for e in map(json.loads, result.stdout.splitlines())
                       if e["type"] == "thread.started")
            chat(proxy_index, "a", *args, "resume", uid)

    @unittest.skipUnless(os.getenv("CCP_TEST_CODEX_OFFLINE") == "1" and shutil.which("hty"),
                         "run tests/native/codex-offline.sh for isolated native terminal checks")
    def test_default_codex_tui_asks_for_trust_before_binding(self):
        c=self.s.account("codex","pty")
        models=[]
        self.offline_codex(Path(c["home"]), models)
        work=self.s.root/"project"
        subprocess.run(["git","init","-q",str(work)],check=True)
        (work/".codex").mkdir()
        (work/".codex/config.toml").write_text('model="gpt-6-sol"\n')
        h=self.tui(work)
        self.screen_contains(h, "Trust this folder?")
        self.assertFalse(self.s.api("GET","sessions")[1]["items"])
        h("send", "--key", "down")
        self.screen_contains(h, "› 2.")
        h("send", "--key", "enter")
        self.screen_contains(h, "Trust this folder?", present=False)
        h("kill", check=False)
        self.assertFalse(models)
        self.assertFalse(list(Path(c["home"]).glob("sessions/**/rollout-*")))
        h=self.tui(work)
        self.screen_contains(h, "Trust this folder?")
        h("send","--key","enter")
        self.screen_contains(h, "GPT-6-Sol")
        self.assertFalse(list(Path(c["home"]).glob("sessions/**/rollout-*")))
        h("send","--text","Local fixture only")
        time.sleep(.4)
        h("send","--key","enter")
        self.screen_contains(h, "fixture rejection")
        self.assertIn("gpt-6-sol", models)
        for _ in range(70):
            rows=self.s.api("GET","sessions")[1]["items"]
            if rows:break
            time.sleep(.15)
        self.assertEqual([r["config_id"] for r in rows],[c["id"]],h("snapshot"))
        uid=rows[0]["native_id"]
        self.close_tui(h)
        self.s.account("codex","other")
        h=self.tui(work,"resume",uid)
        self.screen_contains(h, "Local fixture only")
        log=self.s.api("GET","startups?tool=codex")[1]["items"][0]
        self.assertEqual((log["reason"],log["config_id"]),("sticky",c["id"]))
        self.assertEqual(len(self.s.api("GET","sessions")[1]["items"]),1)
        self.close_tui(h)

if __name__=="__main__":
    unittest.main()
