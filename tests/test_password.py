import concurrent.futures
from contextlib import closing
import os
import pty
import select
import sqlite3
import time
import unittest

import test_service


@unittest.skipUnless(test_service.DAEMON.is_file(), "build ccpd first")
class PanelPassword(unittest.TestCase):
    def setUp(self):
        self.s = test_service.Service("test_web_authenticates_reads_and_mutations")
        self.s.claim = False
        self.addCleanup(self.s.doCleanups)
        self.s.setUp()

    def test_only_one_first_visitor_can_claim_and_it_survives_restart(self):
        s = self.s
        self.assertEqual(s.api("GET","session")[1], {"setup_required":True,"signed_in":False})
        self.assertFalse((s.root/"run/cc-profile/pairing-token").exists())
        self.assertNotEqual(s.api("POST","login",{"token":"obsolete token"})[0],200)
        self.assertEqual(s.api("GET","accounts?tool=claude")[0],403)
        self.assertEqual(s.api("POST","setup",{"password":"abc"})[0],400)
        self.assertEqual(s.api("POST","setup",{"password":s.password},{"Origin":"https://example.invalid"})[0],403)
        passwords = ["1234", " a b"]
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            results=list(pool.map(lambda p:s.api("POST","setup",{"password":p}),passwords))
        self.assertCountEqual([r[0] for r in results],[200,409])
        winner=next(i for i,r in enumerate(results) if r[0]==200)
        s.stop()
        s.start()
        self.assertEqual(s.api("GET","session")[1],{"setup_required":False,"signed_in":False})
        self.assertEqual(s.api("POST","login",{"password":passwords[1-winner]})[0],401)
        self.assertEqual(s.api("POST","login",{"password":passwords[winner]})[0],200)
        with closing(sqlite3.connect(s.root/"state/cc-profile/router.sqlite3")) as db, db:
            verifier,=db.execute("SELECT verifier FROM panel_password").fetchone()
            self.assertTrue(verifier.startswith("$argon2id$"))
            self.assertNotIn(passwords[winner],verifier)

    def set_local(self,password,confirmation):
        pid,fd=pty.fork()
        if pid==0:
            os.execve(str(test_service.DAEMON),["ccpd","set-password"],self.s.env)
        try:
            seen=b""
            for prompt, value in [(b"characters): ",password),(b"Confirm new password: ",confirmation)]:
                output=b""
                deadline=time.monotonic()+8
                while prompt not in output:
                    self.assertTrue(select.select([fd],[],[],max(0,deadline-time.monotonic()))[0],"password prompt timed out")
                    output+=os.read(fd,1024)
                seen+=output
                os.write(fd,(value+"\n").encode())
            status=os.waitstatus_to_exitcode(os.waitpid(pid,0)[1])
            self.assertIn(os.fsencode(self.s.root/"state/cc-profile/router.sqlite3"),seen)
            return status
        finally:
            os.close(fd)

    def test_terminal_setup_and_reset_revoke_existing_logins(self):
        s=self.s
        self.assertIn(str(s.root/"state/cc-profile/router.sqlite3"),s.p.stderr.readline())
        s.stop()
        self.assertEqual(self.set_local(s.password,s.password),0)
        s.start()
        self.assertEqual(s.api("POST","setup",{"password":"overwriting passphrase"})[0],409)
        _,data,headers=s.api("POST","login",{"password":s.password})
        s.session=data["session"]
        self.assertEqual(s.api("GET","accounts?tool=claude")[0],200)
        new="aaaa"
        self.assertNotEqual(self.set_local(new,"mismatch"),0)
        self.assertEqual(s.api("GET","accounts?tool=claude")[0],200)
        self.assertEqual(self.set_local(new,new),0)
        self.assertEqual(s.api("GET","accounts?tool=claude")[0],403)
        self.assertEqual(s.api("POST","login",{"password":s.password})[0],401)
        self.assertEqual(s.api("POST","login",{"password":new})[0],200)

    def test_panel_password_change_verifies_current_password_and_revokes_sessions(self):
        s = self.s
        _, account, _ = s.api("POST", "setup", {"password": s.password})
        other_tab = s.api("POST", "login", {"password": s.password})[1]["session"]
        change = {"current_password": s.password, "new_password": "1234"}
        self.assertEqual(s.api("POST", "password", change)[0], 403)
        s.session = account["session"]
        self.assertEqual(s.api("POST", "password", change, {"Origin": "https://example.invalid"})[0], 403)
        self.assertEqual(s.api("POST", "password", dict(change, current_password="wrong"))[0], 400)
        self.assertEqual(s.api("POST", "password", dict(change, new_password="abc"))[0], 400)
        self.assertEqual(s.api("POST", "password", change)[:2], (200, {}))
        self.assertFalse(s.api("GET", "session")[1]["signed_in"])
        s.session = other_tab
        self.assertEqual(s.api("GET", "accounts?tool=codex")[0], 403)
        s.stop()
        s.start()
        self.assertEqual(s.api("POST", "login", {"password": s.password})[0], 401)
        self.assertEqual(s.api("POST", "login", {"password": "1234"})[0], 200)

    def test_concurrent_password_changes_have_one_winner(self):
        s = self.s
        s.session = s.api("POST", "setup", {"password": s.password})[1]["session"]
        replacements = ["first replacement", "second replacement"]
        def change(new):
            return s.api("POST", "password", {"current_password": s.password, "new_password": new})[0]
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            statuses = list(pool.map(change, replacements))
        self.assertEqual(statuses.count(200), 1, statuses)
        winner = statuses.index(200)
        self.assertIn(statuses[1 - winner], (403, 409))
        self.assertFalse(s.api("GET", "session")[1]["signed_in"])
        self.assertEqual(s.api("POST", "login", {"password": replacements[1 - winner]})[0], 401)
        self.assertEqual(s.api("POST", "login", {"password": replacements[winner]})[0], 200)

    def test_upgrading_an_existing_router_keeps_mappings_and_rejects_old_tokens(self):
        s=self.s
        s.stop()
        with closing(sqlite3.connect(s.root/"state/cc-profile/router.sqlite3")) as db, db:
            db.execute("DROP TABLE settings")
            db.execute("ALTER TABLE configs DROP COLUMN priority")
            db.execute("DROP TABLE panel_password")
            db.execute("DROP INDEX config_identity")
            db.execute("ALTER TABLE configs DROP COLUMN birth_ns")
            db.execute("ALTER TABLE configs ADD COLUMN observer_stamp TEXT")
            db.execute("PRAGMA user_version=1")
            db.execute("INSERT INTO configs(tool,label,home,dev,ino) VALUES('claude','existing','/missing',1,1)")
            db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms) VALUES('claude','existing-mapping',1,'assigned','/work',1,1)")
        s.start()
        self.assertTrue(s.api("GET","session")[1]["setup_required"])
        _,data,headers=s.api("POST","setup",{"password":s.password})
        s.session=data["session"]
        self.assertEqual(s.api("GET","sessions")[1]["items"][0]["native_id"],"existing-mapping")

    def test_password_guessing_is_throttled(self):
        s=self.s
        status, data, _ = s.api("POST","setup",{"password":s.password})
        self.assertEqual(status, 200)
        s.session = data["session"]
        self.assertEqual(s.api("POST","password",{"current_password":"wrong","new_password":"1234"})[0],400)
        for _ in range(4):
            self.assertEqual(s.api("POST","login",{"password":"incorrect password"})[0],401)
        self.assertEqual(s.api("POST","login",{"password":s.password})[0],429)
        self.assertEqual(s.api("POST","password",{"current_password":s.password,"new_password":"1234"})[0],429)
