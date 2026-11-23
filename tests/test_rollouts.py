import os
from pathlib import Path
import time
import unittest
import uuid

import test_service as svc


class CodexRollouts(unittest.TestCase):
    def test_worker_starts_for_verified_build_only_and_repairs_offline_saves(self):
        s = svc.Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(s.doCleanups)
        s.setUp()
        profile = s.account("codex", "rollout-fixture")
        home = Path(profile["home"])
        active = home / "sessions/2024/12/31"
        active.mkdir(parents=True)
        uid = str(uuid.uuid4())
        def filename(thread):
            return f"rollout-2024-12-31T23-59-59-{thread}_{uuid.uuid4()}.jsonl.zst"
        def rows():
            return s.api("GET", "sessions?tool=codex")[1]["items"]
        def wait(thread):
            until = time.monotonic() + 8
            while time.monotonic() < until:
                for row in rows():
                    if row["native_id"] == thread:
                        return row
                time.sleep(.05)
            self.fail("No saved-file discovery")
        def restart():
            s.start()
            s.session = s.api("POST", "login", {"password":s.password})[1]["session"]

        saved = active / filename(uid)
        saved.write_bytes(b"inert compressed fixture")
        row = wait(uid)
        self.assertEqual((row["config_id"], row["evidence"], row["workspace"], row["last_routed_ms"]),
                         (profile["id"], "saved_file", None, None))
        s.stop()
        archive = home / "archived_sessions"
        archive.mkdir()
        os.rename(saved, archive / saved.name)
        fresh = str(uuid.uuid4())
        (archive / filename(fresh)).write_bytes(b"inert")
        fake = s.root / "bin/codex"
        verified = fake.read_text()
        fake.write_text(verified.replace("codex-cli 0.171.0", "codex-cli 0.0.0"))
        restart()
        self.assertFalse(s.api("GET", "accounts?tool=codex")[1]["supported"])
        time.sleep(.4)
        self.assertEqual(rows(), [row])
        s.stop()
        fake.write_text(verified)
        restart()
        self.assertEqual(wait(fresh)["config_id"], profile["id"])
        self.assertIn(row, rows())
        self.assertEqual(s.api("GET", "startups?tool=codex")[1]["items"], [])
