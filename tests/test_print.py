"""One-shot contract across the panel daemon, launcher and disposable native executables."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import subprocess
import sys
import time
import unittest

import test_service as svc


NATIVE = r'''
import json, os, pathlib, sys
tool = pathlib.Path(sys.argv[0]).name
if sys.argv[1:] == ["--version"]:
    print({"claude":"2.1.329 (Claude Code)", "codex":"codex-cli 0.171.0", "agy":"1.5.14"}[tool])
    sys.exit()
key = {"claude":"CLAUDE_CONFIG_DIR", "codex":"CODEX_HOME", "agy":"HOME"}[tool]
if tool == "agy":
    assert os.getenv("AGY_ACP_FORCE_FILE_STORAGE")=="1"
    assert not os.getenv("DBUS_SESSION_BUS_ADDRESS")
    runtime=pathlib.Path(os.environ["XDG_RUNTIME_DIR"])
    assert runtime.is_dir() and not (runtime/"bus").exists()
    if sys.argv[1:] == ["--print=fixture-save-history"]:
        directory = pathlib.Path(os.environ[key])
        for part in (".gemini", "antigravity-cli", "conversations"):
            directory = directory / part
            directory.mkdir(mode=0o700, exist_ok=True)
        (directory/"90000000-0000-4000-8000-000000000001.db").write_bytes(b"fixture")
print(json.dumps([sys.argv[1:],os.environ[key],os.getcwd(),os.getenv("HTTPS_PROXY")]),flush=True)
sys.stdout.buffer.write(sys.stdin.buffer.read())
print("native diagnostic",file=sys.stderr)
sys.exit(23)
'''


@unittest.skipUnless(svc.DAEMON.is_file() and svc.LAUNCHER.is_file(),"build ccp and ccpd first")
class OneShot(unittest.TestCase):
    def test_cli_and_executable_alias_preserve_pipes_and_rotate_without_sessions(self):
        s = svc.Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(s.doCleanups)
        s.setUp()
        s.stop()
        aliases = s.root / "aliases"
        aliases.mkdir()
        for tool in ("claude", "codex", "agy"):
            (s.root / "bin" / tool).write_text("#!" + sys.executable + "\n" + NATIVE)
            (aliases / tool).symlink_to(svc.LAUNCHER)
        s.start()
        s.session = s.api("POST", "login", {"password": s.password})[1]["session"]
        env = dict(s.env, PATH=str(aliases) + os.pathsep + s.env["PATH"],
                   DBUS_SESSION_BUS_ADDRESS="unix:path=/foreign")
        prompt = "private multiword prompt"
        payload = b"original stdin\x00\xff\n"
        cases = {
            "claude": [["--model=opus", "-p", prompt, "--permission-mode", "plan"],
                       ["--output-format", "text", "--print"]],
            "codex": [["-m", "fixture", "exec", prompt, "--ephemeral", "--sandbox=read-only"],
                      ["e", "--json", "--ephemeral"]],
            "agy": [["--output-format=json", "--print", "--sandbox", prompt],
                    ["-p", "--model", "fixture"]],
        }
        for tool, requests in cases.items():
            expected = requests
            if tool == "claude":
                expected = [["--no-session-persistence", *args] for args in requests]
            elif tool == "agy":
                expected = [["--output-format=json", "--sandbox", "--print=" + prompt],
                            ["--model", "fixture", "--print="]]
            profiles = {}
            for label in ("one", "two"):
                proxy = f"http://{label}.invalid:8080"
                status, c, _ = s.api("POST", "accounts", dict(tool=tool, label=label, create=True,
                                                             proxy_mode="custom", proxy_url=proxy))
                self.assertEqual(status, 201, c)
                edit = dict(enabled=True, revision=c["revision"])
                self.assertEqual(s.api("PATCH", f"accounts/{c['id']}", edit)[0], 200)
                profiles[c["home"]] = proxy + "/"

            def launch(index):
                prefix = [svc.LAUNCHER, tool] if index == 0 else [aliases/tool]
                p = subprocess.run([*prefix, *requests[index]], cwd=s.root, env=env,
                                   input=payload, capture_output=True, timeout=8)
                self.assertEqual(p.returncode, 23, p.stderr)
                line, stdout = p.stdout.split(b"\n", 1)
                argv, home, cwd, proxy = json.loads(line)
                self.assertEqual((argv, cwd, proxy, stdout), (expected[index], str(s.root), profiles[home], payload))
                self.assertIn(b"native diagnostic\n", p.stderr)
                return home

            with ThreadPoolExecutor(max_workers=2) as pool:
                homes = set(pool.map(launch, range(2)))
            self.assertEqual(homes, set(profiles))
            rows = s.api("GET", f"startups?tool={tool}")[1]["items"]
            self.assertEqual([(x["intent"], x["state"], x["native_id"]) for x in rows], [("print", "routed", None)] * 2)
            self.assertNotIn(prompt, str(rows))
            self.assertEqual(s.api("GET", f"sessions?tool={tool}")[1]["items"], [])

        # A native print can leave a file. Discover its profile without assigning it to the launch.
        p = subprocess.run([aliases/"agy", "-p", "fixture-save-history"], cwd=s.root, env=env,
                           input=b"", capture_output=True, timeout=8)
        self.assertEqual(p.returncode, 23, p.stderr)
        launch = s.api("GET", "startups?tool=agy")[1]["items"][0]
        self.assertEqual((launch["intent"], launch["native_id"]), ("print", None))
        deadline = time.monotonic() + 6
        while not (rows := s.api("GET", "sessions?tool=agy")[1]["items"]):
            self.assertLess(time.monotonic(), deadline, "Saved file was not discovered")
            time.sleep(.05)
        row, = rows
        self.assertEqual((row["config_id"], row["native_id"], row["evidence"], row["workspace"]),
                         (launch["config_id"], "90000000-0000-4000-8000-000000000001", "saved_file", None))
        self.assertEqual(s.api("GET", "startups?tool=agy")[1]["items"][0], launch)
