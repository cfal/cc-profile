"""Managed Codex discovery over a real router socket and native JSONL pipes."""
from contextlib import contextmanager
import json
from pathlib import Path
import select
import subprocess
import sys
import time
import unittest
import uuid

import test_service as svc


FEATURES = ["--config", "features.goals=false", "--config", "features.wake_on_agent_messages=false",
            "--config", "features.default_mode_request_user_input=true"]
GARCON = {"cursor": None, "limit": 500, "sortKey": "updated_at", "sortDirection": "desc",
          "archived": False, "useStateDbOnly": True}
NATIVE = r'''
import json, os, sys, time, uuid
from pathlib import Path
if sys.argv[1:] == ["--version"]:
    print("codex-cli 0.172.0")
    sys.exit()
home = Path(os.environ["CODEX_HOME"])
for line in sys.stdin:
    call = json.loads(line)
    if "id" not in call: continue
    method, p = call["method"], call.get("params") or {}
    if method == "initialize":
        result = {"userAgent":"codex-cli/0.172.0", "codexHome":str(home)}
    else:
        cfg = json.loads((home/"fixture.json").read_text())
        rows = cfg["rows"]
        with (home/"calls").open("a") as log: print(json.dumps([method,p]), file=log)
        if method in ("thread/list","account/read") and (home/"hold").exists():
            (home/"waiting").touch()
            while (home/"hold").exists(): time.sleep(.02)
        if method == "thread/list":
            if cfg.get("fail"):
                print(json.dumps({"id":call["id"],"error":{"code":-1,"message":"private fixture diagnostic"}}),flush=True)
                continue
            rows = [r for r in rows if r.get("archived",False) == bool(p.get("archived"))]
            start = int(p.get("cursor") or 0)
            end = start + min(100, p.get("limit") or 25)
            page = rows[start:end]
            result = {"data":page, "nextCursor":str(end) if end<len(rows) else None,
                      "readStates":{r["id"]:{"n":r["updatedAt"]} for r in page}, "backwardsCursor":"private"}
            if cfg.get("loop"): result.update(data=[], readStates={}, nextCursor="0")
        elif method == "thread/resume":
            result = {"thread":next(r for r in rows if r["id"] == p["threadId"])}
        elif method == "thread/start":
            uid=str(uuid.uuid4())
            result={"thread":{"id":uid,"sessionId":uid,"cwd":p["cwd"],"ephemeral":False}}
        elif method == "account/read": result = {"account":{"label":home.name}}
        else: result = {}
    print(json.dumps({"id":call["id"],"result":result}),flush=True)
'''


def row(n):
    return {"id":str(uuid.UUID(int=n)), "createdAt":n, "updatedAt":n // 2, "cwd":"/fixture/work",
            "ephemeral":False, "path":None, "preview":f"thread {n}", "extra":{"retain":[n]}}


def fixture(account, rows, **extra):
    (Path(account["home"])/"fixture.json").write_text(json.dumps(dict(rows=rows, **extra)))


class Client:
    def __init__(self, process):
        self.p = process
        self.seq = 0

    def send(self, method, params):
        self.seq += 1
        self.p.stdin.write(json.dumps({"id":self.seq, "method":method, "params":params})+"\n")
        self.p.stdin.flush()

    def read(self):
        if not select.select([self.p.stdout], [], [], 40)[0]:
            raise AssertionError("Timed out waiting for Codex")
        reply = json.loads(self.p.stdout.readline())
        assert reply["id"] == self.seq
        return reply

    def call(self, method, params):
        self.send(method, params)
        return self.read()

    def listing(self, **params):
        return self.call("thread/list", GARCON | params)


