"""Private socket publication of native Codex ownership, without a live provider."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import http.client
import json
from pathlib import Path
import socket
import sqlite3
import unittest
import uuid

import test_service as svc


def uid():
    return str(uuid.uuid4())


@unittest.skipUnless(svc.DAEMON.is_file(), "build ccpd first")
class ListedCodex(unittest.TestCase):
    def setUp(self):
        self.s = svc.Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(self.s.doCleanups)
        self.s.setUp()
        self.a, self.b = [self.s.account("codex", name) for name in ("first", "second")]
        self.bin = str(self.s.root / "bin/codex")
        self.roster = [[c["id"], c["revision"]] for c in (self.b, self.a)]
        self.db_path = self.s.root / "state/cc-profile/router.sqlite3"

    def private(self, path, body):
        with socket.socket(socket.AF_UNIX) as sock, closing(http.client.HTTPConnection("localhost")) as conn:
            sock.settimeout(5)
            sock.connect(str(self.s.root / "run/cc-profile/launch.sock"))
            conn.sock = sock
            conn.request("POST", path, json.dumps(body), {"Content-Type": "application/json"})
            r = conn.getresponse()
            return r.status, r.read().decode()

    def listed(self, threads, **kwargs):
        body = dict(dict(binary=self.bin, profiles=self.roster, threads=threads), **kwargs)
        return self.private("/v4/codex-stdio/listed", body)

    def sessions(self):
        return self.s.api("GET", "sessions?tool=codex")[1]["items"]

    def test_old_launcher_cannot_eagerly_select_a_codex_stdio_profile(self):
        r = {"request_id":uid(),"ticket":"a"*64,"cwd":str(self.s.root),"tool":"codex",
             "argv":["app-server"],"binary":self.bin,"override_names":[]}
        self.assertEqual(self.private("/v3/launch", r)[0],426)
        self.assertFalse(self.s.api("GET","startups")[1]["items"])

    def test_active_only_native_results_cannot_publish_over_an_archived_competitor(self):
        thread = uid()
        name = f"rollout-2026-11-23T12-00-00-{thread}.jsonl"
        # Both files predate discovery: there is no historical owner to catch this.
        self.s.stop()
        for c, rel in [(self.a, "sessions/2026/11/23"), (self.b, "archived_sessions")]:
            directory = Path(c["home"]) / rel
            directory.mkdir(parents=True)
            (directory / name).write_bytes(b"not a transcript")
        self.s.start()
        self.s.session = self.s.api("POST", "login", {"password": self.s.password})[1]["session"]
        self.assertEqual(self.sessions(), [])
        rows = [[self.b["id"], uid(), "/first"], [self.a["id"], thread, "/from-active-list"]]
        status, data = self.listed(rows)
        self.assertEqual((status, json.loads(data)["error"]), (409, "rollout_conflict"))
        self.assertEqual(self.sessions(), [])
        self.assertEqual(self.s.api("GET", "startups")[1]["items"], [])

    def test_listed_only_thread_resumes_without_disk_but_never_overrules_the_disk_or_disabled_owner(self):
        aid, bid = self.a["id"], self.b["id"]
        x, y = uid(), uid()
        with closing(sqlite3.connect(self.db_path)) as db:
            cursor = lambda: db.execute("SELECT cursor FROM tools WHERE tool='codex'").fetchone()
            before = cursor()
            self.assertEqual(self.listed([[aid, x, "/native/a"], [bid, y, "/native/b"]]), (200, "2"))
            self.assertEqual(self.listed([[bid, y, "/native/changed"]]), (200, "1"))
            self.assertEqual(cursor(), before)
        rows = {r["native_id"]: r for r in self.sessions()}
        self.assertEqual(set(rows), {x,y})
        for key, owner, cwd in [(x, aid, "/native/a"), (y, bid, "/native/changed")]:
            row = rows[key]
            self.assertEqual((row["config_id"], row["workspace"], row["evidence"]), (owner, cwd, "codex_listed"))
            self.assertIsNone(row["last_routed_ms"])
            self.assertIsNone(row["saved_file_seen_ms"])
        self.assertEqual(self.s.api("GET", "startups")[1]["items"], [])
        p = self.s.cli("codex", "resume", y)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(json.loads(p.stdout)["home"], self.b["home"])
        self.assertIsNotNone(next(r for r in self.sessions() if r["native_id"] == y)["last_routed_ms"])

        archive = Path(self.a["home"]) / "archived_sessions"
        archive.mkdir()
        (archive / f"rollout-2026-11-23T12-00-00-{y}.jsonl").write_bytes(b"fixture")
        self.assertNotEqual(self.s.cli("codex", "resume", y).returncode, 0)
        self.assertEqual(self.s.api("GET", "startups")[1]["items"][0]["reason"], "rollout_conflict")
        self.s.api("PATCH", f"accounts/{aid}", {"enabled": False, "revision": self.a["revision"]})
        self.assertNotEqual(self.s.cli("codex", "resume", x).returncode, 0)
        self.assertEqual(self.s.api("GET", "startups")[1]["items"][0]["reason"], "owner_disabled")

    def test_bad_roster_owner_shape_or_directory_cannot_publish_a_partial_batch(self):
        aid, bid = self.a["id"], self.b["id"]
        old, x = uid(), "aaaabbbb-cccc-dddd-eeee-ffffffffffff"
        valid = [aid, x, "/first"]
        for data, extra in [
                ([valid], {"binary": self.bin+"-other"}),
                ([valid], {"profiles": self.roster[:1]}),
                ([valid], {"profiles": self.roster+self.roster[:1]}),
                ([valid], {"profiles": self.roster+[[999,1]]}),
                ([valid], {"profiles": [[aid,0], self.roster[0]]}),
                ([valid, [999, old, "/dir"]], {}),
                ([valid, [bid, x, "/dir"]], {}),
                ([valid, valid], {}),
                ([valid, [bid, x.upper(), "/dir"]], {}),
                ([valid, [bid, old, "relative"]], {}),
                ([valid, [bid, old, "/bad\n"]], {}),
                ([valid, [bid, old, "/" + "x"*4096]], {}),
                ([valid, [bid, old, "/dir", "private title"]], {}),
                ([valid], {"title": "private title"}),
                ([valid]*2049, {})]:
            with self.subTest(extra=list(extra), count=len(data)):
                self.assertGreaterEqual(self.listed(data, **extra)[0], 400)
                self.assertEqual(self.sessions(), [])
        self.assertEqual(self.listed([[bid, old, "/bound"]]), (200, "1"))
        self.s.api("PATCH", f"accounts/{bid}", {"enabled": False, "revision": self.b["revision"]})
        self.assertEqual(self.listed([valid])[0], 409)
        self.roster = [[aid, self.a["revision"]]]
        self.assertEqual(self.listed([valid, [aid, old, "/other"]])[0], 409)
        prior, = self.sessions()
        self.assertEqual((prior["native_id"], prior["config_id"]), (old, bid))
        home = Path(self.a["home"])
        home.rename(home.with_name("moved"))
        home.mkdir()
        self.assertEqual(self.listed([valid])[0], 409)
        self.assertEqual(self.sessions(), [prior])

    def test_concurrent_conflicting_publications_commit_one_whole_batch(self):
        shared = uid()
        lists = [[[c["id"], uid(), "/one"], [c["id"], shared, "/two"]] for c in (self.a, self.b)]
        with ThreadPoolExecutor(max_workers=2) as pool:
            statuses = [status for status, _ in pool.map(self.listed, lists)]
        self.assertEqual(sorted(statuses), [200,409])
        won = lists[statuses.index(200)]
        self.assertEqual({(r["config_id"], r["native_id"]) for r in self.sessions()},
                         {(owner, session) for owner, session, _ in won})

    def launch(self, path, argv):
        req = dict(request_id=uid(), ticket=uuid.uuid4().hex*2, binary=self.bin, tool="codex",
                   argv=argv, override_names=[], cwd=str(self.s.root))
        status, data = self.private(path, req)
        self.assertEqual(status, 200, data)
        return [req["request_id"], req["ticket"]], json.loads(data)["config_id"]

    def test_bound_listing_rejects_wrong_launch_kind_failed_launch_and_missing_owner(self):
        for path, argv in [("/v3/launch", []), ("/v4/codex-stdio/launch", ["app-server"])]:
            ticket, owner = self.launch(path, argv)
            if argv:
                self.assertEqual(self.private("/v2/exec-failure", dict(request_id=ticket[0],
                                             ticket=ticket[1], errno=2))[0], 200)
            self.assertEqual(self.listed([[owner, uid(), "/native"]], profiles=[], bound=ticket)[0], 403)
        ticket, owner = self.launch("/v4/codex-stdio/launch", ["app-server"])
        c = next(c for c in (self.a, self.b) if c["id"] == owner)
        home, moved = Path(c["home"]), self.s.root / "offline"
        home.rename(moved)
        home.symlink_to(moved)
        self.assertEqual(self.listed([[owner, uid(), "/native"]], profiles=[], bound=ticket)[0], 409)
        home.unlink()
        moved.rename(home)
        self.assertEqual(self.s.api("PATCH", f"accounts/{owner}", {"enabled": False, "revision": c["revision"]})[0], 200)
        self.assertEqual(self.listed([[owner, uid(), "/native"]], profiles=[], bound=ticket)[0], 409)
        self.assertEqual(self.sessions(), [])

    def test_bound_listing_uses_only_its_ticket_and_promotes_without_erasing_stronger_evidence(self):
        ticket, owner = self.launch("/v4/codex-stdio/launch", ["app-server"])
        c, other = (self.a, self.b) if owner == self.a["id"] else (self.b, self.a)
        x = uid()
        row = [owner, x, "/native"]
        def bound(threads, **kw):
            return self.listed(threads, **dict(dict(profiles=[], bound=ticket), **kw))
        with closing(sqlite3.connect(self.db_path)) as db, db:
            db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,bound_ms,saved_file_seen_ms) "
                       "VALUES('codex',?,?,'saved_file',1,1)", (x, owner))
        self.assertEqual(bound([row], bound=[uid(),ticket[1]])[0], 403)
        self.assertEqual(bound([row], bound=[ticket[0],"0"*64])[0], 403)
        self.assertGreaterEqual(bound([row, [other["id"], uid(), "/wrong"]])[0], 400)
        self.assertGreaterEqual(bound([row], profiles=self.roster)[0], 400)
        self.assertEqual(self.sessions()[0]["evidence"], "saved_file")
        # A physically absent, unrelated profile supplies no conflicting rollout evidence.
        Path(other["home"]).rename(self.s.root/"unavailable")
        starts = self.s.api("GET", "startups")[1]["items"]
        self.assertEqual(bound([row]), (200, "1"))
        listed, = self.sessions()
        self.assertEqual((listed["evidence"], listed["bound_ms"], listed["saved_file_seen_ms"], listed["last_routed_ms"]),
                         ("codex_listed", 1, 1, None))
        self.assertEqual(self.s.api("GET", "startups")[1]["items"], starts)
        status, data = self.private("/v2/observations", dict(request_id=ticket[0], ticket=ticket[1],
                    event="thread/start", source="app-server", native_id=x, workspace="/observed"))
        self.assertEqual(status, 200, data)
        observed, = self.sessions()
        self.assertEqual((observed["evidence"], observed["workspace"]), ("observed","/observed"))
        self.assertIsNotNone(observed["last_routed_ms"])
        self.assertEqual(bound([row], profiles=[[owner,c["revision"]]]), (200, "1"))
        self.assertEqual(self.sessions(), [observed])
        status, edited, _ = self.s.api("PATCH", f"accounts/{owner}", {"priority": 7, "revision": c["revision"]})
        self.assertEqual(status, 200)
        self.assertEqual(bound([[owner, uid(), "/other"]], profiles=[[owner,edited["revision"]]])[0], 409)
        self.assertEqual(self.sessions(), [observed])
