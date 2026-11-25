"""Optional Linux AgY smoke: run only the native agent in an offline mount namespace."""
import json
import os
from pathlib import Path
import select
import shlex
import shutil
import subprocess
import sys
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_service as svc

AGY = os.getenv("CCP_TEST_AGY") or shutil.which("agy")

@unittest.skipUnless(AGY and shutil.which("bwrap"), "set CCP_TEST_AGY to a native Linux binary; bwrap required")
class NativeACP(unittest.TestCase):
    def setUp(self):
        class Service(svc.Service):
            def start(self):
                # The agent sees only its disposable test filesystem, never the host bus or network.
                argv=["bwrap","--unshare-all","--die-with-parent","--ro-bind","/usr","/usr",
                      "--symlink","usr/bin","/bin","--symlink","usr/lib","/lib","--symlink","usr/lib64","/lib64",
                      "--dev","/dev","--proc","/proc","--tmpfs","/tmp","--tmpfs","/run",
                      "--bind",str(self.root),str(self.root),"--ro-bind",str(Path(AGY).resolve()),"/agy","/agy"]
                (self.root/"bin/agy").write_text("#!/bin/sh\nexec "+shlex.join(argv)+' "$@"\n')
                super().start()
        s=Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(s.doCleanups)
        s.setUp()
        _,c,_=s.api("POST","accounts",{"tool":"agy","label":"Native","create":True})
        s.api("PATCH",f"accounts/{c['id']}",{"revision":c["revision"],"enabled":True})
        s.env["DBUS_SESSION_BUS_ADDRESS"]="unix:path=/run/not-a-real-desktop-bus"
        self.service, self.profile = s, c

    def test_upstream_storage_flags_by_command(self):
        s = self.service
        # An upstream flag change is a reason to revisit Runtime::configure,
        # not a reason to block launches on a new version number.
        for command in ([], ["models"], ["acp"]):
            with self.subTest(command=command):
                p = subprocess.run([s.root/"bin/agy", *command, "--help"], env=s.env, cwd=s.root,
                                   capture_output=True, text=True, timeout=8)
                self.assertEqual(p.returncode, 0, p.stderr)
                help_text = p.stdout + p.stderr
                for flag in ("--gemini-dir", "--app-data-dir"):
                    self.assertEqual(flag in help_text, command == ["acp"], flag)

    def test_native_terminal_resume_aliases_select_saved_file_owner(self):
        s = self.service
        help_result = subprocess.run([s.root/"bin/agy","--help"], cwd=s.root, env=s.env,
                                     capture_output=True, text=True, check=True, timeout=8)
        if "--resume" not in help_result.stdout + help_result.stderr:
            self.skipTest("native CLI does not advertise the 1.5.16 resume aliases")
        _, owner, _ = s.api("POST","accounts",{"tool":"agy","label":"owner","create":True})
        s.api("PATCH", f"accounts/{owner['id']}", {"revision":owner["revision"],"enabled":True})
        home = Path(owner["home"])
        conversations = home/".gemini/antigravity-cli/conversations"
        for d in (conversations.parents[1], conversations.parent, conversations):
            d.mkdir(mode=0o700, exist_ok=True)
        sid = str(uuid.uuid4())
        # Valid metadata for routing, deliberately unreadable as a native conversation.
        (conversations/f"{sid}.pb").write_bytes(b"not a native conversation")
        for selector in ("--resume", "-r", "--conversation"):
            with self.subTest(selector=selector):
                p = subprocess.run([svc.LAUNCHER,"agy",selector,sid], cwd=s.root, env=s.env,
                                   stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                   start_new_session=True, timeout=8)
                self.assertIn(f'conversation "{sid}" not found', p.stderr)
                row = s.api("GET","startups?tool=agy")[1]["items"][0]
                self.assertEqual((row["reason"],row["config_id"],row["native_id"]), ("sticky",owner["id"],sid))
        self.assertFalse((Path(self.profile["home"])/".gemini/antigravity-cli/log").exists())

    def test_models_reaches_signed_out_native_cli(self):
        s, c = self.service, self.profile
        endpoint = f"accounts/{c['id']}/identity"
        self.assertEqual(s.api("GET", endpoint)[:2], (200, {"state":"no_saved_login","email":None}))
        self.assertEqual(s.api("POST", endpoint, {})[:2], (200, {"state":"sign_in_required","email":None}))
        self.assertEqual(s.api("GET", "startups")[1]["items"], [])
        self.assertEqual(s.api("PUT", "current/agy", {"config_id":c["id"]})[0], 200)
        p = s.cli("agy", "models")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("sign in", (p.stderr + p.stdout).lower())
        self.assertNotIn("ccp:", p.stderr)

    @unittest.skipUnless(shutil.which("hty"), "hty required for the native terminal UI")
    def test_bare_launch_opens_native_tui_without_creating_a_session(self):
        s = self.service
        name = "ccp-agy-" + uuid.uuid4().hex
        def hty(*args):
            return subprocess.run(["hty", *args], capture_output=True, text=True, timeout=18)
        self.addCleanup(hty, "delete", name)
        self.addCleanup(hty, "kill", name)
        alias = s.root / "aliases/agy"
        alias.parent.mkdir()
        alias.symlink_to(svc.LAUNCHER)
        s.env["PATH"] = str(alias.parent) + ":" + s.env["PATH"]
        p = hty("run", "--name", name, "--snapshot", "--wait-until-text", "Google OAuth", "--timeout", "10000",
                "--", "env", "-i", "-C", str(s.root), "TERM=xterm-256color",
                *(f"{k}={v}" for k,v in s.env.items()), os.getenv("CCP_TEST_AGY_ALIAS", str(alias)))
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertIn("Google OAuth", p.stdout)
        row, = s.api("GET","startups?tool=agy")[1]["items"]
        self.assertEqual((row["intent"], row["config_id"], row["native_id"]), ("tui", self.profile["id"], None))
        self.assertEqual(s.api("GET","sessions?tool=agy")[1]["items"], [])

    def test_mixed_print_options_and_piped_prompt_reach_native_print_mode(self):
        s = self.service
        for args, stdin, code, error in [
            (["--prompt", "--sandbox", "/clear", "--output-format=json"], b"", 2, "/clear is not available in print mode"),
            (["--output-format", "json", "-p"], b"/clear\n", 2, "/clear is not available in print mode"),
            (["--print", "--output-format=json"], b"", 1, "empty prompt"),
        ]:
            p = subprocess.run([svc.LAUNCHER, "agy", *args], input=stdin, capture_output=True,
                               env=s.env, cwd=s.root, timeout=8)
            # This command is refused before authentication or a model turn.
            self.assertEqual(p.returncode, code, p.stderr)
            result = json.loads(p.stdout)
            self.assertEqual((result["status"], result["num_turns"]), ("ERROR", 0))
            self.assertIn(error, result["error"])
        rows = s.api("GET", "startups?tool=agy")[1]["items"]
        self.assertEqual([(r["intent"], r["native_id"]) for r in rows], [("print", None)] * 3)
        self.assertEqual(s.api("GET", "sessions?tool=agy")[1]["items"], [])

    def test_print_settings_use_native_parser_without_saving(self):
        s = self.service
        finder = '{"permissions":{"allow":["read_file(*)"]}}'
        # This relative settings filename is also a subcommand, not a dispatch request.
        (s.root/"acp").write_text(finder)
        gemini = Path(self.profile["home"])/".gemini"
        app = gemini/"antigravity-cli"
        for directory in (gemini, app):
            directory.mkdir(mode=0o700, exist_ok=True)
        saved = app/"settings.json"
        # Native also reformats noncanonical saved JSON on launches without an overlay.
        original = b'{\n  "showTips": false\n}\n'
        saved.write_bytes(original)
        for args, stdin in [
            (["--settings", finder, "-p", "/clear", "--json-schema", '{"type":"object"}'], b""),
            (["--print", "--settings=" + finder], b"/clear\n"),
            (["--settings", "acp", "--print=/clear"], b""),
        ]:
            p = subprocess.run([svc.LAUNCHER, "agy", "--output-format=json", *args], input=stdin,
                               capture_output=True, env=s.env, cwd=s.root, timeout=8)
            self.assertEqual(p.returncode, 2, p.stderr)
            result = json.loads(p.stdout)
            self.assertEqual((result["status"], result["num_turns"]), ("ERROR", 0))
            self.assertIn("/clear is not available in print mode", result["error"])
        # A flag-like filename must remain attached; malformed content fails in AgY, not CCP.
        malformed = "{bad-fixture-json}"
        (s.root/"--sandbox").write_text(malformed)
        for option in (["--settings=--sandbox"], ["--settings", malformed]):
            p = s.cli("agy", "-p", "/clear", *option)
            self.assertEqual((p.returncode, p.stdout), (1, ""))
            self.assertIn("Error: --settings must be a JSON object", p.stderr)
            self.assertNotIn(malformed, p.stderr)
        self.assertEqual(saved.read_bytes(), original)

    def test_initialize_and_unauthenticated_new_use_isolated_file_storage(self):
        s, c = self.service, self.profile
        p=subprocess.Popen([svc.LAUNCHER,"agy","acp","--stdio","--disable-telemetry"],cwd=s.root,env=s.env,
                           stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        with p:
            def call(i, method, params):
                p.stdin.write(json.dumps({"jsonrpc":"2.0","id":i,"method":method,"params":params})+"\n")
                p.stdin.flush()
                while select.select([p.stdout],[],[],8)[0]:
                    line=p.stdout.readline()
                    if not line: break
                    msg=json.loads(line)
                    if msg.get("id")==i: return msg
                self.fail("Native ACP reply missing")
            try:
                # Keep stdin open until native replies (EOF can stop ACP immediately).
                info=call(1,"initialize",{"protocolVersion":1,"clientCapabilities":{}})["result"]
                self.assertEqual(info["agentInfo"]["version"],s.api("GET","accounts?tool=agy")[1]["version"])
                self.assertTrue(info["agentCapabilities"]["loadSession"])
                self.assertEqual(info["authMethods"],[])
                self.assertIn("list",info["agentCapabilities"]["sessionCapabilities"])
                p.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}\n')
                p.stdin.flush()
                self.assertEqual(s.api("GET","startups?tool=agy")[1]["items"],[])
                self.assertFalse((Path(c["home"])/".gemini/antigravity-cli/log").exists())
                self.assertIn("no saved login",call("browse","session/list",{})["error"]["message"])
                self.assertFalse((Path(c["home"])/".gemini/antigravity-cli/log").exists())
                self.assertEqual(s.api("GET","startups?tool=agy")[1]["items"],[])
                self.assertEqual(call(2,"session/new",{"cwd":str(s.root),"mcpServers":[]})["error"]["code"],-32000)
                p.stdin.close()
                self.assertEqual(p.wait(timeout=6),0)
            finally:
                if p.poll() is None: p.terminate();p.wait(timeout=5)
        root=Path(c["home"])/".gemini/antigravity-cli"
        self.assertTrue((root/"conversations").is_dir())
        logs = "\n".join(log.read_text() for log in (root/"log").glob("*.log"))
        self.assertIn("Using file-based token storage", logs)
        plugin_store = Path(c["home"])/".config/antigravity/plugin-secrets.json"
        for line in logs.splitlines():
            if "Using file-based secret storage at " in line:
                self.assertTrue(line.endswith(str(plugin_store)), line)
        self.assertFalse((s.root/".gemini").exists())
        self.assertFalse((s.root/".config/antigravity").exists())
        self.assertEqual(s.api("GET","sessions?tool=agy")[1]["items"],[])
        self.assertFalse(list(s.scratch.glob("agy-*")))

if __name__=="__main__":
    unittest.main()
