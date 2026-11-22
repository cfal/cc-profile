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

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_service as svc

AGY = os.getenv("CCP_TEST_AGY") or shutil.which("agy")

@unittest.skipUnless(AGY and shutil.which("bwrap"), "set CCP_TEST_AGY to the pinned Linux binary; bwrap required")
class NativeACP(unittest.TestCase):
    def test_initialize_and_unauthenticated_new_use_isolated_file_storage(self):
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
