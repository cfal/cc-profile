"""Run the shipped ccp executable against a disposable, same-UID socket."""
from http.server import BaseHTTPRequestHandler
import json
import os
from pathlib import Path
import pty
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import unittest

from test_service import LAUNCHER


@unittest.skipUnless(LAUNCHER.is_file(), "build ccp first")
class Launcher(unittest.TestCase):
    def setUp(self):
        temp=tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root=Path(temp.name)
        self.directory=self.root/"cc-profile"
        self.directory.mkdir(mode=0o700)
        self.native=self.root/"claude"
        self.native.write_text("#!"+sys.executable+"\n"+'''
import json,os,sys
print(json.dumps([sys.argv[1:],os.getenv("CLAUDE_CONFIG_DIR"),os.getenv("EDITOR"),os.getenv("CCP_OBSERVE_TICKET"),sys.stdin.read(),os.getpid()]))
sys.exit(17)
''')
        self.native.chmod(0o700)
        self.calls=[]
        self.respond=lambda path,r: (200,self.plan(r))
        case=self
        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                raw=self.rfile.read(int(self.headers["content-length"]))
                case.calls.append((self.path,raw))
                reply=case.respond(self.path,json.loads(raw))
                if reply is None: return
                status,data=reply
                body=json.dumps(data).encode()
                self.send_response(status)
                self.send_header("Content-Length",str(len(body)))
                self.end_headers()
                try: self.wfile.write(body)
                except BrokenPipeError: pass
            def log_message(self,*args): pass
        server=socketserver.ThreadingUnixStreamServer(str(self.directory/"ccpd.sock"),Handler)
        thread=threading.Thread(target=server.serve_forever,kwargs={"poll_interval":0.02},daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        self.env={"PATH":str(self.root),"HOME":str(self.root),"XDG_RUNTIME_DIR":str(self.root),
                  "EDITOR":"test-editor","CCP_OBSERVE_TICKET":"old-ticket","HTTPS_PROXY":"http://127.0.0.1:9"}

    def plan(self,r):
        return {"v":3,"request_id":r["request_id"],"tool":r["tool"],"label":"work","decision":"current","config_id":1,
                "argv":r["argv"],"env_set":{"CLAUDE_CONFIG_DIR":"/physical/home"},
                "env_unset":[]}

    def run_ccp(self,*args):
        return subprocess.run([str(LAUNCHER),*args],env=self.env,cwd=self.root,input="stdin with spaces\n",
                              capture_output=True,text=True,timeout=8)

    def test_same_exec_pid_argv_environment_stdin_and_exit_status(self):
        p=subprocess.Popen([str(LAUNCHER),"claude","--","a b",""],env=self.env,cwd=self.root,text=True,
                           stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        out,err=p.communicate("input\n",timeout=8)
        self.assertEqual(json.loads(out),[["--","a b",""],"/physical/home","test-editor",None,"input\n",p.pid])
        self.assertEqual(p.returncode,17)
        self.assertEqual(err,"")

    def test_route_banner_is_opt_in_on_stderr_only(self):
        for value in ("", "0", "1"):
            with self.subTest(value=value):
                self.env["CC_PROFILE_DEBUG"] = value
                p = self.run_ccp("claude", "test")
                self.assertEqual(p.returncode, 17)
                self.assertEqual(json.loads(p.stdout)[0], ["test"])
                self.assertEqual(p.stderr, "ccp: claude/work -> /physical/home\n" if value == "1" else "")

    def test_codex_is_the_native_process_for_chats_and_exact_resumes(self):
        exe = self.root/"codex"
        exe.write_text("#!" + sys.executable + "\n" + """
import json, os, sys
print(json.dumps([sys.argv[1:], os.getpid(), os.getcwd(), os.environ["CODEX_HOME"],
                  os.environ["HTTPS_PROXY"], sys.stdin.isatty(), None if sys.stdin.isatty() else sys.stdin.read()]))
print("native stderr", file=sys.stderr)
sys.exit(17)
""")
        exe.chmod(0o700)
        uid = "12345678-1234-4234-9234-123456789abc"
        for argv, tty, sticky in [
            (["-a", "never", "-m", "exec", "--image=pic.png", "--", "--last"], True, False),
            (["-m", "resume", "exec", "--sandbox=read-only", "-C", ".", "--json", "hello"], False, False),
            (["resume", uid, "--image=pic.png", "--", "text with spaces"], True, True),
            (["exec", "resume", uid, "--", "-"], False, True),
            (["exec", "--ephemeral", "--", "-"], False, False),
        ]:
            with self.subTest(argv=argv):
                self.calls.clear()
                home = str(self.root/"selected")
                self.respond = lambda path,r: (200, dict(self.plan(r), env_set={"CODEX_HOME":home},
                                                       decision="sticky" if sticky else "round_robin"))
                master, slave = pty.openpty()
                try:
                    expected = self.env | {"CODEX_HOME":home}
                    for cmd, env in [([exe], expected), ([LAUNCHER,"codex"], self.env)]:
                        with subprocess.Popen([*cmd,*argv], cwd=self.root, env=env, text=True,
                                              stdin=slave if tty else subprocess.PIPE,
                                              stdout=subprocess.PIPE,stderr=subprocess.PIPE) as child:
                            out, err = child.communicate(None if tty else "input\n", timeout=5)
                        self.assertEqual((child.returncode,err), (17,"native stderr\n"))
                        self.assertEqual(json.loads(out), [argv,child.pid,str(self.root),home,
                                         self.env["HTTPS_PROXY"],tty,None if tty else "input\n"])
                finally:
                    os.close(slave)
                    os.close(master)
                self.assertEqual([p for p,_ in self.calls], ["/v3/launch"])
                self.assertFalse(list(self.directory.glob("c-*")))

    def test_runtime_socket_does_not_require_a_home_directory(self):
        self.env.pop("HOME")
        p=self.run_ccp("claude")
        self.assertEqual(p.returncode,17,p.stderr)

    def test_managed_stdio_never_falls_back_to_legacy_admission(self):
        self.respond=lambda path,r: (404,{}) if path.startswith("/v4/") else (200,self.plan(r))
        for tool,verb,route,uid in [("agy","acp","agy-acp","known"),
                ("codex","app-server","codex-stdio","12345678-1234-4234-9234-123456789abc")]:
            (self.root/tool).symlink_to(self.native)
            for args,expected in [([], "check"),(["--for-session",uid],"launch")]:
                self.calls.clear()
                p=self.run_ccp(tool,verb,*args)
                self.assertNotEqual(p.returncode,0)
                self.assertEqual(p.stdout,"")
                self.assertEqual([path for path,_ in self.calls],[f"/v4/{route}/"+expected])

    def test_version_probe_environment_cannot_short_circuit_an_ordinary_launch(self):
        self.env["CCP_NATIVE_VERSION_PROBE"] = "1"
        p = self.run_ccp("claude", "hello")
        self.assertEqual(p.returncode, 17, p.stderr)
        self.assertEqual(json.loads(p.stdout)[0], ["hello"])

    def test_empty_path_does_not_pick_a_program_in_the_working_directory(self):
        for path in ["", ":/nonexistent:"]:
            with self.subTest(path=path):
                self.env["PATH"]=path
                p=self.run_ccp("claude")
                self.assertIsNone(json.loads(self.calls[-1][1])["binary"])
                self.assertEqual((p.returncode,p.stdout),(1,""))

    def test_launcher_links_dispatch_by_invoked_name_and_skip_aliases_on_path(self):
        install, symlinks, hardlinks = [self.root / d for d in ("install", "symlinks", "hardlinks")]
        install.mkdir()
        symlinks.mkdir()
        hardlinks.mkdir()
        shutil.copy2(LAUNCHER, install / "ccp")
        (symlinks / "claude").symlink_to(install / "ccp")
        os.link(install / "ccp", hardlinks / "claude")
        self.env["PATH"] = os.pathsep.join(map(str, [symlinks, hardlinks, self.root, install]))
        for exe, prefix in [(symlinks / "claude", []), (hardlinks / "claude", []),
                            (install / "ccp", ["claude"])]:
            for argv in [[], ["--version"], ["--help"], ["login", "with spaces", ""]]:
                with self.subTest(exe=exe, argv=argv):
                    p = subprocess.run([exe, *prefix, *argv], env=self.env, cwd=self.root,
                                       input="", capture_output=True, text=True, timeout=3)
                    self.assertEqual(p.returncode, 17, p.stderr)
                    self.assertEqual(json.loads(p.stdout)[0], argv)
                    request = json.loads(self.calls[-1][1])
                    self.assertEqual((request["tool"], request["binary"]), ("claude", str(self.native)))

    def test_launch_survives_replacement_of_the_launcher_binary(self):
        retired=self.root/"retired"
        shutil.copyfile(LAUNCHER,retired)
        retired.chmod(0o700)
        with retired.open("rb") as old:
            retired.unlink()
            p=subprocess.run([f"/proc/self/fd/{old.fileno()}","claude"],pass_fds=(old.fileno(),),
                             env=self.env,cwd=self.root,input="",capture_output=True,text=True,timeout=8)
        self.assertEqual(p.returncode,17,p.stderr)

    @unittest.skipIf(os.geteuid()==0,"root may execute files with only the other execute bit")
    def test_path_skips_a_file_not_executable_by_its_owner(self):
        first=self.root/"first"
        first.mkdir()
        decoy=first/"claude"
        decoy.touch(mode=0o700)
        decoy.chmod(0o001)
        self.env["PATH"]=os.pathsep.join([str(first),str(self.root)])
        p=self.run_ccp("claude")
        self.assertEqual(p.returncode,17,p.stderr)
        self.assertEqual(json.loads(self.calls[0][1])["binary"],str(self.native))

    def test_invalid_plans_never_execute(self):
        for bad in [{"env_unset":["PATH"]},{"env_set":{"CLAUDE_CONFIG_DIR":"relative"}},
                    {"env_set":{"CLAUDE_CONFIG_DIR":"/profile","UNEXPECTED_KEY":"bad"}},
                    {"env_set":{"CLAUDE_CONFIG_DIR":"/profile","CCP_OBSERVE_TICKET":"unissued"},"env_unset":[]},
                    {"env_unset":["CCP_LAUNCH_ID"]},{"argv":["a\0b"]},{"tool":"codex"},{"request_id":"other"},
                    {"argv":"not-an-array"},{"v":1}]:
            self.respond=lambda path,r: (200,dict(self.plan(r),**bad))
            with self.subTest(bad=bad):
                p=self.run_ccp("claude")
                self.assertEqual((p.returncode,p.stdout),(1,""))

    def test_print_plans_cannot_substitute_a_persistent_launch(self):
        for tool, key, argv, replaced in [
            ("claude", "CLAUDE_CONFIG_DIR", ["-p", "hi"], ["--session-id", "old", "-p", "hi"]),
            ("codex", "CODEX_HOME", ["exec", "--ephemeral", "hi"], ["exec", "hi"]),
            ("agy", "HOME", ["--print", "hi"], ["-i", "hi"]),
        ]:
            if tool != "claude":
                (self.root/tool).symlink_to(self.native)
            self.respond = lambda path, req: (200, dict(
                self.plan(req), decision="round_robin", argv=replaced, env_set={key: str(self.root)}))
            with self.subTest(tool=tool):
                p = self.run_ccp(tool, *argv)
                self.assertEqual((p.returncode, p.stdout), (1, ""))
                self.assertIn("Invalid launch plan", p.stderr)

    def test_claude_stream_plan_must_keep_the_id_persistence_and_decision(self):
        args = ["--session-id=93000000-0000-4000-8000-000000000001", "--input-format=stream-json",
                "-p", "--output-format=stream-json"]
        for changes in [{"argv": args[1:]}, {"argv": ["--no-session-persistence", *args]},
                        {"decision": "sticky"}]:
            self.respond = lambda path, req: (200, dict(self.plan(req), decision="round_robin") | changes)
            p = self.run_ccp("claude", *args)
            self.assertEqual((p.returncode, p.stdout), (1, ""))
            self.assertIn("Invalid launch plan", p.stderr)

    def test_denial_sends_override_names_not_values_and_does_not_retry(self):
        self.respond=lambda path,r:(409,{"message":"owner disabled"})
        for args, key in [(("claude","--resume","id"),"ANTHROPIC_API_KEY"), (("codex",),"CODEX_ACCESS_TOKEN"),
                          (("codex",),"CODEX_EXEC_SERVER_URL"), (("codex",),"CODEX_REFRESH_TOKEN_URL_OVERRIDE")]:
            with self.subTest(key=key):
                self.env[key]="private-override"
                self.calls.clear()
                p=self.run_ccp(*args)
                self.assertEqual((p.returncode,p.stdout,p.stderr),(1,"","ccp: owner disabled\n"))
                (path,raw),=self.calls
                self.assertEqual(path,"/v3/launch")
                self.assertEqual(json.loads(raw)["override_names"],[key])
                self.assertNotIn(b"private-override",raw)
                del self.env[key]

    def test_profile_login_never_falls_back_to_the_default_account_or_an_unrelated_command(self):
        for changes in [{"config_id": 8}, {"decision": "current"}, {"argv": ["chat"]}]:
            def respond(path, req):
                if path == "/v2/profile-tool":
                    self.assertEqual(req, 7)
                    return 200, "claude"
                self.assertEqual(req["login_profile"], 7)
                plan = dict(self.plan(req), config_id=7, decision="profile_login")
                return 200, plan | changes
            self.respond = respond
            p = self.run_ccp("login", "7")
            self.assertEqual((p.returncode, p.stdout), (1, ""))
            self.assertIn("Invalid launch plan", p.stderr)

    def test_transport_retry_is_byte_identical(self):
        self.respond=lambda path,r: None if len(self.calls)==1 else (200,self.plan(r))
        self.assertEqual(self.run_ccp("claude").returncode,17)
        self.assertEqual(len(self.calls),2)
        self.assertEqual(self.calls[0],self.calls[1])

    def test_agy_resume_with_options_rechecks_file_before_spawning(self):
        (self.root/"agy").symlink_to(self.native)
        def respond(path, req):
            if path == "/v2/exec-failure":
                return 200, {}
            return 200, dict(self.plan(req), decision="sticky", env_set={"HOME":str(self.root)})
        self.respond = respond
        p = self.run_ccp("agy","--sandbox","--conversation=60000000-0000-4000-8000-000000000002")
        self.assertEqual((p.returncode, p.stdout), (1, ""))
        self.assertIn("Saved conversation is no longer available", p.stderr)

    def test_codex_cannot_downgrade_or_accept_substituted_argv_or_hook_markers(self):
        (self.root/"codex").symlink_to(self.native)
        for changes in [{"v":2},{"argv":["--no-daemon"]},{"decision":"current"},
                        {"env_set":{"CODEX_HOME":"/profile","CCP_LAUNCH_ID":"unissued"}}]:
            def respond(path,req):
                plan=dict(self.plan(req),decision="round_robin",env_set={"CODEX_HOME":"/profile"})
                return 200,plan|changes
            self.respond=respond
            p=self.run_ccp("codex")
            self.assertEqual((p.returncode,p.stdout),(1,""))
            self.assertIn("Invalid launch plan",p.stderr)

    def test_slow_router_does_not_retry_or_extend_a_non_rotating_request(self):
        def slow(path,r):
            time.sleep(5.3)
            return None
        self.respond=slow
        p=self.run_ccp("claude", "--version")
        self.assertEqual((p.returncode,p.stdout),(1,""))
        self.assertIn("Timed out waiting",p.stderr)
        self.assertEqual(len(self.calls),1)

    def test_native_exec_failure_once_and_no_implicit_shell(self):
        for code in [126,127]:
            self.calls.clear()
            if code==126:
                self.native.write_text("echo implicit-shell\n")
            def respond(path,r):
                if path=="/v2/exec-failure": return 200,{}
                if code==127: self.native.unlink()
                return 200,self.plan(r)
            self.respond=respond
            p=self.run_ccp("claude")
            self.assertEqual((p.returncode,p.stdout),(code,""))
            self.assertEqual([path for path,_ in self.calls],["/v3/launch","/v2/exec-failure"])
            req,failure=[json.loads(raw) for _,raw in self.calls]
            for key in ["request_id","ticket"]: self.assertEqual(req[key],failure[key])

    def test_oversized_replies_and_requests_fail_closed(self):
        self.respond=lambda path,r:(200,{"padding":"x"*(512*1024)})
        self.assertIn("reply too large",self.run_ccp("claude").stderr)
        self.assertEqual(len(self.calls),1)
        self.assertIn("request too large",self.run_ccp("claude",*(["x"*64000]*5)).stderr)
        self.assertEqual(len(self.calls),1)

    def test_help_unsupported_commands_encoding_and_self_resolution_are_offline(self):
        self.assertIn("Usage: ccp",self.run_ccp("--help").stdout)
        self.assertEqual(self.run_ccp("run","claude").returncode,2)
        self.assertIn("UTF-8",self.run_ccp("claude",b"\xff").stderr)
        self.native.unlink()
        self.native.symlink_to(LAUNCHER)
        p = self.run_ccp("claude")
        self.assertEqual(p.returncode, 2)
        self.assertIn("not a native claude executable", p.stderr)
        self.assertFalse(self.calls)
