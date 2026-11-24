"""Opt-in Garcon/Codex 0.172.0 transport test. Run in the offline namespace with Bun and Garcon mounted."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_service as svc


@unittest.skipUnless(os.getenv("CCP_TEST_CODEX_OFFLINE") == "1" and os.getenv("CCP_TEST_GARCON")
                     and shutil.which("bun") and svc.DAEMON.is_file() and svc.LAUNCHER.is_file(),
                     "requires network-isolated Codex 0.172.0, CCP_TEST_GARCON, Bun and a built CCP pair")
class GarconCodex(unittest.TestCase):
    def test_native_start_cold_resume_and_fork(self):
        s = svc.Service("test_web_authenticates_reads_and_mutations")
        s.native = True
        self.addCleanup(s.doCleanups)
        s.setUp()
        version = subprocess.check_output(["codex", "--version"], cwd=s.root, env=s.env, text=True, timeout=5)
        self.assertEqual(version.strip(), "codex-cli 0.172.0")
        self.assertEqual(s.api("PUT", "settings", {"routing_mode":"round_robin"})[0], 200)
        a, b = [s.account("codex", name) for name in ("first", "second")]
        for profile, effort in [(a, "low"), (b, "medium")]:
            (Path(profile["home"])/"config.toml").write_text(f'model_reasoning_effort="{effort}"\n')
        alias = s.root/"codex"
        alias.symlink_to(svc.LAUNCHER)
        work = s.root/"project"
        work.mkdir()
        host_home = s.root/"host-codex"
        host_home.mkdir()
        env = dict(s.env, GARCON_CODEX_CLI=str(alias), CODEX_HOME=str(host_home),
                   CCP_TEST_GARCON=os.environ["CCP_TEST_GARCON"])
        p = subprocess.run([shutil.which("bun"), "--no-install", Path(__file__).with_name("garcon-codex.mjs"),
                            a["home"], b["home"]], cwd=work, env=env, text=True, capture_output=True, timeout=150)
        self.assertEqual(p.returncode, 0, p.stderr)
        result = json.loads(p.stdout)
        self.assertFalse((host_home/"sessions").exists())
        rows = {r["native_id"]:r for r in s.api("GET", "sessions?tool=codex")[1]["items"]}
        for key, owner in [("parent", a), ("child", a), ("next", b)]:
            row = rows[result[key]]
            self.assertEqual((row["evidence"], row["config_id"]), ("observed", owner["id"]), key)
        rows = s.api("GET", "startups?tool=codex")[1]["items"]
        self.assertEqual([(r["config_id"], r["reason"]) for r in reversed(rows)],
                         [(a["id"], "round_robin"), (a["id"], "sticky"), (a["id"], "sticky"),
                          (a["id"], "sticky"), (a["id"], "sticky"), (b["id"], "round_robin")])
