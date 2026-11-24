"""Oversized JSONL on a deferred app-server, using a disposable native fixture."""
import asyncio
import json
import subprocess
import sys
import unittest

import test_service as svc


LIMIT = 16 << 20
NATIVE = r'''
import json, os, sys
if sys.argv[1:] == ["--version"]:
    print("codex-cli 0.171.0")
    sys.exit()
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    ident = msg["id"]
    if "method" not in msg:
        assert ident == asked and msg["result"] == "approved"
        method = "fixture/large"
    else:
        method = msg["method"]
    if method == "initialize":
        result = dict(userAgent="fixture/0.171.0", codexHome=os.environ["CODEX_HOME"])
    elif method == "thread/list" and msg["params"].get("ask"):
        asked = ident
        print(json.dumps({"id":ident, "method":"fixture/approval", "params":{}}), flush=True)
        continue
    elif method in ("fixture/large", "fixture/stream", "thread/list") or msg.get("params", {}).get("large"):
        out = sys.stdout.buffer
        out.write(b'{ "id":' + json.dumps(ident).encode() + b', "result":{"data":[{"title":"NATIVE-LARGE:')
        for _ in range(257):
            out.write(b"x" * 65536)
        if method == "fixture/stream":
            out.flush()
            # No newline until the client sees part of this frame and unblocks us.
            assert json.loads(next(sys.stdin))["method"] == "fixture/unblock"
        out.write(b'"}]} }\r\n')
        out.flush()
        continue
    elif method == "thread/start":
        uid = "50000000-0000-4000-8000-000000000001"
        result = dict(thread=dict(id=uid,sessionId=uid,ephemeral=False,cwd=os.getcwd()))
    elif method == "fixture/wait":
        held = ident
        continue
    else:
        result = {"seen":method}
        if method == "fixture/release":
            print(json.dumps({"id":held, "result":"done"}), flush=True)
    print(json.dumps({"id":ident, "result":result}), flush=True)
'''

def call(ident, method, **params):
    return json.dumps({"id":ident, "method":method, "params":params}).encode() + b"\n"


@unittest.skipUnless(svc.DAEMON.is_file() and svc.LAUNCHER.is_file(), "build the CCP pair first")
class CodexFrames(unittest.TestCase):
    def setUp(self):
        class Service(svc.Service):
            def start(self):
                (self.root/"bin/codex").write_text("#!" + sys.executable + "\n" + NATIVE)
                super().start()
        self.s = Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(self.s.doCleanups)
        self.s.setUp()
        self.s.account("codex", "fixture")

    def launch(self, *frames):
        return subprocess.run([svc.LAUNCHER, "codex", "app-server"], env=self.s.env, cwd=self.s.root,
                              input=call("hello", "initialize") + b"".join(frames),
                              capture_output=True, timeout=20)

    def test_discarded_requests_keep_framing_and_pending_calls(self):
        padding = b"x" * LIMIT
        before = b'{"params":{"id":"decoy"},"id":"pre\\u002dbind","method":"thread/start","extra":"' + padding + b'"}\n'
        after = b'{"id":-7,"method":"thread/resume","params":{"history":"' + padding + b'"}}\n'
        tail = b'"id":3,"method":"thread/fork"}'
        # One byte over, with the newline already consumed: don't drain the next call.
        late = b'{"params":{"id":91},'.ljust(LIMIT - len(tail)) + tail + b"\n"
        p = self.launch(before, call(1, "thread/start"), call(2, "fixture/wait"), after, late,
                        call(4, "fixture/release"))
        self.assertEqual(p.returncode, 0, p.stderr)
        msgs = [json.loads(line) for line in p.stdout.splitlines()]
        self.assertCountEqual([m["id"] for m in msgs], ["hello", "pre-bind", 1, 2, -7, None, 4])
        replies = {m["id"]: m for m in msgs}
        for ident in ("pre-bind", -7, None):
            self.assertIn("16 MiB", replies[ident]["error"]["message"])
        self.assertEqual(replies[2]["result"], "done")
        self.assertEqual(replies[4]["result"], {"seen":"fixture/release"})
        self.assertEqual(len(self.s.api("GET", "startups")[1]["items"]), 1)

    def test_bound_ordinary_output_is_byte_exact_and_next_frame_survives(self):
        async def exchange():
            p = await asyncio.create_subprocess_exec(svc.LAUNCHER, "codex", "app-server",
                    env=self.s.env, cwd=self.s.root, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE)
            try:
                p.stdin.write(call(0, "initialize") + call(1, "thread/start") + call(2, "fixture/stream"))
                for ident in [0, 1]:
                    self.assertEqual(json.loads(await p.stdout.readline())["id"], ident)
                prefix = b'{ "id":2, "result":{"data":[{"title":"NATIVE-LARGE:'
                self.assertEqual(await p.stdout.readexactly(len(prefix)), prefix)
                p.stdin.write(b'{"method":"fixture/unblock"}\n')
                for _ in range(257):
                    self.assertTrue(await p.stdout.readexactly(65536) == b"x" * 65536)
                self.assertEqual(await p.stdout.readline(), b'"}]} }\r\n')
                p.stdin.write(call(2, "fixture/small"))  # Reuse the completed, streamed response ID.
                p.stdin.close()
                self.assertEqual(json.loads(await p.stdout.readline())["result"], {"seen":"fixture/small"})
                self.assertEqual(await p.wait(), 0)
            finally:
                if p.returncode is None:
                    p.kill()
                    await p.communicate()
        asyncio.run(asyncio.wait_for(exchange(), timeout=20))

    def test_duplicate_rejection_keeps_original_id_and_native_direction_separate(self):
        p = self.launch(call(1, "thread/start"), call(7, "thread/list", ask=True),
                        call(7, "fixture/duplicate"), b'{"id":7,"result":"approved"}\n')
        # Only receipt of the client approval causes the native oversized list reply.
        self.assertNotEqual(p.returncode, 0)
        self.assertIn(b"oversized Codex stdio frame", p.stderr)
        self.assertTrue(b"NATIVE-LARGE" not in p.stdout)
        with_id = [msg for line in p.stdout.splitlines() if (msg := json.loads(line))["id"] == 7]
        self.assertEqual(len(with_id), 2)
        self.assertTrue(any("already pending" in m.get("error", {}).get("message", "") for m in with_id))
        self.assertTrue(any(m.get("method") == "fixture/approval" for m in with_id))

    def test_oversized_pending_list_or_claim_never_leaks_its_prefix(self):
        for method in ("thread/start", "thread/list"):
            with self.subTest(method=method):
                p = self.launch(call(1, "thread/start"), call(2, method, large=True))
                self.assertNotEqual(p.returncode, 0)
                self.assertTrue(b"NATIVE-LARGE" not in p.stdout)
                self.assertEqual([json.loads(s)["id"] for s in p.stdout.splitlines()], ["hello", 1])
