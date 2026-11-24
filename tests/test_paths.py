"""Exercise directory selection across the daemon, launcher and native runtime."""
from pathlib import Path
import json
import subprocess
import unittest

import test_service as svc


@unittest.skipUnless(svc.DAEMON.is_file(), "build ccpd first")
class Paths(unittest.TestCase):
    def setUp(self):
        self.s = svc.Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(self.s.doCleanups)
        self.s.setUp()

    def test_fresh_default_and_xdg_config_home(self):
        s = self.s
        s.env.pop("CC_PROFILE_PATH")
        s.env.pop("XDG_RUNTIME_DIR")
        legacy = s.root/".local/state/cc-profile"
        legacy.mkdir(mode=0o700, parents=True)
        marker = legacy/"initialized"
        marker.write_bytes(b"legacy marker; database must not be opened or repaired")
        # The fixture also has a real, password-protected database in this XDG state root.
        s.env["XDG_STATE_HOME"] = str(s.root/"state")
        for base in (s.root/".config", s.root/"other-config"):
            s.stop()
            if base != s.root/".config":
                s.env["XDG_CONFIG_HOME"] = str(base)
            s.start()
            directory = base/"cc-profile"
            self.assertTrue(s.api("GET","session")[1]["setup_required"])
            self.assertTrue((directory/"router.sqlite3").is_file())
            self.assertTrue((directory/"runtime/launch.sock").is_socket())
            self.assertEqual(directory.stat().st_mode & 0o777, 0o700)
            s.env.pop("XDG_STATE_HOME", None)
        self.assertEqual(list(legacy.iterdir()), [marker])

    def test_exact_overrides_for_panel_launcher_and_agy_runtime(self):
        s = self.s
        db = Path(s.env["CC_PROFILE_PATH"])
        rt = s.root/"ipc"
        s.stop()
        s.env.update(CC_PROFILE_RUNTIME_DIR=str(rt), XDG_CONFIG_HOME=str(s.root/"unused config"))
        s.start()
        s.session = s.api("POST", "login", {"password":s.password})[1]["session"]
        self.assertTrue((db/"service.lock").is_file())
        self.assertTrue((rt/"listener.lock").is_file())
        self.assertTrue((rt/"launch.sock").is_socket())
        self.assertEqual(rt.stat().st_mode & 0o777, 0o700)
        self.assertFalse((s.root/"unused config/cc-profile").exists())
        _, profile, _ = s.api("POST","accounts",{"tool":"agy","label":"runtime","create":True})
        home = Path(profile["home"])
        self.assertEqual(home.parent, s.root/".local/share/cc-profile/profiles/agy")
        self.assertEqual(s.api("PATCH",f"accounts/{profile['id']}",{"revision":profile["revision"],"enabled":True})[0],200)
        p = s.cli("agy")
        self.assertEqual(p.returncode, 0, p.stderr)
        private = Path(json.loads((home/"last-run").read_text())["runtime"])
        self.assertEqual(private.parent, rt)
        self.assertFalse(private.exists())
        s.env.pop("CC_PROFILE_RUNTIME_DIR")
        self.assertNotEqual(s.cli("agy").returncode, 0)

    def test_invalid_overrides_do_not_select_a_different_store_or_socket(self):
        s = self.s
        for name in ("CC_PROFILE_PATH", "CC_PROFILE_RUNTIME_DIR"):
            for value in ("", "relative"):
                with self.subTest(name=name, value=value):
                    p = subprocess.run([svc.DAEMON, "--port", "0"], cwd=s.root,
                                       env=dict(s.env, **{name:value}), capture_output=True,
                                       text=True, timeout=5)
                    self.assertNotEqual(p.returncode, 0)
                    self.assertIn(name, p.stderr)
        s.env["CC_PROFILE_RUNTIME_DIR"] = "relative"
        self.assertIn("CC_PROFILE_RUNTIME_DIR", s.cli("agy").stderr)
        exposed = s.root/"shared"
        exposed.mkdir()
        exposed.chmod(0o755)
        s.env["CC_PROFILE_RUNTIME_DIR"] = str(exposed)
        self.assertIn("Unsafe router socket directory", s.cli("agy").stderr)
        p = subprocess.run([svc.DAEMON, "--port", "0"], cwd=s.root, env=s.env,
                           capture_output=True, text=True, timeout=5)
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("private user-owned directory", p.stderr)
