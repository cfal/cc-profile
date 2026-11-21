"""Opt-in smoke with installed providers, disposable homes, and no real credentials."""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
import sys
import threading
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
        session=self.s.api("GET","sessions")[1]["items"][0]
        self.assertEqual(json.loads(result.stdout)["session_id"],session["native_id"])

    def test_claude_assigns_exact_id_before_exec(self):
        c = self.s.account("claude","cc-home")
        self.s.cli("claude","-p","--output-format","json","Reply OK")
        row = self.s.api("GET","sessions?tool=claude")[1]["items"][0]
        uid = row["native_id"]
        self.assertEqual(row["evidence"],"assigned")
        self.assertTrue(list(Path(c["home"]).glob("projects/*/"+uid+".jsonl")))
        resumed = self.s.cli("claude","-p","--output-format","json","--resume",uid,"Continue")
        self.assertIn(uid,resumed.stdout,resumed.stderr)
        self.assertNotEqual(self.s.cli("claude","--resume",str(uuid.uuid4())).returncode,0)

    def test_codex_reports_native_thread_with_a_local_rejecting_provider(self):
        c = self.s.account("codex","cx-home")
        home = Path(c["home"])
        cfg = home / "config.toml"
        # A closed port triggers Codex's offline reconnect loop. A terminal HTTP
        # rejection lets the real CLI finish without sending any model traffic upstream.
        class Reject(BaseHTTPRequestHandler):
            def do_POST(self):
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
        route = "accounts/%s/observer/" % c["id"]
        data = {"revision":c["revision"]}
        self.assertEqual(self.s.api("POST",route+"install",data)[0],200)
        self.assertEqual(self.s.api("POST",route+"check",data)[0],409)
        # Simulate the operator approving this exact generated hook in this disposable home.
        env = dict(self.s.env, CODEX_HOME=str(home))
        lines = [{"id":1,"method":"initialize","params":{"clientInfo":{"name":"ccp-test","version":"1"},"capabilities":{"experimentalApi":True}}},
                 {"method":"initialized"},{"id":2,"method":"hooks/list","params":{"cwds":[str(self.s.root)]}}]
        with subprocess.Popen(["codex","app-server","--listen","stdio://"],env=env,cwd=self.s.root,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True) as p:
            try:
                for msg in lines:
                    p.stdin.write(json.dumps(msg)+"\n")
                    p.stdin.flush()
                    if msg.get("id"):
                        for line in p.stdout:
                            reply=json.loads(line)
                            if reply.get("id")==msg["id"]: break
            finally:
                p.terminate()
        hooks=reply["result"]["data"][0]["hooks"]
        hook,= [h for h in hooks if h.get("command","").endswith(" observe") and h["sourcePath"] == str(home/"hooks.json")]
        with cfg.open("a") as f:
            f.write("\n[hooks.state."+json.dumps(hook["key"])+"]\ntrusted_hash="+json.dumps(hook["currentHash"])+"\n")
        status,reply,_=self.s.api("POST",route+"check",data)
        self.assertEqual(status,200,reply)
        with cfg.open("a") as f:
            f.write("\n# A native settings edit must not require manual reapproval.\n")
        p=self.s.cli("codex","exec","--skip-git-repo-check","--json","Reply OK")
        events=[json.loads(line) for line in p.stdout.splitlines() if line.startswith("{")]
        actual=next((e["thread_id"] for e in events if e.get("type")=="thread.started"),None)
        self.assertIsNotNone(actual,p.stderr)
        mapped=self.s.api("GET","sessions?native_id="+actual)[1]["items"]
        self.assertEqual([m["config_id"] for m in mapped],[c["id"]],p.stderr)
        row=self.s.api("GET","startups?tool=codex")[1]["items"][0]
        self.assertEqual(row["state"],"observed")
        p=self.s.cli("codex","exec","--skip-git-repo-check","--json","resume",actual,"Continue")
        self.assertIn(actual,p.stdout,p.stderr)

if __name__=="__main__":
    unittest.main()
