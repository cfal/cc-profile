import io
import os
import unittest
from contextlib import redirect_stderr
from unittest.mock import patch
import cc_profile as c

class Launcher(unittest.TestCase):
    def setUp(self):
        self.req = {"request_id": "x", "tool": "claude", "ticket": "t"}
        self.plan = {"v": 1, "request_id": "x", "tool": "claude", "label": "work", "argv": ["--", "a b", ""],
                     "env_set": {"CLAUDE_CONFIG_DIR": "/physical/home"}, "env_unset": list(c.MARKERS)}

    def test_allowlist_applies_to_additions_and_removals(self):
        for key, bad in [("env_unset", ["PATH"]), ("env_set", {"LD_PRELOAD": "bad"}),
                         ("env_set", {"CODEX_HOME": "/other"}), ("tool", "codex"), ("argv", "shell")]:
            with self.subTest(key=key), self.assertRaises(c.Refusal):
                c.validate(dict(self.plan, **{key: bad}), self.req)

    def test_args_and_unrelated_environment_survive(self):
        with patch.dict(os.environ, {"CCP_OBSERVE_TICKET": "old", "EDITOR": "vi"}, clear=True):
            args, env, home = c.validate(self.plan, self.req)
        self.assertEqual((args, home), (["--", "a b", ""], "/physical/home"))
        self.assertEqual(env, {"EDITOR": "vi", "CLAUDE_CONFIG_DIR": home})

    def test_denial_does_not_exec_and_request_contains_only_override_names(self):
        with patch.dict(os.environ, {"ANTHROPIC_API_KEY": "never-on-wire"}), patch.object(c, "post", side_effect=c.Refusal("disabled")) as p, patch("os.execve") as execute, redirect_stderr(io.StringIO()):
            self.assertEqual(c.main(["claude", "--resume", "id"]), 1)
        execute.assert_not_called()
        request = p.call_args[0][1]
        self.assertIn("ANTHROPIC_API_KEY", request["override_names"])
        self.assertNotIn("never-on-wire", str(request))

    def test_exec_failure_reports_same_ticket_without_allocating_again(self):
        calls=[]
        def post(route,request,**kwargs):
            calls.append((route,request))
            return dict(self.plan,request_id=request["request_id"]) if route=="launch" else {}
        with patch.object(c,"post",side_effect=post), patch("shutil.which",return_value="/native/claude"), patch("os.execve",side_effect=PermissionError(13,"Denied")), redirect_stderr(io.StringIO()):
            self.assertEqual(c.main(["claude","-p","hello"]),126)
        self.assertEqual([r for r,_ in calls],["launch","exec-failure"])
        for field in ("request_id","ticket"):
            self.assertEqual(calls[0][1][field],calls[1][1][field])
