"""Opt-in smoke with installed providers, disposable homes, and no real credentials."""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from queue import Queue
import subprocess
import shutil
import sys
import threading
import time
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_service

class Native(unittest.TestCase):
    def setUp(self):
        self.s = test_service.Service("test_web_authenticates_reads_and_mutations")
        self.s.native = True
        self.addCleanup(self.s.doCleanups)
        self.s.setUp()

    def test_empty_profiles_report_signed_out(self):
        for tool in ("claude", "codex"):
            profile = self.s.account(tool, tool + "-empty")
            self.assertEqual(self.s.api("GET", f"accounts/{profile['id']}/identity")[:2],
                             (200, {"state":"signed_out", "email":None}), tool)
        self.assertEqual(self.s.api("GET", "startups")[1]["items"], [])

    def test_claude_delimiter_depends_on_print_mode(self):
        c = self.s.account("claude","delimiter")
        for verb in ("rc","remote-control"):
            args=["--permission-mode","default","--",verb]
            # This local validation error identifies dispatch without starting Remote Control.
            native=subprocess.run(["claude","--session-id",str(uuid.uuid4()),*args],
                                  env=dict(self.s.env,CLAUDE_CONFIG_DIR=c["home"]), cwd=self.s.root,
                                  stdin=subprocess.DEVNULL,capture_output=True,text=True,timeout=5)
            self.assertIn("before `remote-control` is not carried over",native.stderr)
            self.assertNotEqual(self.s.cli("claude",*args).returncode,0)
        self.assertFalse(self.s.api("GET","sessions")[1]["items"])
        result=self.s.cli("claude","-p","--output-format","json","--","rc")
        self.assertEqual(self.s.api("GET","sessions")[1]["items"],[])
        self.assertNotIn("Unsupported",result.stderr)
        self.assertEqual(json.loads(result.stdout)["type"], "result")

    def test_claude_assigns_exact_id_before_exec(self):
        c = self.s.account("claude","cc-home")
        self.s.cli("claude","Reply OK")
        row = self.s.api("GET","sessions?tool=claude")[1]["items"][0]
        uid = row["native_id"]
        self.assertEqual(row["evidence"],"assigned")
        self.assertTrue(list(Path(c["home"]).glob("projects/*/"+uid+".jsonl")))
        resumed = self.s.cli("claude","-p","--output-format","json","--resume",uid,"Continue")
        self.assertIn(uid,resumed.stdout,resumed.stderr)
        self.assertNotEqual(self.s.cli("claude","--resume",str(uuid.uuid4())).returncode,0)

    def offline_codex(self, home, models=None):
        cfg = home / "config.toml"
        # A closed port triggers Codex's offline reconnect loop. A terminal HTTP
        # rejection lets the real CLI finish without sending any model traffic upstream.
        class Reject(BaseHTTPRequestHandler):
            def do_POST(self):
                if models is not None and self.command == "POST":
                    request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    if "model" in request:
                        models.append(request["model"])
                body=b'{"error":{"message":"fixture rejection","type":"invalid_request_error"}}'
                self.send_response(400)
                self.send_header("Content-Type","application/json")
                self.send_header("Content-Length",str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            do_GET=do_POST
            def log_message(self,*args):
                pass
        endpoint=ThreadingHTTPServer(("127.0.0.1",0),Reject)
        threading.Thread(target=endpoint.serve_forever,daemon=True).start()
        self.addCleanup(endpoint.server_close)
        self.addCleanup(endpoint.shutdown)
        cfg.write_text('model_provider="offline"\nmodel="offline"\n[model_providers.offline]\n'
                       f'name="Local fixture"\nbase_url="http://127.0.0.1:{endpoint.server_port}/v1"\nwire_api="responses"\n'
                       'request_max_retries=0\nstream_max_retries=0\n')
        return cfg

    def test_default_codex_exec_and_stdio_agree_on_sticky_owner(self):
        c = self.s.account("codex","no-hooks")
        self.s.account("codex","other")
        home = Path(c["home"])
        models = []
        self.offline_codex(home, models)
        refused = self.s.cli("codex","exec","--json","--","--skip-git-repo-check")
        self.assertIn("Not inside a Git repository", refused.stderr)
        refused = self.s.cli("codex","exec","--skip-git-repo-check","--sandbox","read-only","Local fixture only")
        self.assertIn("incompatible", refused.stderr)
        self.assertFalse(self.s.api("GET","sessions")[1]["items"])
        work = self.s.root / "project"
        subprocess.run(["git","init","-q",str(work)],check=True)
        p = self.s.cli("codex","-m","fixture-model","-C",str(work),"exec","--json","Local fixture only")
        events = [json.loads(line) for line in p.stdout.splitlines()]
        uid = next((e["thread_id"] for e in events if e.get("type")=="thread.started"),None)
        self.assertIsNotNone(uid,p.stderr)
        self.assertNotEqual(p.returncode,0,"native fixture rejection must reach the caller")
        self.assertIn("fixture-model", models)
        rows = self.s.api("GET","sessions?native_id="+uid)[1]["items"]
        self.assertEqual([r["config_id"] for r in rows],[c["id"]],p.stderr)
        self.assertEqual(rows[0]["workspace"],str(work))
        resumed=self.s.cli("codex","exec","--skip-git-repo-check","--json","resume",uid,"Local fixture only")
        self.assertIn(uid,resumed.stdout,resumed.stderr)
        with subprocess.Popen([test_service.LAUNCHER,"codex","app-server","--for-session",uid],
                              env=self.s.env,cwd=self.s.root,stdin=subprocess.PIPE,stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE,text=True) as child:
            try:
                replies=Queue()
                def read_replies():
                    for line in child.stdout:
                        replies.put(json.loads(line))
                threading.Thread(target=read_replies,daemon=True).start()
                def call(num,method,params):
                    child.stdin.write(json.dumps({"id":num,"method":method,"params":params})+"\n")
                    child.stdin.flush()
                    while True:
                        msg=replies.get(timeout=8)
                        if msg.get("id")==num:
                            self.assertNotIn("error",msg)
                            return msg["result"]
                call(1,"initialize",{"clientInfo":{"name":"ccp-test","version":"1"}})
                child.stdin.write('{"method":"initialized"}\n')
                self.assertEqual(call(2,"thread/resume",{"threadId":uid})["thread"]["id"],uid)
                new=call(3,"thread/start",{})["thread"]["id"]
                rows=self.s.api("GET","sessions?native_id="+new)[1]["items"]
                self.assertEqual([r["config_id"] for r in rows],[c["id"]])
            finally:
                child.terminate()
                child.wait(timeout=8)
        self.assertFalse((home/"hooks.json").exists())
        self.assertFalse(list((self.s.root/"run/cc-profile").glob("c-*")))

    def test_codex_exec_outside_git_with_explicit_opt_out(self):
        c=self.s.account("codex","exec")
        self.offline_codex(Path(c["home"]))
        p=self.s.cli("codex","exec","--skip-git-repo-check","Local fixture only")
        self.assertIn("fixture rejection",p.stderr)
        self.assertNotEqual(p.returncode,0)
        rows=self.s.api("GET","sessions?tool=codex")[1]["items"]
        self.assertEqual([r["config_id"] for r in rows],[c["id"]])

    @unittest.skipUnless(shutil.which("hty"),"hty required for native terminal checks")
    def test_default_codex_tui_asks_for_trust_before_binding(self):
        c=self.s.account("codex","pty")
        cfg=self.offline_codex(Path(c["home"]))
        cfg.write_text(cfg.read_text().replace('model="offline"', 'model="gpt-6-sol"'))
        work=self.s.root/"project"
        subprocess.run(["git","init","-q",str(work)],check=True)
        def h(*args,check=True):
            p=subprocess.run(["hty",*args],capture_output=True,text=True,timeout=18)
            if check and p.returncode:
                target=args[2] if args[0]=="run" else args[1]
                screen=subprocess.run(["hty","snapshot",target],capture_output=True,text=True,timeout=4)
                self.fail(f"{args}: {p.stderr}\n{screen.stdout}")
            return p
        def screen_contains(text):
            until=time.monotonic()+12
            while time.monotonic()<until:
                screen=h("snapshot",name).stdout
                if text in screen:return
                time.sleep(.15)
            self.fail(f"Missing {text!r}: {screen}")
        def launch(*args):
            name="ccp-pty-"+uuid.uuid4().hex[:12]
            h("run","--name",name,"--cwd",str(work),"--cols","120","--rows","46","--",
              "/usr/bin/env","-i","TERM=xterm-256color",*[f"{k}={v}" for k,v in self.s.env.items()],
              str(test_service.LAUNCHER),"codex",*args)
            self.addCleanup(h,"delete",name,check=False)
            self.addCleanup(h,"kill",name,check=False)
            return name
        def submit(text):
            h("send",name,"--text",text)
            time.sleep(.4)
            h("send",name,"--key","enter")
        def close():
            h("send",name,"--key","ctrl-c")
            time.sleep(.2)
            h("send",name,"--key","ctrl-c",check=False)
            h("wait",name,"--exit","--timeout","6000")
            self.assertFalse(list((self.s.root/"run/cc-profile").glob("c-*")))
        name=launch()
        screen_contains("Trust this folder?")
        self.assertFalse(self.s.api("GET","sessions")[1]["items"])
        time.sleep(.25)
        h("send",name,"--key","enter")
        for _ in range(70):
            rows=self.s.api("GET","sessions")[1]["items"]
            if rows:break
            time.sleep(.15)
        self.assertEqual([r["config_id"] for r in rows],[c["id"]],h("snapshot",name).stdout)
        screen_contains("GPT-6-Sol")
        uid=rows[0]["native_id"]
        submit("/status")
        screen_contains(uid)
        submit("Local fixture only")
        screen_contains("fixture rejection")
        close()
        self.s.account("codex","other")
        name=launch("resume",uid)
        screen_contains("Local fixture only")
        submit("/status")
        screen_contains(uid)
        log=self.s.api("GET","startups?tool=codex")[1]["items"][0]
        self.assertEqual((log["reason"],log["config_id"]),("sticky",c["id"]))
        self.assertEqual(len(self.s.api("GET","sessions")[1]["items"]),1)
        close()

if __name__=="__main__":
    unittest.main()
