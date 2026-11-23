"""Optional two-profile cold restore with Garcon's actual ACP client and an offline AgY fixture."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_service as svc

GARCON = os.getenv("CCP_TEST_GARCON")


@unittest.skipUnless(GARCON and shutil.which("bun"), "set CCP_TEST_GARCON to a Garcon checkout with managed ACP auth")
class GarconACP(unittest.TestCase):
    def test_cold_restore_needs_no_launcher_flags(self):
        s = svc.Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(s.doCleanups)
        s.setUp()
        def profile(name):
            status, c, _ = s.api("POST","accounts",{"tool":"agy","label":name,"create":True})
            self.assertEqual(status,201,c)
            status, c, _ = s.api("PATCH",f"accounts/{c['id']}",{"enabled":True,"revision":c["revision"]})
            self.assertEqual(status,200,c)
            return c
        a, b = profile("A"), profile("B")
        link = s.root / "agy"
        link.symlink_to(svc.LAUNCHER)
        script = """
          const dir = process.env.GARCON_ACP;
          const {AcpClient} = await import(dir + '/client.ts');
          const {AcpTransport} = await import(dir + '/transport.ts');
          const spec = {command:process.env.FIXTURE_AGY,args:['acp'],cwd:process.cwd(),env:process.env};
          async function run(method, params) {
            const c = new AcpClient(new AcpTransport({requestTimeoutMs:4000}), {
              initialize:{protocolVersion:1,clientCapabilities:{},clientInfo:{name:'Garcon',version:'fixture'}},
              authenticateMethodId:'oauth-personal'});
            try {
              const {authenticatedMethodId} = await c.connect(spec);
              if (authenticatedMethodId !== null || !c.getAdvertisedCapabilities().sessionResume) throw Error('handshake');
              return await c[method]({cwd:process.cwd(), ...params});
            } finally { c.close(); }
          }
          const {sessionId} = await run('newSession',{fixtureId:'garcon-cold'});
          const loaded = await run('resumeSession',{sessionId});
          await run('newSession',{fixtureId:'next'});
          console.log(JSON.stringify(loaded));
        """
        r = subprocess.run([shutil.which("bun"), "-e", script], cwd=s.root, timeout=16,
                           env=dict(s.env, FIXTURE_AGY=str(link), GARCON_ACP=str(Path(GARCON).resolve() /
                                    "server-agents/common/src/acp")), text=True, capture_output=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)["home"], a["home"])
        self.assertEqual((Path(b["home"])/"calls").read_text().splitlines(), ["initialize","session/new"])
        self.assertEqual([x["config_id"] for x in s.api("GET","startups?tool=agy")[1]["items"]],
                         [b["id"], a["id"], a["id"]])
