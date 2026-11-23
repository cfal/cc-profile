"""One-shot contract across the panel daemon, launcher and disposable native executables."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import select
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
if not sys.stdin.isatty():
    sys.stdout.buffer.write(sys.stdin.buffer.read())
print("native diagnostic",file=sys.stderr)
sys.exit(23)
'''


@unittest.skipUnless(svc.DAEMON.is_file() and svc.LAUNCHER.is_file(),"build ccp and ccpd first")
class OneShot(unittest.TestCase):
    def setUp(self):
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
        self.s, self.aliases = s, aliases

    def test_cli_and_executable_alias_preserve_pipes_and_rotate_without_sessions(self):
        s, aliases = self.s, self.aliases
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
                            ["--model", "fixture"]]
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

    def agy_profile(self, label):
        s = self.s
        code, c, _ = s.api("POST", "accounts", dict(tool="agy", label=label, create=True))
        self.assertEqual(code, 201, c)
        self.assertEqual(s.api("PATCH", f"accounts/{c['id']}", dict(enabled=True, revision=c["revision"]))[0], 200)
        return c

    def test_agy_finder_settings_are_one_native_operand(self):
        s = self.s
        profile = self.agy_profile("finder")
        finder = '{"permissions":{"allow":["read_file(*)"]}}'
        spaced = '{ "permissions": { "allow": ["read_file(src dir/*.rs)"] } }'
        schema = ["--json-schema", '{"type":"object"}']
        env = dict(s.env, CC_PROFILE_DEBUG="1")
        for exe, args, stdin, expected in [
            ([self.aliases/"agy"], ["--settings", finder, "-p", "find sources", *schema, "--output-format=json"],
             b"", ["--settings", finder, *schema, "--output-format=json", "--print=find sources"]),
            ([svc.LAUNCHER, "agy"], [*schema, "--print", "--settings=" + spaced], b"find sources\n",
             [*schema, "--settings=" + spaced]),
        ]:
            with self.subTest(exe=exe[0].name):
                p = subprocess.run([*exe, *args], env=env, cwd=s.root, input=stdin, capture_output=True, timeout=8)
                self.assertEqual(p.returncode, 23, p.stderr)
                header, forwarded = p.stdout.split(b"\n", 1)
                argv, home, cwd, _ = json.loads(header)
                self.assertEqual((argv, home, cwd, forwarded), (expected, profile["home"], str(s.root), stdin))
                self.assertNotIn(b"read_file", p.stderr)
        rows = s.api("GET", "startups?tool=agy")[1]["items"]
        self.assertEqual([(r["intent"], r["state"]) for r in rows], [("print", "routed")] * 2)
        self.assertNotIn("read_file", str(rows))

    def test_agy_settings_require_a_value_and_explicit_print(self):
        s = self.s
        self.agy_profile("settings")
        overlay = '--settings={"disableSlashCommands":true}'
        for args in [
            ["-p", "--settings"], ["--settings", "-p"], ["-p", "--settings", "--sandbox"],
            ["-p", "--settings", ""], ["--settings=", "-p"],
            [overlay], ["-i", "chat", overlay], ["acp", overlay], ["models", overlay],
            ["-p", "/usage", overlay],
        ]:
            p = s.cli("agy", *args)
            self.assertEqual((p.returncode, p.stdout), (1, ""))
            self.assertNotIn("disableSlashCommands", p.stderr)
        s.env["GEMINI_API_KEY"] = "inert-test-value"
        p = s.cli("agy", "-p", "hello", overlay)
        self.assertEqual((p.returncode, p.stdout), (1, ""))
        self.assertIn("GEMINI_API_KEY", p.stderr)

    def test_agy_input_modes_keep_bytes_and_never_select_tui_on_eof(self):
        s = self.s
        self.agy_profile("stdin")
        exe = [svc.LAUNCHER, "agy"]
        large = bytes(range(256)) * 8192
        for args, data, want in [
            (["-p", "--input-format", "text"], large, ["--input-format", "text"]),
            (["--input-format=text", "--print"], b"", ["--input-format=text", "--print="]),
            (["--print="], b"explicitly empty prompt", ["--print="]),
        ]:
            result = subprocess.run([*exe, *args], env=s.env, cwd=s.root, input=data,
                                    capture_output=True, timeout=8)
            self.assertEqual(result.returncode, 23, result.stderr)
            header, forwarded = result.stdout.split(b"\n", 1)
            self.assertEqual((json.loads(header)[0], forwarded), (want, data))

        master, slave = os.openpty()
        try:
            result = subprocess.run([*exe, "--print"], env=s.env, cwd=s.root, stdin=slave,
                                    capture_output=True, timeout=8)
            self.assertEqual(result.returncode, 23, result.stderr)
            self.assertEqual(json.loads(result.stdout)[0], ["--print="])
        finally:
            os.close(slave)
            os.close(master)

        bad_stdin = os.open(s.root, os.O_RDONLY | os.O_DIRECTORY)
        try:
            result = subprocess.run([*exe, "-p"], env=s.env, cwd=s.root, stdin=bad_stdin,
                                    capture_output=True, timeout=8)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, b"")
        finally:
            os.close(bad_stdin)

        for fmt in [["--input-format", "stream-json"], ["--input-format=stream-json"]]:
            args = [*fmt, "--output-format=stream-json"]
            with subprocess.Popen([*exe, *args, "-p"], env=s.env, cwd=s.root,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as p:
                try:
                    # A streaming client may wait for native output before sending a message.
                    self.assertTrue(select.select([p.stdout], [], [], 4)[0])
                    self.assertEqual(json.loads(p.stdout.readline())[0], [*args, "--print="])
                    data = b'{"type":"user","message":{"role":"user","content":"/clear"}}\n'
                    out, err = p.communicate(data, timeout=8)
                    self.assertEqual((p.returncode, out), (23, data), err)
                finally:
                    p.kill()
