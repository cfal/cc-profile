"""Exercise directory selection across the daemon, launcher and native runtime."""
from pathlib import Path
import json
import os
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
        s.env.pop("CC_PROFILE_DATA_PATH")
        s.env.pop("XDG_RUNTIME_DIR")
        legacy = s.root/".local/state/cc-profile"
        legacy.mkdir(mode=0o700, parents=True)
        marker = legacy/"initialized"
        marker.write_bytes(b"legacy marker; database must not be opened or repaired")
        # The fixture also has a real, password-protected database in this XDG state root.
        s.env["XDG_STATE_HOME"] = str(s.root/"state")
        s.env["XDG_DATA_HOME"] = str(s.root/"unused-data")
        s.env.update(CC_PROFILE_PATH=str(legacy), CC_PROFILE_RUNTIME_DIR=str(s.root/"retired-runtime"))
        for base in (s.root/".config", s.root/"other-config"):
            s.stop()
            if base != s.root/".config":
                s.env["XDG_CONFIG_HOME"] = str(base)
            s.start()
            directory = base/"cc-profile"
            self.assertTrue(s.api("GET","session")[1]["setup_required"])
            self.assertTrue((directory/"router.sqlite3").is_file())
            self.assertTrue((directory/"runtime/ccpd.sock").is_socket())
            self.assertEqual(directory.stat().st_mode & 0o777, 0o700)
            s.session = s.api("POST", "setup", {"password": s.password})[1]["session"]
            for tool in ("claude", "codex", "agy"):
                status, c, _ = s.api("POST", "accounts", {"tool":tool, "label":"work", "create":True})
                self.assertEqual(status, 201, c)
                home = Path(c["home"])
                self.assertEqual(home.parent, directory/"accounts"/tool)
                self.assertEqual(home.stat().st_mode & 0o777, 0o700)
            s.env.pop("XDG_STATE_HOME", None)
        self.assertEqual(list(legacy.iterdir()), [marker])
        self.assertFalse((s.root/"unused-data").exists())
        self.assertFalse((s.root/"retired-runtime").exists())

    def test_exact_overrides_for_panel_launcher_and_agy_runtime(self):
        s = self.s
        db = Path(s.env["CC_PROFILE_DATA_PATH"])
        rt = s.root/"ipc"
        accounts, temp = s.root/"provider homes", s.root/"scratch base"
        s.stop()
        s.env.update(CC_PROFILE_RUNTIME_PATH=str(rt), CC_PROFILE_ACCOUNTS_PATH=str(accounts),
                     CC_PROFILE_TEMP_PATH=str(temp), XDG_CONFIG_HOME=str(s.root/"unused config"))
        rt.mkdir(mode=0o700)
        old_socket = rt/"launch.sock"
        old_socket.write_bytes(b"leave alone")
        s.start()
        s.session = s.api("POST", "login", {"password":s.password})[1]["session"]
        self.assertTrue((db/"service.lock").is_file())
        self.assertTrue((rt/"listener.lock").is_file())
        self.assertTrue((rt/"ccpd.sock").is_socket())
        self.assertEqual(old_socket.read_bytes(), b"leave alone")
        self.assertEqual(rt.stat().st_mode & 0o777, 0o700)
        self.assertFalse((s.root/"unused config/cc-profile").exists())
        _, profile, _ = s.api("POST","accounts",{"tool":"agy","label":"runtime","create":True})
        home = Path(profile["home"])
        self.assertEqual(home.parent, accounts/"agy")
        self.assertFalse((db/"accounts").exists())
        self.assertEqual(s.api("PATCH",f"accounts/{profile['id']}",{"revision":profile["revision"],"enabled":True})[0],200)
        p = s.cli("agy")
        self.assertEqual(p.returncode, 0, p.stderr)
        private = Path(json.loads((home/"last-run").read_text())["runtime"])
        self.assertEqual(private.parent, temp/f"cc-profile-{os.getuid()}")
        self.assertEqual(private.parent.stat().st_mode & 0o777, 0o700)
        self.assertFalse(private.exists())
        self.assertFalse((rt/"codex-router-home").exists())
        s.env.pop("CC_PROFILE_RUNTIME_PATH")
        self.assertNotEqual(s.cli("agy").returncode, 0)

    def test_existing_profile_in_old_location_is_not_moved_or_rediscovered(self):
        s = self.s
        old = s.root/".local/share/cc-profile/profiles/codex/previous"
        old.mkdir(mode=0o700, parents=True)
        existing = s.api("POST", "accounts", {"tool":"codex", "label":"old", "home":str(old)})[1]
        s.stop()
        s.start()
        s.session = s.api("POST", "login", {"password":s.password})[1]["session"]
        item, = s.api("GET", "accounts?tool=codex")[1]["items"]
        self.assertEqual((item["id"], item["home"], item["present"]), (existing["id"], str(old), True))
        self.assertFalse((Path(s.env["CC_PROFILE_DATA_PATH"])/"accounts").exists())

    def test_router_subdirectories_are_not_general_purpose_provider_homes(self):
        s = self.s
        root = Path(s.env["CC_PROFILE_DATA_PATH"])
        homes = root/"accounts"/"claude"
        for path in [root, root/"accounts", homes, root/"private", homes/"unused"/"nested",
                     root/"accounts"/"codex"/"foreign", Path(s.env["XDG_RUNTIME_DIR"])/"cc-profile"/"scratch",
                     s.scratch/"not-an-account"]:
            path.mkdir(mode=0o700, parents=True, exist_ok=True)
            status, _, _ = s.api("POST", "accounts", {"tool":"claude", "label":"unsafe", "home":str(path)})
            self.assertEqual(status, 400, path)
        outside = s.root/"external"
        outside.mkdir()
        (root/"accounts"/"agy").symlink_to(outside, target_is_directory=True)
        self.assertEqual(s.api("POST", "accounts", {"tool":"agy", "label":"redirect", "create":True})[0], 400)
        self.assertEqual(list(outside.iterdir()), [])

    def test_invalid_overrides_do_not_select_a_different_store_or_socket(self):
        s = self.s
        for name in ("CC_PROFILE_DATA_PATH", "CC_PROFILE_RUNTIME_PATH", "CC_PROFILE_ACCOUNTS_PATH", "CC_PROFILE_TEMP_PATH"):
            for value in ("", "relative", str(s.root/".."/"bad")):
                with self.subTest(name=name, value=value):
                    env = dict(s.env, CC_PROFILE_DATA_PATH=str(s.root/"other-store"),
                               CC_PROFILE_RUNTIME_PATH=str(s.root/"other-ipc"))
                    env[name] = value
                    p = subprocess.run([svc.DAEMON, "--port", "0"], cwd=s.root,
                                       env=env, capture_output=True,
                                       text=True, timeout=5)
                    self.assertNotEqual(p.returncode, 0)
                    self.assertIn(name, p.stderr)
        s.env["CC_PROFILE_RUNTIME_PATH"] = "relative"
        self.assertIn("CC_PROFILE_RUNTIME_PATH", s.cli("agy").stderr)
        exposed = s.root/"shared"
        exposed.mkdir()
        exposed.chmod(0o755)
        s.env["CC_PROFILE_RUNTIME_PATH"] = str(exposed)
        self.assertIn("Unsafe router socket directory", s.cli("agy").stderr)
        p = subprocess.run([svc.DAEMON, "--port", "0"], cwd=s.root, env=s.env,
                           capture_output=True, text=True, timeout=5)
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("private user-owned directory", p.stderr)
