import json
import os
from pathlib import Path
import select
import subprocess
import unittest

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
        self.assertNotEqual(self.s.cli("login",cid,"--status").returncode,0)
        self.assertNotEqual(self.s.cli("usage",cid).returncode,0)
        self.assertEqual(self.s.cli("login",cid).returncode,0)
        self.assertIn("validity not checked",self.s.cli("login",cid,"--status").stdout)
        self.assertEqual((home/".gemini/antigravity-cli/antigravity-oauth-token").stat().st_mode & 0o777,0o600)
        self.assertEqual(self.s.cli("usage",cid).stdout.strip(),"Model quotas: weekly remaining 76%")
        obs=json.loads((home/"last-run").read_text())
        self.assertEqual(obs["proxy"],["https://proxy.invalid:8443/"]*4)
        self.assertFalse(Path(obs["runtime"]).exists())
        self.assertFalse(self.s.api("GET","sessions?tool=agy")[1]["items"])
        self.assertEqual(self.s.cli("agy","acp").returncode,1)

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
