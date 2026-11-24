from contextlib import contextmanager
import json
from pathlib import Path
import select
import subprocess
import unittest
import uuid

import test_service as svc


@unittest.skipUnless(svc.DAEMON.is_file() and svc.LAUNCHER.is_file(), "build the CCP pair first")
class DeferredCodex(unittest.TestCase):
    def setUp(self):
        self.s = svc.Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(self.s.doCleanups)
        self.s.setUp()
        self.a, self.b = [self.s.account("codex", label) for label in ("account-A", "account-B")]
        self.s.api("PUT", "current/codex", {"config_id": self.b["id"]})

    def connection(self, *calls):
        frames = [{"id":"hello","method":"initialize","params":{"clientInfo":{"name":"garcon","version":"1"},
                  "capabilities":{"experimentalApi":True,"requestAttestation":False}}}, {"method":"initialized"}]
        frames.extend({"id":n, "method":method, "params":params} for n,(method,params) in enumerate(calls))
        args = ["app-server","--listen","stdio://","--config","features.goals=false",
                "--config","features.wake_on_agent_messages=false",
                "--config","features.default_mode_request_user_input=true"]
        p = subprocess.run([svc.LAUNCHER,"codex",*args], env=self.s.env, cwd=self.s.root,
                           input="".join(json.dumps(f)+"\n" for f in frames), text=True,
                           capture_output=True, timeout=12)
        self.assertEqual(p.returncode,0,p.stderr)
        replies = [json.loads(line) for line in p.stdout.splitlines()]
        hello, *operations = replies
        self.assertEqual(hello["id"],"hello")
        path = Path(hello["result"]["codexHome"])
        self.assertTrue(path.is_absolute() and path.is_dir())
        self.assertNotIn(str(path), (self.a["home"],self.b["home"],str(self.s.root)))
        self.assertEqual(list(path.iterdir()), [])
        self.assertCountEqual([r["id"] for r in operations], range(len(calls)))
        return sorted(operations,key=lambda r:r["id"])

    def test_hello_and_default_account_probe_are_not_admissions(self):
        account = Path(self.b["home"])
        (account/"fixture-identity.json").write_text('{"account":{"email":"fixture@example.test"}}')
        self.connection()
        result, = self.connection(("account/read",{"refreshToken":False}))
        self.assertEqual(result["result"]["account"]["email"],"fixture@example.test")
        self.assertFalse(self.s.api("GET","startups")[1]["items"])
        self.assertFalse(self.s.api("GET","sessions")[1]["items"])
        self.assertFalse(list(self.s.scratch.glob("codex-init-*")))
        self.assertTrue((self.s.scratch/"codex-router-home").is_dir())
        first, = self.connection(("thread/start",{}))
        self.assertEqual(first["result"]["home"],self.a["home"])

    def test_cold_resume_fork_and_denials_never_select_a_different_profile(self):
        (first,), (other,) = [self.connection(("thread/start",{})) for _ in range(2)]
        ua, ub = [row["result"]["thread"]["id"] for row in (first,other)]
        self.assertEqual([row["result"]["home"] for row in (first,other)], [self.a["home"],self.b["home"]])
        misleading = self.a["home"]+"/wrong-path.jsonl"
        failed, resumed, foreign = self.connection(("thread/resume",{"threadId":str(uuid.uuid4())}),
            ("thread/resume",{"threadId":ub,"path":misleading}),
            ("thread/fork",{"threadId":ua,"path":misleading}))
        self.assertIn("error",failed)
        self.assertEqual(resumed["result"]["home"],self.b["home"])
        self.assertIn("error",foreign)
        bad, fork = self.connection(("thread/resume",{"threadId":ub,"history":[]}),
                                    ("thread/fork",{"threadId":ub,"path":misleading}))
        self.assertIn("error",bad)
        child = fork["result"]["thread"]["id"]
        self.assertNotEqual(child,ub)
        result, = self.connection(("thread/resume",{"threadId":child}))
        self.assertEqual(result["result"]["home"],self.b["home"])
        accounts = self.s.api("GET","accounts?tool=codex")[1]["items"]
        revision = next(c["revision"] for c in accounts if c["id"] == self.b["id"])
        self.assertEqual(self.s.api("PATCH","accounts/"+str(self.b["id"]),
                                    {"enabled":False,"revision":revision})[0],200)
        result, = self.connection(("thread/read",{"threadId":ub}))
        self.assertIn("error",result)

    def test_private_replay_failure_does_not_dispatch_or_retry_the_activation(self):
        for c in (self.a,self.b):
            (Path(c["home"])/"wrong-capabilities").touch()
        result, = self.connection(("thread/start",{}))
        self.assertIn("error",result)
        self.assertFalse(self.s.api("GET","sessions")[1]["items"])
        rows = self.s.api("GET","startups")[1]["items"]
        self.assertEqual([row["state"] for row in rows],["exec_failed"])

    def test_bound_list_confirms_before_reply_and_does_not_expose_historical_conflicts(self):
        db_only = str(uuid.uuid4())
        listed = {"id":db_only, "cwd":"/fixture/work", "name":"private fixture"}
        (Path(self.a["home"])/"listed-fixture.json").write_text(json.dumps([listed]))
        _, page = self.connection(("thread/start",{}),("thread/list",{}))
        self.assertEqual(page["result"]["data"],[listed])
        sessions = self.s.api("GET","sessions?native_id="+db_only)[1]["items"]
        self.assertEqual([(s["evidence"],s["last_routed_ms"]) for s in sessions],[("codex_listed",None)])
        resumed, = self.connection(("thread/resume",{"threadId":db_only}))
        self.assertEqual(resumed["result"]["home"],self.a["home"])
        (Path(self.b["home"])/"listed-fixture.json").write_text(json.dumps([listed]))
        _, rejected = self.connection(("thread/start",{}),("thread/list",{}))
        self.assertIn("error",rejected)
        self.assertNotIn("result",rejected)

    @contextmanager
    def live_connection(self):
        with subprocess.Popen([svc.LAUNCHER,"codex","app-server"],env=self.s.env,cwd=self.s.root,
                              stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True) as child:
            try:
                def rpc(method,params):
                    child.stdin.write(json.dumps({"id":7,"method":method,"params":params})+"\n")
                    child.stdin.flush()
                    self.assertTrue(select.select([child.stdout],[],[],8)[0])
                    return json.loads(child.stdout.readline())
                self.assertIn("result",rpc("initialize",{}))
                child.stdin.write('{"method":"initialized"}\n')
                yield rpc
            finally:
                child.terminate()
                child.wait(timeout=4)

    def test_disabling_the_bound_profile_blocks_another_start_not_existing_turn_traffic(self):
        with self.live_connection() as rpc:
            first = rpc("thread/start",{})["result"]
            self.assertEqual(first["home"],self.a["home"])
            self.assertEqual(self.s.api("PATCH","accounts/"+str(self.a["id"]),
                                        {"enabled":False,"revision":self.a["revision"]})[0],200)
            self.assertIn("error",rpc("thread/start",{}))
            self.assertIn("result",rpc("turn/interrupt",{"threadId":first["thread"]["id"]}))
        self.assertEqual(len(self.s.api("GET","sessions")[1]["items"]),1)

    def test_ephemeral_fork_can_be_read_on_its_live_connection_but_is_never_bound(self):
        with self.live_connection() as rpc:
            parent = rpc("thread/start",{})["result"]["thread"]["id"]
            fork = rpc("thread/fork",{"threadId":parent,"ephemeral":True})["result"]["thread"]
            self.assertEqual((fork["ephemeral"],fork["forkedFromId"]),(True,parent))
            self.assertEqual(rpc("thread/read",{"threadId":fork["id"]})["result"]["thread"]["id"],fork["id"])
        self.assertFalse(self.s.api("GET","sessions?native_id="+fork["id"])[1]["items"])
        denied, = self.connection(("thread/read",{"threadId":fork["id"]}))
        self.assertIn("error",denied)
