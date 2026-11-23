"""Optional Linux 1.5.14 smoke: run only the native agent in an offline mount namespace."""
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

@unittest.skipUnless(AGY and shutil.which("bwrap"), "set CCP_TEST_AGY to the pinned Linux binary; bwrap required")
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
            (["--print", "--sandbox", "/clear", "--output-format=json"], b"", 2, "/clear is not available in print mode"),
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
                self.assertEqual(info["agentInfo"]["version"],"1.5.14")
                self.assertTrue(info["agentCapabilities"]["loadSession"])
                self.assertEqual(info["authMethods"],[])
                self.assertIn("list",info["agentCapabilities"]["sessionCapabilities"])
                p.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}\n')
                p.stdin.flush()
                self.assertEqual(s.api("GET","startups?tool=agy")[1]["items"],[])
                self.assertFalse((Path(c["home"])/".gemini/antigravity-cli/log").exists())
                # Native 1.5.14 refuses listing while signed out; an empty aggregate would be misleading.
                self.assertIn("No partial list",call("browse","session/list",{})["error"]["message"])
                self.assertEqual(s.api("GET","startups?tool=agy")[1]["items"],[])
                self.assertEqual(call(2,"session/new",{"cwd":str(s.root),"mcpServers":[]})["error"]["code"],-32000)
                p.stdin.close()
                self.assertEqual(p.wait(timeout=6),0)
            finally:
                if p.poll() is None: p.terminate();p.wait(timeout=5)
        root=Path(c["home"])/".gemini/antigravity-cli"
        self.assertTrue((root/"conversations").is_dir())
        self.assertTrue(any("Using file-based token storage because no D-Bus session bus detected" in log.read_text()
                            for log in (root/"log").glob("*.log")))
        self.assertEqual(s.api("GET","sessions?tool=agy")[1]["items"],[])
        self.assertFalse(list((s.root/"run/cc-profile").glob("agy-*")))

if __name__=="__main__":
    unittest.main()