@unittest.skipUnless(svc.DAEMON.is_file() and svc.LAUNCHER.is_file(), "build ccp and ccpd first")
class Browse(unittest.TestCase):
    def setUp(self):
        self.s = svc.Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(self.s.doCleanups)
        self.s.setUp()
        (self.s.root/"bin/codex").write_text("#!"+sys.executable+"\n"+NATIVE)
        self.a = self.s.account("codex", "alpha")
        self.b = self.s.account("codex", "beta")
        fixture(self.a, [row(1)])
        fixture(self.b, [row(2)])

    @contextmanager
    def connection(self):
        with subprocess.Popen([svc.LAUNCHER,"codex","app-server","--listen","stdio://",*FEATURES],
                              cwd=self.s.root, env=self.s.env, stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as child:
            try:
                c = Client(child)
                self.assertIn("result", c.call("initialize", {"clientInfo":{"name":"garcon","version":"fixture"},
                                  "capabilities":{"experimentalApi":True,"requestAttestation":False}}))
                child.stdin.write('{"method":"initialized"}\n')
                yield c
            finally:
                child.terminate()
                child.wait(timeout=5)

    def change(self, profile, **fields):
        code, updated, _ = self.s.api("PATCH", f"accounts/{profile['id']}",
                                      {"revision":profile["revision"], **fields})
        self.assertEqual(code, 200, updated)
        profile.update(updated)

    def items(self, name):
        return self.s.api("GET", name+"?tool=codex")[1]["items"]

    def assert_denied(self, reply):
        self.assertIn("error", reply)
        self.assertNotIn("result", reply)
        self.assertNotIn("private fixture diagnostic", str(reply))

    def test_global_pages_then_resume_db_only_non_default_owner(self):
        a_rows = [row(i) for i in range(1, 211, 2)]
        b_rows = [row(i) for i in range(2, 22, 2)]
        fixture(self.a, a_rows+[row(300)|{"archived":True}])
        fixture(self.b, b_rows)
        off = self.s.account("codex", "disabled")
        self.change(off, enabled=False)
        self.assertEqual(self.s.api("PUT","current/codex",{"config_id":self.a["id"]})[0],200)
        with self.connection() as c:
            pages = [c.listing(limit=37)["result"]]
            self.assertTrue(pages[0]["nextCursor"])
            while pages[-1]["nextCursor"]:
                pages.append(c.listing(cursor=pages[-1]["nextCursor"])["result"])
            merged = [r for p in pages for r in p["data"]]
            self.assertEqual(merged, sorted(a_rows+b_rows, key=lambda r:(r["updatedAt"],r["id"]),reverse=True))
            for page in pages:
                self.assertEqual(page["readStates"],{r["id"]:{"n":r["updatedAt"]} for r in page["data"]})
                self.assertIsNone(page["backwardsCursor"])
            self.assertFalse((Path(off["home"])/"calls").exists())
            self.assertEqual(self.items("startups"), [])
            uid = b_rows[0]["id"]
            saved, = self.s.api("GET",f"sessions?tool=codex&native_id={uid}")[1]["items"]
            self.assertEqual((saved["config_id"],saved["evidence"],saved["last_routed_ms"]),
                             (self.b["id"],"codex_listed",None))
            self.assertEqual(c.call("thread/resume",{"threadId":uid})["result"]["thread"]["id"],uid)
            self.assertEqual(c.listing()["result"]["data"],b_rows)
            self.assertEqual(self.items("startups")[0]["config_id"],self.b["id"])
        with self.connection() as c:
            self.assertEqual(c.listing(archived=True,sortKey="created_at",
                                       sortDirection="asc")["result"]["data"],[row(300)|{"archived":True}])
        a_calls = [json.loads(line) for line in (Path(self.a["home"])/"calls").read_text().splitlines()]
        self.assertTrue(any(m=="thread/list" and p.get("cursor") and p["useStateDbOnly"] for m,p in a_calls))

    def test_garcon_paging_loop_can_fetch_the_entire_supported_roster(self):
        fixture(self.a,[row(n) for n in range(1,1025)])
        fixture(self.b,[row(n) for n in range(1025,2049)])
        with self.connection() as c:
            ids, sizes, cursor = [], [], None
            for _ in range(20):
                page = c.listing(cursor=cursor)["result"]
                sizes.append(len(page["data"]))
                ids.extend(r["id"] for r in page["data"])
                cursor = page["nextCursor"]
                if cursor is None:
                    break
            self.assertEqual(sizes,[500,500,500,500,48])
            self.assertEqual(ids,[row(n)["id"] for n in range(2048,0,-1)])
            fixture(self.b,[row(n) for n in range(1025,2050)])
            self.assert_denied(c.listing())

    def test_cursor_is_connection_and_query_scoped_and_revision_checked(self):
        with self.connection() as c, self.connection() as other:
            cursor = c.listing(limit=1)["result"]["nextCursor"]
            self.assert_denied(other.listing(cursor=cursor))
            self.assert_denied(c.listing(cursor=cursor,archived=True))
            cursor = c.listing(limit=1)["result"]["nextCursor"]
            self.change(self.b, priority=1)
            self.change(self.b, priority=0)
            self.assert_denied(c.listing(cursor=cursor))
            self.assertEqual(len(c.listing()["result"]["data"]),2)
            first = c.listing(limit=1, sortKey="created_at", sortDirection="asc")["result"]
            self.assertEqual(first["data"],[row(1)])
            self.assertEqual(c.listing(cursor=first["nextCursor"], limit=1, sortKey="created_at",
                                       sortDirection="asc")["result"]["data"],[row(2)])
            self.assert_denied(c.listing(cursor=first["nextCursor"], limit=1, sortKey="created_at",
                                        sortDirection="asc"))

    def test_empty_roster_and_unmergeable_sort_do_not_launch_or_publish(self):
        with self.connection() as c:
            self.assert_denied(c.listing(sortKey="section_position"))
            self.change(self.a, enabled=False)
            self.change(self.b, enabled=False)
            result = c.listing()["result"]
            self.assertEqual((result["data"],result["nextCursor"]),([],None))
            self.assertEqual(self.items("startups"),[])
            self.assertEqual(self.items("sessions"),[])

    def test_incomplete_conflicting_and_over_budget_snapshots_never_publish(self):
        with self.connection() as c:
            for rows,extra in [
                ([row(2)], {"fail":True}), ([], {"loop":True}),
                ([row(1)], {}), ([row(2)|{"cwd":"relative"}], {}),
                ([row(i)|{"cwd":"/"+"q"*3000} for i in range(2,102)], {}),
            ]:
                fixture(self.b, rows, **extra)
                self.assert_denied(c.listing(limit=1))
                self.assertEqual(self.items("sessions"),[])
            fixture(self.b,[row(2)])
            c.listing()["result"]
            fixture(self.a,[])
            fixture(self.b,[row(1),row(2),row(3)])
            self.assert_denied(c.listing(limit=1))
            self.assertNotIn(row(3)["id"],[r["native_id"] for r in self.items("sessions")])
            self.assertEqual(self.items("startups"),[])

    def test_missing_participant_and_changed_roster_during_native_io(self):
        b = Path(self.b["home"])
        with self.connection() as c:
            b.rename(b.with_suffix(".moved"))
            self.assert_denied(c.listing())
            b.with_suffix(".moved").rename(b)
            (b/"hold").touch()
            c.send("thread/list", GARCON)
            deadline = time.monotonic()+5
            while not (b/"waiting").exists():
                self.assertLess(time.monotonic(),deadline)
                time.sleep(.02)
            self.change(self.a, enabled=False)
            (b/"hold").unlink()
            self.assert_denied(c.read())
            self.assertEqual(self.items("sessions"),[])
            self.assertEqual(c.listing()["result"]["data"],[row(2)])

    def test_default_utility_rechecks_before_returning_native_reply(self):
        home = Path(self.a["home"])
        self.assertEqual(self.s.api("PUT","current/codex",{"config_id":self.a["id"]})[0],200)
        (home/"hold").touch()
        with self.connection() as c:
            c.send("account/read", {"refreshToken":False})
            deadline = time.monotonic()+5
            while not (home/"waiting").exists():
                self.assertLess(time.monotonic(),deadline)
                time.sleep(.02)
            self.assertEqual(self.s.api("PUT","current/codex",{"config_id":self.b["id"]})[0],200)
            (home/"hold").unlink()
            self.assert_denied(c.read())
            self.assertEqual(c.call("account/read", {})["result"]["account"]["label"],"beta")

    def test_busy_sweep_does_not_block_cursor_or_new_thread(self):
        home = Path(self.a["home"])
        with self.connection() as cached, self.connection() as running, self.connection() as competitor:
            cursor = cached.listing(limit=1)["result"]["nextCursor"]
            (home/"hold").touch()
            try:
                running.send("thread/list",GARCON)
                deadline=time.monotonic()+5
                while not (home/"waiting").exists():
                    self.assertLess(time.monotonic(),deadline)
                    time.sleep(.02)
                busy = competitor.listing()
                self.assert_denied(busy)
                self.assertIn("in progress",busy["error"]["message"])
                self.assertEqual(len(cached.listing(cursor=cursor)["result"]["data"]),1)
                self.assertIn("thread",competitor.call("thread/start",{"cwd":str(self.s.root)})["result"])
            finally:
                (home/"hold").unlink()
            self.assertEqual(len(running.read()["result"]["data"]),2)
