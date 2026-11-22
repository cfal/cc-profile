import json
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
from contextlib import closing

import test_service as svc


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
        result = subprocess.run([svc.LAUNCHER,"agy","acp",*args],input=wire,text=True,capture_output=True,
                                cwd=self.s.root,env=self.s.env,timeout=8)
        self.assertEqual(result.returncode,0,result.stderr)
        return [json.loads(line) for line in result.stdout.splitlines()]

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
        self.assertNotEqual(self.s.cli("usage",cid,"--settings","{}").returncode,0)
        for argv in (["profile-usage"], ["-p", "/usage"], ["--settings", "{}", "-p", "/usage"]):
            self.assertNotEqual(self.s.cli("agy",*argv).returncode,0)
        self.assertEqual((home/"usage-calls").read_text().splitlines(),
                         [json.dumps(["--settings",'{"disableSlashCommands":false}',"-p","/usage"])])
        obs=json.loads((home/"last-run").read_text())
        self.assertEqual(obs["proxy"],["https://proxy.invalid:8443/"]*4)
        self.assertFalse(Path(obs["runtime"]).exists())
        self.assertFalse(self.s.api("GET","sessions?tool=agy")[1]["items"])
        self.assertEqual(self.s.cli("agy","acp").returncode,1)

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
                     ("--sandbox","models"), ("--agent","--conversation=hidden"), ("--sandbox","-p","secret"),
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
        self.assertIn("Set current", models().stderr)
        self.s.api("PUT", "current/agy", {"config_id":b["id"]})
        failed = models()
        self.assertEqual((failed.returncode, failed.stdout), (1, ""))
        self.assertIn("Please sign in", failed.stderr)
        self.assertEqual(self.s.cli("login",str(b["id"])).returncode,0)
        result = models()
        self.assertEqual((result.returncode, result.stdout), (0, "fixture-model\tFixture Model\n"))
        self.assertIn("Fetching available models", result.stderr)
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
            self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"],[])
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
        (q/f"{uid}.db").write_bytes(b"fixture")
        self.bridge([], (1, "session/new", {"cwd":"/work", "fixtureId":uid}))
        self.assertIn("Conflicting", run().stderr)
        self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"][0]["config_id"], a["id"])
        uid = str(uuid.uuid4())
        (q/f"{uid}.db").write_bytes(b"other fixture")
        self.s.api("PATCH",f"accounts/{b['id']}",{"revision":b["revision"],"enabled":False})
        self.assertIn("disabled", run().stderr)
        self.assertEqual(len(self.s.api("GET","sessions?tool=agy")[1]["items"]),1)
        self.assertEqual(self.s.cli("agy").returncode, 0)
        self.assertEqual(self.s.api("GET","startups?tool=agy")[1]["items"][0]["config_id"], c["id"])

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

    def test_quota_worker_never_runs_a_replaced_or_unsupported_binary(self):
        c = self.profile("pinned")
        self.assertEqual(self.s.cli("login", str(c["id"])).returncode, 0)
        exe = self.s.root/"bin/agy"
        exe.write_text(exe.read_text().replace('print("1.5.14")', 'print("1.5.99")'))
        self.assertEqual(self.refresh_quota(stale_only=True), 1)
        self.assertIsNone(self.wait_quota(c, "unavailable")["snapshot"])
        self.s.stop()
        self.s.start()
        self.s.session = self.s.api("POST", "login", {"password": self.s.password})[1]["session"]
        self.assertFalse(self.s.api("GET", "accounts?tool=agy")[1]["supported"])
        self.allow_retry(c)
        self.assertEqual(self.refresh_quota(c), 1)
        self.assertIsNone(self.wait_quota(c, "unavailable")["snapshot"])
        self.assertFalse((Path(c["home"])/"usage-calls").exists())

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
        wrong=self.bridge([], (1,"session/load",{"sessionId":first}), (2,"session/resume",{"sessionId":"unknown"}),
                          (3,"session/new",new("owned-by-b","/work")))
        self.assertTrue(all("error" in r for r in wrong[:2]))
        self.assertEqual((Path(b["home"])/"calls").read_text().splitlines(),["session/new"])
        result=self.bridge(["--for-session",first], (1,"session/load",{"sessionId":first}),
                           (2,"session/resume",{"sessionId":"CaseSensitive"}))
        self.assertEqual([r["result"]["home"] for r in result], [a["home"]]*2)
        self.assertNotEqual(self.s.cli("agy","acp","--for-session","unknown").returncode,0)
        self.s.api("PATCH",f"accounts/{a['id']}",{"revision":a["revision"],"enabled":False})
        self.assertNotEqual(self.s.cli("agy","acp","--for-session",first).returncode,0)
        self.assertNotEqual(self.s.cli("agy","acp","--gemini-dir","/other").returncode,0)
        self.s.env["AGY_ADC_AUTH"]="true"
        self.assertIn("AGY_ADC_AUTH",self.s.cli("agy","acp").stderr)

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
                self.assertEqual(call(1,"initialize",{})["result"]["authMethods"],[])
                self.assertIn("ccp login",call(2,"authenticate",{"methodId":"terminal-login"})["error"]["message"])
                self.s.api("PATCH",f"accounts/{c['id']}",{"revision":c["revision"],"enabled":False})
                self.assertIn("error",call(3,"session/new",{"fixtureId":"refused","cwd":"/work"}))
                child.stdin.close()
                self.assertEqual(child.wait(timeout=5),0)
            finally:
                if child.poll() is None: child.kill()
        self.assertEqual((Path(c["home"])/"calls").read_text().splitlines(),["initialize"])
        self.assertEqual(self.s.api("GET","sessions?tool=agy")[1]["items"],[])

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
        self.bridge([], new(4))
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
            result = subprocess.run([svc.LAUNCHER,"agy","acp"],input=wire,cwd=self.s.root,env=self.s.env,
                                    capture_output=True,text=True,timeout=8)
            self.assertNotEqual(result.returncode,0)
            self.assertEqual(result.stdout,"")
        rows = self.s.api("GET","sessions?tool=agy")[1]["items"]
        self.assertEqual([(r["native_id"],r["config_id"]) for r in rows],[("already-bound",a["id"])])
