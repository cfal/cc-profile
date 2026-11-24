import json
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import pty
import select
import signal
import sqlite3
import subprocess
import time
import unittest
import uuid
from contextlib import closing, contextmanager

import test_service as svc

INIT = {"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{},
        "clientInfo":{"name":"fixture","version":"1"},"_meta":{"test":"client-specific"}}}
INITIALIZED = {"jsonrpc":"2.0","method":"notifications/initialized"}
PRELUDE = json.dumps(INIT)+"\n"+json.dumps(INITIALIZED)+"\n"


class Antigravity(unittest.TestCase):
    def setUp(self):
        self.s = svc.Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(self.s.doCleanups)
        self.s.setUp()
        self.s.env.update(DBUS_SESSION_BUS_ADDRESS="unix:path=/dummy", XDG_CONFIG_HOME="/not-the-profile",
                          AGY_ACP_FORCE_FILE_STORAGE="0")

    def profile(self, name, enable=True):
        status, c, _ = self.s.api("POST","accounts",{"tool":"agy","label":name,"create":True,
                                "proxy_mode":"custom","proxy_url":"https://proxy.invalid:8443"})
        self.assertEqual(status,201,c)
        if enable:
            _, c, _ = self.s.api("PATCH",f"accounts/{c['id']}",{"revision":c["revision"],"enabled":True})
        return c

    def bridge(self, args, *calls):
        wire = "".join(json.dumps({"jsonrpc":"2.0","id":n,"method":m,"params":p})+"\n" for n,m,p in calls)
        result = subprocess.run([svc.LAUNCHER,"agy","acp",*args],input=PRELUDE+wire,text=True,capture_output=True,
                                cwd=self.s.root,env=self.s.env,timeout=8)
        self.assertEqual(result.returncode,0,result.stderr)
        init, *replies = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual((init["id"],init["result"]["authMethods"]), ("init",[]))
        return replies

    @staticmethod
    def login_markers(*profiles):
        for c in profiles:
            gemini = Path(c["home"])/".gemini"
            app = gemini/"antigravity-cli"
            for d in (gemini,app): d.mkdir(mode=0o700,exist_ok=True)
            token=app/"antigravity-oauth-token"
            token.touch(mode=0o600)
            token.write_text("inert fixture marker")

    @contextmanager
    def connection(self, *args):
        with subprocess.Popen([svc.LAUNCHER,"agy","acp",*args],cwd=self.s.root,env=self.s.env,
                              stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True) as p:
            def call(i,m,params):
                p.stdin.write(json.dumps(dict(jsonrpc="2.0",id=i,method=m,params=params))+"\n")
                p.stdin.flush()
                self.assertTrue(select.select([p.stdout],[],[],6)[0],m)
                r=json.loads(p.stdout.readline())
                self.assertEqual(r["id"],i)
                return r
            try:
                self.assertEqual(call("init","initialize",INIT["params"])["result"]["authMethods"],[])
                p.stdin.write(json.dumps(INITIALIZED)+"\n")
                p.stdin.flush()
                yield call
                p.stdin.close()
                self.assertEqual(p.wait(timeout=5),0,p.stderr.read())
            finally:
                if p.poll() is None: p.kill()

    def private_status(self,path,body):
        import http.client
        import socket
        with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as sock:
            sock.settimeout(3)
            sock.connect(str(self.s.root/"run/cc-profile/launch.sock"))
            with closing(http.client.HTTPConnection("localhost")) as conn:
                conn.sock=sock
                conn.request("POST",path,json.dumps(body),{"Content-Type":"application/json"})
                return conn.getresponse().status

    def test_disabled_named_login_status_and_native_usage(self):
        c = self.profile("work", enable=False)
        cid, home = str(c["id"]), Path(c["home"])
        identity = lambda: self.s.api("GET",f"accounts/{cid}/identity")[:2]
        self.assertEqual(identity(), (200, {"state":"no_saved_login","email":None}))
        self.assertNotEqual(self.s.cli("login",cid,"--status").returncode,0)
        self.assertNotEqual(self.s.cli("usage",cid).returncode,0)
        self.assertEqual(self.s.cli("login",cid).returncode,0)
        self.assertEqual(identity(), (200, {"state":"saved_login","email":None}))
        self.assertIn("validity not checked",self.s.cli("login",cid,"--status").stdout)
        self.assertEqual((home/".gemini/antigravity-cli/antigravity-oauth-token").stat().st_mode & 0o777,0o600)
        self.assertEqual(self.s.cli("usage",cid).stdout.strip(),
                         "Gemini Models\tWeekly Limit Remaining\t76%\t2030-01-01T00:00:00Z")
        for extra in (["--settings", "{}"], ['--settings={"disableSlashCommands":true}']):
            p = self.s.cli("usage", cid, *extra)
            self.assertEqual((p.returncode, p.stdout), (2, ""))
            self.assertNotIn("disableSlashCommands", p.stderr)
        for argv in (["profile-usage"], ["-p", "/usage"], ["--settings", "{}", "-p", "/usage"]):
            self.assertNotEqual(self.s.cli("agy",*argv).returncode,0)
        self.assertEqual((home/"usage-calls").read_text().splitlines(),
                         [json.dumps(["--settings",'{"disableSlashCommands":false}',"-p","/usage"])])
        obs=json.loads((home/"last-run").read_text())
        self.assertEqual(obs["proxy"],["https://proxy.invalid:8443/"]*4)
        self.assertFalse(Path(obs["runtime"]).exists())
        self.assertFalse(self.s.api("GET","sessions?tool=agy")[1]["items"])
        self.assertEqual(self.s.cli("agy","acp").returncode,0)  # EOF before selecting a profile.

    def test_native_tui_selects_a_profile_without_claiming_a_session(self):
        a = self.profile("first")
        b = self.profile("second")
        for owner, args in [(a, []), (b, ["--add-dir", "one", "--model=acp", "--sandbox=true",
                                          "--add-dir=two", "-i", "private initial prompt"])]:
            result = self.s.cli("agy", *args)
            self.assertEqual(result.returncode, 0, result.stderr)
            if args:
                self.assertEqual(json.loads(result.stdout), {"args":args, "home":owner["home"]})
            else:
                self.assertEqual(result.stdout.strip(), "fixture Antigravity TUI")
            obs = json.loads((Path(owner["home"]) / "last-run").read_text())
            self.assertEqual((obs["home"], obs["cwd"]), (owner["home"], str(self.s.root)))
            self.assertEqual(obs["proxy"], ["https://proxy.invalid:8443/"] * 4)
            self.assertFalse(Path(obs["runtime"]).exists())
            row = self.s.api("GET","startups?tool=agy")[1]["items"][0]
            self.assertEqual((row["config_id"],row["intent"],row["state"],row["native_id"]),
                             (owner["id"],"tui","routed",None))
            self.assertNotIn("private initial prompt", str(row))
        for args in [("--continue",), ("--conversation","unknown"), ("--settings","{}"),
                     ("--sandbox","models"), ("--agent","--conversation=hidden"), ("--sandbox","-p","-i","secret"),
                     ("acp","--dangerously-skip-permissions"), ("--remote-control",), ("--new-project",)]:
            denied = self.s.cli("agy", *args)
            self.assertNotEqual(denied.returncode, 0, args)
            self.assertEqual(denied.stdout, "")
        self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"],[])

    def test_nonfatal_tui_interrupt_is_delivered_once(self):
        home = Path(self.profile("terminal")["home"])
        (home/"interactive-test").touch()
        pid, fd = pty.fork()
        if not pid:
            os.chdir(self.s.root)
            os.execve(svc.LAUNCHER,[str(svc.LAUNCHER),"agy","--sandbox"],self.s.env)
        try:
            deadline = time.monotonic() + 5
            while not (home/"tui-ready").exists():
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.03)
            os.write(fd,b"\x03")
            time.sleep(2.3)
            os.write(fd,b"still here\n")
            deadline = time.monotonic() + 5
            while True:
                done, status = os.waitpid(pid,os.WNOHANG)
                if done:
                    pid = 0
                    self.assertEqual(status,0)
                    break
                self.assertLess(time.monotonic(),deadline)
                time.sleep(.03)
            self.assertEqual((home/"interrupts").read_text().splitlines(),["SIGINT"])
        finally:
            if pid:
                try: os.killpg(pid,signal.SIGKILL)
                except ProcessLookupError: pass
                try: os.waitpid(pid,0)
                except ChildProcessError: pass
            os.close(fd)

    def test_provider_named_link_dispatches_tui_and_acp(self):
        self.profile("linked")
        alias = self.s.root / "aliases/agy"
        alias.parent.mkdir()
        alias.symlink_to(svc.LAUNCHER)
        self.s.env["PATH"] = str(alias.parent) + ":" + self.s.env["PATH"]
        for args, wire, expected in [([], "", "fixture Antigravity TUI"),
                (["acp"], PRELUDE, '"protocolVersion"'),
                (["acp"], '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n', '"protocolVersion"')]:
            result = subprocess.run(["agy", *args], input=wire, text=True, capture_output=True,
                                    env=self.s.env, cwd=self.s.root, timeout=8)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(expected, result.stdout)

    def test_model_discovery_requires_current_and_preserves_native_streams_and_rotation(self):
        a, b = self.profile("first"), self.profile("catalog")
        alias = self.s.root / "agy"
        alias.symlink_to(svc.LAUNCHER)
        def models(*extra):
            return subprocess.run([alias,"models",*extra], stdin=subprocess.DEVNULL, text=True,
                                  capture_output=True, cwd=self.s.root, env=self.s.env, timeout=8)
        self.assertIn("Set a default account", models().stderr)
        self.s.api("PUT", "current/agy", {"config_id":b["id"]})
        failed = models()
        self.assertEqual((failed.returncode, failed.stdout), (1, ""))
        self.assertEqual(failed.stderr, "Fetching available models...\nError: Please sign in to view available models.\n")
        self.assertEqual(self.s.cli("login",str(b["id"])).returncode,0)
        result = models()
        self.assertEqual((result.returncode, result.stdout), (0, "fixture-model\tFixture Model\n"))
        self.assertEqual(result.stderr, "Fetching available models...\n")
        row = self.s.api("GET","startups?tool=agy")[1]["items"][0]
        self.assertEqual((row["config_id"],row["intent"],row["reason"]), (b["id"],"misc","current"))
        self.s.api("PUT", "current/agy", {"config_id":a["id"]})
        self.assertIn("Please sign in", models().stderr)
        self.assertNotEqual(models("--extra").returncode, 0)
        self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"], [])
        self.assertEqual(self.s.cli("agy").returncode, 0)
        self.assertEqual(self.s.api("GET","startups?tool=agy")[1]["items"][0]["config_id"], a["id"])

    def test_tui_resume_uses_the_unique_saved_file_not_rotation(self):
        a, b, c = self.profile("first"), self.profile("owner"), self.profile("third")
        uid = str(uuid.uuid4())
        p, q = (Path(c["home"])/".gemini/antigravity-cli/conversations" for c in (a, b))
        for d in (p, q):
            for parent in [d.parents[1], d.parent, d]:
                parent.mkdir(mode=0o700, exist_ok=True)
        (self.s.root / "agy").symlink_to(svc.LAUNCHER)
        self.s.env["PATH"] = str(self.s.root) + ":" + self.s.env["PATH"]
        def run(*args):
            return subprocess.run(["agy", *(args or ("--conversation="+uid, "--dangerously-skip-permissions"))],
                                  env=self.s.env, cwd=self.s.root, stdin=subprocess.DEVNULL,
                                  text=True, capture_output=True, timeout=8)
        self.assertNotEqual(run().returncode, 0)
        self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"], [])
        for ext in ("db", "pb"):
            saved = q / f"{uid}.{ext}"
            saved.touch()
            self.assertNotEqual(run().returncode, 0)
            saved.write_bytes(b"fixture: native saved conversation")
            for args in (["--conversation", uid], ["--conversation=" + uid, "--dangerously-skip-permissions"],
                         ["--dangerously-skip-permissions", "--conversation", uid],
                         ["--mode=plan","--effort","high","--conversation",uid,"--agent=acp",
                          "--dangerously-skip-permissions=false"]):
                result = run(*args)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(json.loads(result.stdout), {"args":args, "home":b["home"]})
                row = self.s.api("GET","startups?tool=agy")[1]["items"][0]
                self.assertEqual((row["config_id"],row["intent"],row["reason"],row["native_id"]),
                                 (b["id"],"tui_resume","sticky",uid))
            self.assertNotEqual(run("--conversation",uid,"--sandbox","--conversation="+uid).returncode,0)
            row, = self.s.api("GET","sessions?tool=agy")[1]["items"]
            self.assertEqual((row["config_id"],row["native_id"],row["evidence"],row["workspace"]),
                             (b["id"],uid,"saved_file",str(self.s.root)))
            duplicate = p / saved.name
            duplicate.symlink_to(saved)
            self.assertNotEqual(run().returncode, 0)
            duplicate.unlink()
            duplicate.write_bytes(b"copy")
            self.assertIn("Conflicting", run().stderr)
            config_dir = Path(a["home"])/".config"
            config_dir.mkdir(mode=0o700, exist_ok=True)
            config_dir.chmod(0o755)
            self.assertIn("Conflicting", run().stderr)
            config_dir.chmod(0o700)
            duplicate.unlink()
            saved.unlink()
            self.assertIn("pruned", run().stderr)
        (p/f"{uid}.db").write_bytes(b"moved")
        self.assertIn("Conflicting", run().stderr)
        self.assertNotEqual(self.s.cli("agy","acp","--for-session",uid).returncode,0)
        # ACP has its own observed IDs, including a conflicting file owned elsewhere.
        uid = str(uuid.uuid4())
        self.bridge([], (1, "session/new", {"cwd":"/work", "fixtureId":uid}))
        (q/f"{uid}.db").write_bytes(b"fixture")
        self.assertIn("Conflicting", run().stderr)
        self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"][0]["config_id"], a["id"])
        uid = str(uuid.uuid4())
        (q/f"{uid}.db").write_bytes(b"other fixture")
        self.s.api("PATCH",f"accounts/{b['id']}",{"revision":b["revision"],"enabled":False})
        self.assertIn("disabled", run().stderr)
        # Discovery may already have recorded its file, never an ACP observation.
        self.assertNotEqual(self.s.cli("agy","acp","--for-session",uid).returncode,0)
        self.assertEqual(self.s.cli("agy").returncode, 0)
        self.assertEqual(self.s.api("GET","startups?tool=agy")[1]["items"][0]["config_id"], c["id"])

    def test_saved_file_discovery_and_offline_reconciliation(self):
        a, b = self.profile("first"), self.profile("second")
        uid = lambda: str(uuid.uuid4())
        def directory(c):
            d = Path(c["home"])/".gemini/antigravity-cli/conversations"
            for level in (d.parents[1], d.parent, d):
                level.mkdir(mode=0o700, exist_ok=True)
            return d
        def rows(sid):
            return self.s.api("GET","sessions?tool=agy&native_id="+sid)[1]["items"]
        def wait(sid, ready=lambda row: True):
            deadline = time.monotonic() + 6
            while not (found := rows(sid)) or not ready(found[0]):
                self.assertLess(time.monotonic(), deadline, "Missing saved-file discovery")
                time.sleep(.04)
            self.assertEqual(len(found), 1)
            return found[0]
        def login():
            self.s.session = self.s.api("POST","login", {"password":self.s.password})[1]["session"]

        self.assertEqual(self.s.cli("agy").returncode, 0)
        original, = self.s.api("GET","startups?tool=agy")[1]["items"]
        self.assertIsNone(original["native_id"])
        # Create the conversations directories after both registration and launch.
        da, db = directory(a), directory(b)
        seed = uid()
        staged = da/"staged"
        staged.write_bytes(b"inert")
        staged.chmod(0)  # Discovery needs stat access, never transcript read access.
        staged.rename(da/(seed+".db"))
        first = wait(seed)
        self.assertEqual((first["config_id"], first["evidence"], first["workspace"],
                          first["last_routed_ms"], first["tui_workspace"]),
                         (a["id"], "saved_file", None, None, None))
        self.assertEqual(first["saved_file_seen_ms"], first["bound_ms"])

        left, right = uid(), uid()
        empty = da/(left+".db")
        empty.touch()
        self.assertFalse(rows(left))
        def write():
            with empty.open("ab", buffering=0) as stream:
                stream.write(b"nonempty")
                wait(left)  # MODIFY before close, not just CREATE or CLOSE_WRITE.
        def move():
            tmp = db/"staging"
            tmp.write_bytes(b"inert")
            tmp.rename(db/(right+".pb"))
            wait(right)
        with ThreadPoolExecutor(2) as pool:
            jobs = [pool.submit(fn) for fn in (write, move)]
            for job in jobs: job.result(timeout=8)
        self.assertEqual(rows(left)[0]["config_id"], a["id"])
        self.assertEqual(rows(right)[0]["config_id"], b["id"])
        (da/(seed+".pb")).write_bytes(b"same uuid")
        self.assertEqual(rows(seed), [first])
        self.assertEqual(self.s.api("GET","startups?tool=agy")[1]["items"], [original])
        self.assertNotEqual(self.s.cli("agy","acp","--for-session",seed).returncode,0)
        self.assertEqual(self.s.cli("agy").returncode,0)
        self.assertEqual(self.s.api("GET","startups?tool=agy")[1]["items"][0]["config_id"], b["id"])

        seen_ms = rows(left)[0]["saved_file_seen_ms"]
        self.assertEqual(self.s.cli("agy","--conversation="+left).returncode,0)
        self.assertEqual(rows(left)[0]["tui_workspace"], str(self.s.root))
        # Only session/new supplies ACP evidence, whether it precedes or follows the file.
        acp = uid()
        self.bridge([], (1, "session/new", {"cwd":"/verified","fixtureId":left}),
                        (2, "session/new", {"cwd":"/acp-only","fixtureId":acp}))
        dual = rows(left)[0]
        self.assertEqual((dual["evidence"],dual["workspace"],dual["tui_workspace"],dual["saved_file_seen_ms"]),
                         ("observed","/verified",str(self.s.root),seen_ms))
        (da/(acp+".db")).write_bytes(b"inert")
        after = wait(acp, lambda row: row["saved_file_seen_ms"] is not None)
        self.assertEqual((after["evidence"],after["workspace"],after["tui_workspace"]), ("observed","/acp-only",None))

        self.s.api("PATCH",f"accounts/{b['id']}",{"revision":b["revision"],"enabled":False})
        self.s.stop()
        offline, dup, bad, linked = uid(), uid(), uid(), uid()
        (db/(offline+".db")).write_bytes(b"saved while stopped")
        for d in (da, db): (d/(dup+".db")).write_bytes(b"ambiguous")
        (da/(bad+".db")).symlink_to(db/(offline+".db"))
        (da/"not-a-conversation").write_bytes(b"inert")
        os.link(da/"not-a-conversation",da/(linked+".db"))
        (da/(seed+".db")).unlink()
        (da/(seed+".pb")).unlink()
        self.s.start()
        login()
        self.assertEqual(wait(offline)["config_id"], b["id"])
        self.assertIn("disabled", self.s.cli("agy","--conversation="+offline).stderr)
        self.assertEqual(rows(seed), [first])
        self.assertIn("pruned", self.s.cli("agy","--conversation="+seed).stderr)
        self.assertIn("Conflicting", self.s.cli("agy","--conversation="+dup).stderr)
        for sid in (dup,bad,linked): self.assertFalse(rows(sid))
        # A file moved to another profile doesn't transfer a historical binding.
        (db/(seed+".db")).write_bytes(b"copy")
        self.assertIn("Conflicting", self.s.cli("agy","--conversation="+seed).stderr)
        self.assertEqual(rows(seed), [first])

    def test_native_upgrade_keeps_discovery_models_and_acp_working(self):
        c = self.profile("version")
        self.login_markers(c)
        self.s.api("PUT", "current/agy", {"config_id":c["id"]})
        fake = self.s.root/"bin/agy"
        fake.write_text(fake.read_text().replace("1.5.14", "1.5.16"))
        d = Path(c["home"])/".gemini/antigravity-cli/conversations"
        for level in (d.parents[1], d.parent, d):
            level.mkdir(mode=0o700, exist_ok=True)
        for restart in (False, True):
            if restart:
                self.s.stop()
                self.s.start()
                self.s.session = self.s.api("POST","login", {"password":self.s.password})[1]["session"]
            sid = str(uuid.uuid4())
            (d/(sid+".pb")).write_bytes(b"inert")
            self.assertEqual(self.s.cli("agy").returncode, 0)
            self.assertIn("fixture-model", self.s.cli("agy", "models").stdout)
            with self.connection() as call:
                reply = call(2, "session/new", {"cwd": "/work", "fixtureId": "chat-"+sid})
                self.assertEqual(reply["result"]["sessionId"], "chat-"+sid)
            deadline = time.monotonic() + 5
            while not any(r["native_id"] == sid for r in self.s.api("GET","sessions?tool=agy")[1]["items"]):
                self.assertLess(time.monotonic(), deadline, sid)
                time.sleep(.05)

    def refresh_quota(self, c=None, stale_only=False):
        route = f"accounts/{c['id']}/quota/refresh" if c else "quota/refresh"
        status, data, _ = self.s.api("POST", route, {"stale_only": stale_only})
        self.assertEqual(status, 202, data)
        return data["queued"]

    def wait_quota(self, c, status):
        deadline = time.monotonic() + 5
        while True:
            rows = self.s.api("GET", "accounts?tool=agy")[1]["items"]
            q = next(row["quota"] for row in rows if row["id"] == c["id"])
            if not q["loading"] and q["status"] == status:
                return q
            self.assertLess(time.monotonic(), deadline, q)
            time.sleep(.02)

    def allow_retry(self, c):
        with closing(sqlite3.connect(self.s.root/"state/cc-profile/router.sqlite3")) as db, db:
            db.execute("UPDATE quota_cache SET retry_ms=0 WHERE config_id=?", (c["id"],))

    def test_quota_cache_needs_login_and_withholds_unexpected_output(self):
        c = self.profile("report", enable=False)
        cid, home = str(c["id"]), Path(c["home"])
        self.assertEqual(self.refresh_quota(stale_only=True), 0)  # Disabled, but can check manually.
        self.assertEqual(self.refresh_quota(c), 1)
        self.assertIsNone(self.wait_quota(c, "sign_in_required")["snapshot"])
        self.assertEqual(self.refresh_quota(c), 0)
        self.assertFalse((home/"usage-calls").exists())
        self.assertEqual(self.s.cli("login",cid).returncode,0)
        before = self.s.api("GET","startups?tool=agy")[1]
        self.assertEqual(self.refresh_quota(c), 1)  # Login changed during backoff.
        good = self.wait_quota(c, "fresh")
        self.assertFalse(good["stale"])
        self.assertEqual(good["snapshot"], {"windows":[
            {"kind":"model","label":"Gemini Models, Weekly","used_pct":24,"reset_at_ms":1893456000000},
            {"kind":"model","label":"Gemini Models, 5 hours","used_pct":0,"reset_at_ms":None},
            {"kind":"model","label":"Claude and GPT models, Weekly","used_pct":100,"reset_at_ms":None},
        ]})
        obs = json.loads((home/"last-run").read_text())
        self.assertEqual(obs["proxy"],["https://proxy.invalid:8443/"]*4)
        self.assertFalse(Path(obs["runtime"]).exists())
        command = {"name":"usage","data":{"groups":[{"name":"Gemini Models","buckets":[
            {"window":"weekly","remaining_fraction":0.5}]}]}}
        valid = {"status":"SUCCESS","num_turns":0,"command":command}
        invalid = ["Model quotas: fixture-private", json.dumps(dict(valid,unused="x"*65536)),
                   json.dumps(dict(valid,status="ERROR")), json.dumps(dict(valid,num_turns=1)),
                   json.dumps({"status":"SUCCESS","command":command}),
                   json.dumps(dict(valid,command=dict(command,name="fixture-private"))),
                   json.dumps(dict(valid,command={"name":"usage","data":{"groups":[]}})),
                   json.dumps(dict(valid,command={"name":"usage","data":{"groups":"fixture-private"}}))]
        for raw in invalid:
            (home/"report.json").write_text(raw)
            self.allow_retry(c)
            self.assertEqual(self.refresh_quota(c), 1)
            q = self.wait_quota(c, "unavailable")
            self.assertTrue(q["stale"])
            self.assertEqual((q["success_ms"], q["snapshot"]), (good["success_ms"], good["snapshot"]))
            self.assertGreaterEqual(q["retry_ms"] - q["attempt_ms"], 30000)
            self.assertEqual(self.refresh_quota(c), 0)
            self.assertNotIn("fixture-private", str(q))
        (home/"report.json").write_text(json.dumps(valid))
        (home/"fail-report").touch()
        self.allow_retry(c)
        self.assertEqual(self.refresh_quota(c), 1)
        self.assertTrue(self.wait_quota(c, "unavailable")["stale"])
        calls = [json.loads(line) for line in (home/"usage-calls").read_text().splitlines()]
        self.assertEqual(calls, [["--output-format","json","--settings",
                                 '{"disableSlashCommands":false}',"-p","/usage"]] * (2 + len(invalid)))
        self.assertEqual(self.s.api("GET","startups?tool=agy")[1],before)
        self.assertFalse(self.s.api("GET","sessions?tool=agy")[1]["items"])

    def test_panel_usage_rechecks_profile_and_login_after_native_exit(self):
        c = self.profile("hold")
        cid, home = str(c["id"]), Path(c["home"])
        self.assertEqual(self.s.cli("login",cid).returncode,0)
        token = home/".gemini/antigravity-cli/antigravity-oauth-token"
        hold = home/"hold-report"
        for n, edit in enumerate(({"label":"changed"}, {"proxy_mode":"direct"}, None), start=1):
            hold.touch()
            try:
                self.assertEqual(self.refresh_quota(stale_only=True), 1)
                calls = home/"usage-calls"
                deadline = time.monotonic() + 5
                while not calls.exists() or len(calls.read_text().splitlines()) < n:
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.02)
                self.assertEqual(self.refresh_quota(c), 0)
                self.assertEqual(self.refresh_quota(), 0)
                if edit:
                    status, c, _ = self.s.api("PATCH", f"accounts/{cid}", dict(edit, revision=c["revision"]))
                    self.assertEqual(status, 200)
                else:
                    replacement = token.with_suffix(".replacement")
                    replacement.touch(mode=0o600)
                    replacement.write_text("inert fixture marker")
                    replacement.replace(token)
            finally:
                hold.unlink()
            self.assertIsNone(self.wait_quota(c, "not_checked").get("snapshot"))
        self.assertEqual(self.refresh_quota(stale_only=True), 1)
        self.wait_quota(c, "fresh")
        self.assertEqual(json.loads((home/"last-run").read_text())["proxy"], [None]*4)
        self.allow_retry(c)
        self.assertEqual(self.refresh_quota(stale_only=True), 0)  # Fresh even outside cooldown.
        token.unlink()
        self.assertIsNone(self.wait_quota(c, "not_checked").get("snapshot"))

    def test_quota_worker_reprobes_upgrade_and_refuses_a_non_native_replacement(self):
        c = self.profile("pinned")
        self.assertEqual(self.s.cli("login", str(c["id"])).returncode, 0)
        exe = self.s.root/"bin/agy"
        exe.write_text(exe.read_text().replace('print("1.5.14")', 'print("1.5.99")'))
        self.assertEqual(self.refresh_quota(stale_only=True), 1)
        good = self.wait_quota(c, "fresh")
        calls = Path(c["home"])/"usage-calls"
        self.assertEqual(len(calls.read_text().splitlines()), 1)
        exe.write_text(exe.read_text().replace('print("1.5.99")', 'print("not the native tool")'))
        self.assertFalse(self.s.api("GET", "accounts?tool=agy")[1]["supported"])
        self.allow_retry(c)
        self.assertEqual(self.refresh_quota(c), 1)
        self.assertEqual(self.wait_quota(c, "unavailable")["snapshot"], good["snapshot"])
        self.assertEqual(len(calls.read_text().splitlines()), 1)

    def test_bulk_skips_profile_disabled_while_waiting_for_shared_slot(self):
        held = [Path(self.s.account("claude", name)["home"]) for name in ("a", "b")]
        for home in held:
            (home/".credentials.json").write_text('{"claudeAiOauth":{"accessToken":"private-test-token"}}')
            (home/"hold-fetch").touch()
        try:
            self.assertEqual(self.refresh_quota(), 2)
            deadline = time.monotonic() + 5
            while not all((home/"quota-count").exists() for home in held):
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.02)
            queued = self.profile("queued")
            self.assertEqual(self.s.cli("login", str(queued["id"])).returncode, 0)
            self.assertEqual(self.refresh_quota(stale_only=True), 1)
            self.assertEqual(self.s.api("PATCH", f"accounts/{queued['id']}", {
                "revision":queued["revision"], "enabled":False})[0], 200)
        finally:
            for home in held:
                (home/"hold-fetch").unlink()
        self.assertIsNone(self.wait_quota(queued, "not_checked").get("snapshot"))
        self.assertFalse((Path(queued["home"])/"usage-calls").exists())

    def test_acp_correlates_opaque_ids_and_checks_owner_before_forwarding(self):
        a,b=self.profile("a"),self.profile("b")
        first="s:opaque 'first'"
        new=lambda sid,ws,**extra: {"cwd":ws, "fixtureId":sid, **extra}
        replies=self.bridge([],("first","session/new",new(first,"/one",hold=True)),
                            (2,"session/new",new("CaseSensitive","/two",permission=True)))
        self.assertEqual([r["id"] for r in replies],[2,2,"first"])
        self.assertEqual(replies[0]["method"],"session/request_permission")
        rows=self.s.api("GET","sessions?tool=agy")[1]["items"]
        self.assertEqual({(r["native_id"],r["workspace"],r["config_id"]) for r in rows},
                         {(first,"/one",a["id"]),("CaseSensitive","/two",a["id"])})
        for method in ("session/load","session/resume"):
            restored, = self.bridge([], (1,method,{"sessionId":first}))
            self.assertEqual(restored["result"]["home"],a["home"])
            self.assertFalse((Path(b["home"])/"calls").exists())
        opened, wrong = sorted(self.bridge([], (1,"session/new",new("owned-by-b","/work")),
                                   (2,"session/resume",{"sessionId":first})),key=lambda r:r["id"])
        self.assertEqual(opened["result"]["sessionId"],"owned-by-b")
        self.assertIn("error",wrong)
        self.assertEqual((Path(b["home"])/"calls").read_text().splitlines(),["initialize","session/new"])
        result=self.bridge(["--for-session",first], (1,"session/load",{"sessionId":first}),
                           (2,"session/resume",{"sessionId":"CaseSensitive"}))
        self.assertEqual([r["result"]["home"] for r in result], [a["home"]]*2)
        self.assertNotEqual(self.s.cli("agy","acp","--for-session","unknown").returncode,0)
        self.s.api("PATCH",f"accounts/{a['id']}",{"revision":a["revision"],"enabled":False})
        self.assertNotEqual(self.s.cli("agy","acp","--for-session",first).returncode,0)
        refused, = self.bridge([], (1,"session/resume",{"sessionId":first}))
        self.assertIn("disabled",refused["error"]["message"])
        self.assertNotEqual(self.s.cli("agy","acp","--gemini-dir","/other").returncode,0)
        self.s.env["AGY_ADC_AUTH"]="true"
        self.assertIn("AGY_ADC_AUTH",self.s.cli("agy","acp").stderr)

    def test_unbound_browse_aggregates_pages_without_routing_and_restores_listed_owner(self):
        a,b,off = self.profile("a"),self.profile("b"),self.profile("disabled",False)
        self.login_markers(a,b)
        ha,hb = Path(a["home"]), Path(b["home"])
        row = lambda sid,cwd: {"sessionId":sid,"cwd":cwd,"additionalDirectories":["/extra a","/extra-b"],
                               "title":"only for browse","_meta":{"private":1}}
        (ha/"browse-pages").write_text(json.dumps({"":{"sessions":[row("a1","/other")],"nextCursor":"native/A"},
            "native/A":{"sessions":[row("a2","/work")]}}))
        (hb/"browse-pages").write_text(json.dumps({"":{"sessions":[row("b1","/work")]}}))
        bad, filtered, all_rows = self.bridge([], (1,"session/list",{"cursor":"foreign"}),
            (2,"session/list",{"cwd":"/work"}),(3,"session/list",{}))
        self.assertIn("error",bad)
        self.assertEqual([r["sessionId"] for r in filtered["result"]["sessions"]],["a2","b1"])
        self.assertEqual([r["sessionId"] for r in all_rows["result"]["sessions"]],["a1","a2","b1"])
        self.assertNotIn("_meta",str(all_rows))
        self.assertEqual(all_rows["result"]["sessions"][0]["additionalDirectories"],["/extra a","/extra-b"])
        self.assertFalse((Path(off["home"])/"last-run").exists())
        self.assertEqual(self.s.api("GET","startups?tool=agy")[1]["items"],[])
        mappings=self.s.api("GET","sessions?tool=agy")[1]["items"]
        self.assertEqual({(r["evidence"],r["last_routed_ms"]) for r in mappings},{("acp_listed",None)})
        self.assertNotIn("only for browse",str(mappings))
        for home,expected in [(ha,["initialize","session/list","session/list"]*2),
                              (hb,["initialize","session/list"]*2)]:
            self.assertEqual((home/"calls").read_text().splitlines(),expected)
            seen=json.loads((home/"last-run").read_text())
            self.assertEqual(seen["proxy"],["https://proxy.invalid:8443/"]*4)
            self.assertFalse(Path(seen["runtime"]).exists())
        restored, = self.bridge([], (1,"session/resume",{"sessionId":"b1","cwd":"/work"}))
        self.assertEqual(restored["result"]["home"],b["home"])
        self.assertEqual([json.loads(s) for s in (hb/"acp-wire").read_text().splitlines()][-3:-1],[INIT,INITIALIZED])
        browse, selected = self.bridge([], (1,"session/list",{}),(2,"session/load",{"sessionId":"b1","cwd":"/work"}))
        self.assertIn("sessions",browse["result"])
        self.assertEqual(selected["result"]["home"],b["home"])
        self.assertNotIn("session/load",(ha/"calls").read_text())
        self.bridge([], (1,"session/new",{"fixtureId":"next","cwd":"/work"}))
        self.assertEqual(self.s.api("GET","startups?tool=agy")[1]["items"][0]["config_id"],a["id"])

    def test_unbound_browse_is_atomic_on_duplicate_failure_and_profile_change(self):
        a,b=self.profile("a"),self.profile("b")
        self.login_markers(a,b)
        ha,hb=Path(a["home"]),Path(b["home"])
        one={"sessions":[{"sessionId":"first","cwd":"/a"}]}
        (ha/"browse-pages").write_text(json.dumps({"":one}))
        browse=lambda: self.bridge([], (1,"session/list",{}))[0]
        for pages in ({"":one},{"":{"sessions":[],"nextCursor":"loop"},"loop":{"sessions":[],"nextCursor":"loop"}},
                      {"":{"sessions":"private fixture detail"}},
                      {"":{"sessions":[{"sessionId":"large","cwd":"/b","title":"x"*(401*1024)}]}},
                      *[{"":{"sessions":[{"sessionId":"bad-dirs","cwd":"/b","additionalDirectories":dirs}]}}
                        for dirs in ("/work",["relative"],["/work"]*65)]):
            (hb/"browse-pages").write_text(json.dumps(pages))
            reply=browse()
            self.assertIn("error",reply)
            self.assertNotIn("private fixture",str(reply))
            self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"],[])
        (hb/"browse-pages").unlink()
        (hb/"hold-list").touch()
        (hb/"acp-wire").unlink()
        with ThreadPoolExecutor(1) as pool:
            job=pool.submit(browse)
            try:
                deadline=time.monotonic()+5
                while not (hb/"acp-wire").exists() or "session/list" not in (hb/"acp-wire").read_text():
                    self.assertLess(time.monotonic(),deadline)
                    time.sleep(.03)
                self.assertIn("Another AgY browse",browse()["error"]["message"])
                self.assertEqual(self.s.api("PATCH",f"accounts/{a['id']}",{"enabled":False,"revision":a["revision"]})[0],200)
            finally:
                (hb/"hold-list").unlink()
            self.assertIn("error",job.result())
        self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"],[])
        self.assertFalse(list((self.s.root/"run/cc-profile").glob("agy-*")))

    def test_list_conflicting_historical_owner_rolls_back_earlier_rows(self):
        a,b=self.profile("a"),self.profile("b")
        self.login_markers(a,b)
        self.bridge([], (1,"session/new",{"fixtureId":"old","cwd":"/work"}))
        (Path(b["home"])/"browse-pages").write_text(json.dumps({"":{"sessions":[
            {"sessionId":"before-conflict","cwd":"/new"},{"sessionId":"old","cwd":"/new"}]}}))
        refused, = self.bridge([], (2,"session/list",{}))
        self.assertIn("recorded owner",refused["error"]["message"])
        rows=self.s.api("GET","sessions?tool=agy")[1]["items"]
        self.assertEqual([(r["native_id"],r["config_id"],r["workspace"]) for r in rows],[("old",a["id"],"/work")])

    def test_missing_login_in_last_profile_starts_no_browse_workers(self):
        a,b=self.profile("ready"), self.profile("signed-out")
        self.login_markers(a)
        reply,=self.bridge([], (1,"session/list",{}))
        self.assertIn("no saved login",reply["error"]["message"])
        for c in (a,b):
            self.assertFalse((Path(c["home"])/"last-run").exists())
        self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"],[])
        self.assertEqual(self.s.api("GET","startups?tool=agy")[1]["items"],[])

    def test_bound_native_list_records_page_before_reply_or_refuses_it_atomically(self):
        a,b=self.profile("selected"),self.profile("other")
        self.login_markers(a,b)
        row=lambda sid: {"sessionId":sid,"cwd":"/native","title":"not persisted"}
        for sid in ("pin-a","belongs-to-b"):
            self.bridge([], (1,"session/new",{"fixtureId":sid,"cwd":"/work"}))
        pages=Path(a["home"])/"browse-pages"
        for n,args in enumerate([[],["--for-session","pin-a"]]):
            sid, second = f"listed-{n}", f"next-page-{n}"
            with closing(sqlite3.connect(self.s.root/"state/cc-profile/router.sqlite3")) as db, db:
                db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,bound_ms,saved_file_seen_ms) "
                           "VALUES('agy',?,?,'saved_file',1,1)",(sid,a["id"]))
            pages.write_text(json.dumps({"":{"sessions":[row(sid)],"nextCursor":"native-cursor"},
                                         "native-cursor":{"sessions":[row(second)]}}))
            with self.connection(*args) as call:
                if not args:
                    self.assertIn("result",call(1,"session/new",{"fixtureId":"warm-a","cwd":"/work"}))
                self.assertEqual(call(2,"session/list",{})["result"],{"sessions":[row(sid)],"nextCursor":"native-cursor"})
                self.assertEqual(call(3,"session/list",{"cursor":"native-cursor"})["result"]["sessions"],[row(second)])
                items=self.s.api("GET","sessions?tool=agy")[1]["items"]
                for uid in (sid,second):
                    mapping=next(r for r in items if r["native_id"]==uid)
                    self.assertEqual((mapping["config_id"],mapping["workspace"],mapping["evidence"],mapping["last_routed_ms"]),
                                     (a["id"],"/native","acp_listed",None))
                self.assertNotIn("not persisted",str(items))
                self.assertEqual(call(4,"session/load",{"sessionId":sid,"cwd":"/native"})["result"]["home"],a["home"])
                self.assertEqual(call(5,"session/resume",{"sessionId":second,"cwd":"/native"})["result"]["home"],a["home"])
                pages.write_text(json.dumps({"":{"sessions":[row("earlier-in-page"),row("belongs-to-b")]}}))
                refused=call(6,"session/list",{})
                self.assertNotIn("result",refused)
                self.assertIn("recorded owner",refused["error"]["message"])
                self.assertIn("error",call(7,"session/load",{"sessionId":"belongs-to-b","cwd":"/native"}))
                self.assertFalse(any(r["native_id"]=="earlier-in-page" for r in self.s.api("GET","sessions?tool=agy")[1]["items"]))
            restored,=self.bridge([], (1,"session/load",{"sessionId":sid,"cwd":"/native"}))
            self.assertEqual(restored["result"]["home"],a["home"])
        self.assertEqual((Path(b["home"])/"calls").read_text().splitlines(),["initialize","session/new"])

    def test_legacy_acp_admission_rejected_before_selection(self):
        a=self.profile("first")
        self.profile("second")
        args=dict(request_id=str(uuid.uuid4()),ticket="21"*32,tool="agy",cwd=str(self.s.root),
                  argv=["acp"],binary=str(self.s.root/"bin/agy"),override_names=[])
        for endpoint,body in [("/v3/launch",args),("/v3/agy-acp/check",args["binary"]),
                              ("/v3/launch",dict(args,argv=["acp","--stdio","--for-session","prior"]))]:
            self.assertEqual(self.private_status(endpoint,body),426)
        self.assertEqual(self.s.api("GET","startups?tool=agy")[1]["items"],[])
        self.bridge([], (1,"session/new",{"fixtureId":"first","cwd":"/work"}))
        self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"][0]["config_id"],a["id"])

    def test_bound_list_requires_routed_acp_ticket_and_enabled_owner(self):
        a=self.profile("owner")
        self.login_markers(a)
        launch=dict(request_id=str(uuid.uuid4()),ticket="1a"*32,tool="agy",argv=[],cwd=str(self.s.root),
                    binary=str(self.s.root/"bin/agy"),override_names=[])
        page=dict(request_id=launch["request_id"],ticket=launch["ticket"],sessions=[["not-listed","/work"]])
        self.assertEqual(self.private_status("/v3/launch",launch),200)  # A TUI ticket cannot attest to ACP rows.
        self.assertEqual(self.private_status("/v4/agy-acp/listed",page),403)
        home=Path(a["home"])
        (home/"browse-pages").write_text(json.dumps({"":{"sessions":[{"sessionId":"from-disabled","cwd":"/work"}]}}))
        with self.connection() as call, ThreadPoolExecutor(1) as pool:
            self.assertIn("result",call(1,"session/new",{"fixtureId":"observed","cwd":"/work"}))
            req_id=self.s.api("GET","startups?tool=agy")[1]["items"][0]["request_id"]
            self.assertEqual(self.private_status("/v4/agy-acp/listed",dict(page,request_id=req_id)),403)
            (home/"hold-list").touch()
            job=pool.submit(call, 2,"session/list",{})
            try:
                deadline=time.monotonic()+3
                while "session/list" not in (home/"acp-wire").read_text():
                    self.assertLess(time.monotonic(),deadline)
                    time.sleep(.02)
                self.assertEqual(self.s.api("PATCH",f"accounts/{a['id']}",dict(revision=a["revision"],enabled=False))[0],200)
            finally: (home/"hold-list").unlink()
            self.assertIn("error",job.result())
        self.assertEqual([r["native_id"] for r in self.s.api("GET","sessions?tool=agy")[1]["items"]],["observed"])

    def test_slow_browse_does_not_block_restore_or_leave_workers(self):
        a,b=self.profile("a"),self.profile("b")
        self.login_markers(a,b)
        self.bridge([], (1,"session/new",{"fixtureId":"known","cwd":"/work"}))
        home=Path(b["home"])
        (home/"hold-list").touch()
        wire=PRELUDE+json.dumps({"jsonrpc":"2.0","id":"browse","method":"session/list","params":{}})+"\n"
        with subprocess.Popen([svc.LAUNCHER,"agy","acp"],cwd=self.s.root,env=self.s.env,
                              stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True) as p:
            try:
                p.stdin.write(wire)
                p.stdin.flush()
                deadline=time.monotonic()+5
                while not (home/"acp-wire").exists() or "session/list" not in (home/"acp-wire").read_text():
                    self.assertLess(time.monotonic(),deadline)
                    time.sleep(.03)
                restored,=self.bridge([], (1,"session/load",{"sessionId":"known"}))
                self.assertEqual(restored["result"]["home"],a["home"])
                output,err=p.communicate(timeout=12)
                self.assertEqual(p.returncode,0,err)
                self.assertIn("No partial list",json.loads(output.splitlines()[-1])["error"]["message"])
            finally:
                (home/"hold-list").unlink()
                if p.poll() is None: p.kill()
        self.assertFalse(list((self.s.root/"run/cc-profile").glob("agy-*")))

    def test_lazy_protocol_failures_do_not_send_a_session_or_guess_an_owner(self):
        a,b=self.profile("a"),self.profile("b")
        denied=self.bridge([], (1,"session/prompt",{}), (2,"initialize",INIT["params"]),
            (3,"session/resume",{"sessionId":"unknown"}),(4,"session/new",{"cwd":"relative"}))
        self.assertTrue(all("error" in r for r in denied))
        for c in (a,b): self.assertFalse((Path(c["home"])/"last-run").exists())
        (Path(a["home"])/"incompatible-acp").touch()
        first=json.dumps({"jsonrpc":"2.0","id":"open","method":"session/new","params":{"cwd":"/work"}})
        p=subprocess.run([svc.LAUNCHER,"agy","acp"],input=PRELUDE+first+"\n",capture_output=True,
                         text=True,env=self.s.env,cwd=self.s.root,timeout=8)
        self.assertNotEqual(p.returncode,0)
        init,error=map(json.loads,p.stdout.splitlines())
        self.assertEqual((init["id"],error["id"]),("init","open"))
        self.assertIn("handshake",error["error"]["message"])
        self.assertEqual((Path(a["home"])/"calls").read_text().splitlines(),["initialize"])
        self.assertFalse((Path(b["home"])/"calls").exists())
        self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"],[])

    def test_private_profile_checks_apply_to_registration_and_existing_profiles(self):
        external = self.s.root/"external"
        external.mkdir(mode=0o755)
        external.chmod(0o755)
        status, _, _ = self.s.api("POST", "accounts", {"tool":"agy","label":"shared","home":str(external)})
        self.assertEqual(status,400)
        c = self.profile("protected")
        home = Path(c["home"])
        cli = lambda *args: self.s.cli("login",str(c["id"]),*args)
        (home/".config").symlink_to(external, target_is_directory=True)
        self.assertNotEqual(cli().returncode,0)
        (home/".config").unlink()
        self.assertEqual(cli().returncode,0)
        token = home/".gemini/antigravity-cli/antigravity-oauth-token"
        token.chmod(0o644)
        self.assertNotEqual(cli("--status").returncode,0)
        token.chmod(0o600)
        os.link(token, external/"linked")
        self.assertNotEqual(cli("--status").returncode,0)
        token.unlink()
        token.symlink_to(external/"linked")
        self.assertNotEqual(cli("--status").returncode,0)

    def test_live_connection_rechecks_admission_and_does_not_offer_unmanaged_login(self):
        c = self.profile("live")
        with subprocess.Popen([svc.LAUNCHER,"agy","acp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE,text=True,cwd=self.s.root,env=self.s.env) as child:
            def call(n, method, params):
                child.stdin.write(json.dumps({"jsonrpc":"2.0","id":n,"method":method,"params":params})+"\n")
                child.stdin.flush()
                self.assertTrue(select.select([child.stdout],[],[],5)[0], "Missing ACP response")
                return json.loads(child.stdout.readline())
            try:
                self.assertEqual(call(1,"initialize",INIT["params"])["result"]["authMethods"],[])
                self.assertIn("ccp login",call(2,"authenticate",{"methodId":"terminal-login"})["error"]["message"])
                self.assertFalse((Path(c["home"])/"calls").exists())
                self.assertEqual(self.s.api("GET","startups?tool=agy")[1]["items"],[])
                self.assertEqual(call("bind","session/new",{"fixtureId":"first","cwd":"/work"})["result"]["sessionId"],"first")
                self.s.api("PATCH",f"accounts/{c['id']}",{"revision":c["revision"],"enabled":False})
                self.assertIn("error",call(3,"session/new",{"fixtureId":"refused","cwd":"/work"}))
                child.stdin.close()
                self.assertEqual(child.wait(timeout=5),0)
            finally:
                if child.poll() is None: child.kill()
        self.assertEqual((Path(c["home"])/"calls").read_text().splitlines(),["initialize","session/new"])
        self.assertEqual([r["native_id"] for r in self.s.api("GET","sessions?tool=agy")[1]["items"]],["first"])

    def test_acp_priority_pins_connection_and_native_reports_remain_unranked(self):
        a,b = self.profile("a"),self.profile("b")
        self.assertEqual(self.s.api("PATCH",f"accounts/{b['id']}",{"revision":b["revision"],"priority":90})[0],200)
        for c in (a, b):
            self.assertEqual(self.s.cli("login", str(c["id"])).returncode, 0)
        (Path(a["home"])/"report.json").write_text(json.dumps({
            "status":"SUCCESS", "num_turns":0, "command":{"name":"usage", "data":{"groups":[
                {"name":"Gemini Models", "buckets":[{"window":"weekly", "remaining_fraction":0}]}]}}}))
        self.assertEqual(self.refresh_quota(stale_only=True), 2)
        self.assertEqual(self.wait_quota(a, "fresh")["snapshot"]["windows"][0]["used_pct"], 100)
        self.wait_quota(b, "fresh")
        self.assertEqual(self.s.api("PUT","settings",{"routing_mode":"priority"})[0],200)
        new = lambda n: (n,"session/new",{"fixtureId":str(n),"cwd":"/work"})
        self.bridge([],new(1),new(2))
        self.assertEqual([r["config_id"] for r in self.s.api("GET","sessions")[1]["items"]],[b["id"]]*2)
        self.assertEqual(self.s.api("PUT","settings",{"routing_mode":"most_remaining"})[0],200)
        self.bridge([],new(3))
        self.assertEqual(self.s.api("GET","sessions")[1]["items"][0]["config_id"],a["id"])
        result, = self.bridge(["--for-session","1"],(1,"session/resume",{"sessionId":"1"}))
        self.assertEqual(result["result"]["home"],b["home"])
        with closing(sqlite3.connect(self.s.root/"state/cc-profile/router.sqlite3")) as db, db:
            db.execute("UPDATE quota_cache SET success_ms=1,attempt_ms=1,retry_ms=0")
        self.assertEqual(self.s.api("PUT","settings",{"routing_mode":"earliest_expiry"})[0],200)
        self.bridge([], new(4))
        self.assertEqual(self.s.api("GET","sessions")[1]["items"][0]["config_id"],b["id"])
        for c in (a, b):
            self.assertTrue(self.wait_quota(c, "fresh")["stale"])
            self.assertEqual(len((Path(c["home"])/"usage-calls").read_text().splitlines()), 1)

    def test_conflicting_native_reply_is_not_released_or_rebound(self):
        a = self.profile("a")
        self.profile("b")
        args = {"cwd":"/work", "fixtureId":"already-bound"}
        self.bridge([], (1,"session/new",args))
        data = json.dumps({"jsonrpc":"2.0","id":1,"method":"session/new","params":args})+"\n"
        invalid = json.dumps({"jsonrpc":"2.0","id":1,"method":"session/new",
                              "params":dict(args,fixtureId="not-bound",invalidReply=True)})+"\n"
        for wire in (data, "not json\n", data.replace("already-bound", "x" * (8 << 20)), invalid):
            result = subprocess.run([svc.LAUNCHER,"agy","acp"],input=PRELUDE+wire,cwd=self.s.root,env=self.s.env,
                                    capture_output=True,text=True,timeout=8)
            self.assertNotEqual(result.returncode,0)
            self.assertEqual([json.loads(line)["id"] for line in result.stdout.splitlines()],["init"])
        rows = self.s.api("GET","sessions?tool=agy")[1]["items"]
        self.assertEqual([(r["native_id"],r["config_id"]) for r in rows],[("already-bound",a["id"])])
