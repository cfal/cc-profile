#!/usr/bin/env python3
"""The local cc-profile launch client. Routing and configuration live in ccpd."""
import errno
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import stat
import struct
import sys
import time
import uuid

HELP = """Usage: ccp <claude|codex> [native arguments...]

  ccp claude --model opus
  ccp codex exec --json 'review these changes'
  ccp claude --resume <session-uuid>
  ccp codex resume <session-uuid>

New chats rotate; exact resumes use their original config. No transfers,
pickers or --last. Manage accounts and Current in the ccpd web panel
(default http://127.0.0.1:8743)."""
PROXY = {"HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"}
PROXY |= {k.lower() for k in PROXY}
MARKERS = {"CCP_LAUNCH_ID", "CCP_OBSERVE_TICKET"}
OVERRIDES = {
    "claude": {"ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL",
               "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN_FD", "CLAUDE_CODE_SIMPLE",
               "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY",
               "CLAUDE_CODE_USE_GATEWAY", "CLAUDE_CODE_SESSION_ACCESS_TOKEN"},
    "codex": {"OPENAI_API_KEY", "CODEX_API_KEY", "OPENAI_BASE_URL", "CODEX_SQLITE_HOME",
              "CODEX_MANAGED_CONFIG_PATH", "CODEX_CHATGPT_BASE_URL"},
}
class Refusal(Exception):
    pass

def runtime_dir():
    base = os.environ.get("XDG_RUNTIME_DIR", "")
    if os.path.isabs(base):
        return Path(base) / "cc-profile"
    state = os.environ.get("XDG_STATE_HOME", "")
    return (Path(state) if os.path.isabs(state) else Path.home() / ".local/state") / "cc-profile/runtime"

class LocalHTTP(http.client.HTTPConnection):
    def connect(self):
        directory = runtime_dir()
        meta = directory.lstat()
        if (directory.resolve() != directory or not stat.S_ISDIR(meta.st_mode)
                or meta.st_uid != os.geteuid() or meta.st_mode & 0o077):
            raise Refusal("Unsafe router socket directory.")
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(str(directory / "launch.sock"))
        _, uid, _ = struct.unpack("3i", self.sock.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        if uid != os.geteuid():
            raise Refusal("Router is not running as this user.")

def post(route, payload, retry=True):
    data = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    if len(data) > 256 * 1024:
        raise Refusal("Launch request too large; use stdin for a long prompt.")
    deadline = time.monotonic() + 5
    for n in range(2 if retry else 1):
        conn = LocalHTTP("localhost", timeout=min(2, max(0.01, deadline - time.monotonic())))
        try:
            conn.request("POST", "/v1/" + route, body=data, headers={"Content-Type": "application/json"})
            conn.sock.settimeout(max(0.01, deadline - time.monotonic()))
            resp = conn.getresponse()
            body = resp.read(512 * 1024 + 1)
            if len(body) > 512 * 1024:
                raise Refusal("Router reply too large.")
            try:
                result = json.loads(body)
            except (ValueError, UnicodeError):
                raise Refusal("Invalid router reply.") from None
            if not isinstance(result, dict):
                raise Refusal("Invalid router reply.")
            if resp.status != 200:
                raise Refusal(str(result.get("message", "Request refused.")))
            return result
        except (OSError, http.client.HTTPException):
            if not retry or n or time.monotonic() >= deadline:
                raise Refusal("Cannot reach ccpd on the private socket; nothing was launched.") from None
        finally:
            conn.close()
    raise Refusal("No launch decision.")

def valid_string(value):
    return isinstance(value, str) and "\0" not in value

def validate(plan, req):
    home = "CODEX_HOME" if req["tool"] == "codex" else "CLAUDE_CONFIG_DIR"
    allowed = {home} | PROXY | MARKERS
    put, remove, args = plan.get("env_set"), plan.get("env_unset"), plan.get("argv")
    if not (plan.get("v") == 1 and plan.get("request_id") == req["request_id"]
            and plan.get("tool") == req["tool"] and isinstance(put, dict)
            and isinstance(remove, list) and isinstance(args, list)
            and all(valid_string(v) for v in args + remove)
            and all(k in allowed and valid_string(v) for k, v in put.items())
            and set(remove) <= allowed and not set(remove).intersection(put)
            and os.path.isabs(put.get(home, "")) and valid_string(plan.get("label"))
            and not any(ord(c) < 32 for c in plan["label"])):
        raise Refusal("Invalid launch plan; refusing to execute.")
    if MARKERS.intersection(put):
        if (req["tool"] != "codex" or put.get("CCP_LAUNCH_ID") != req["request_id"]
                or put.get("CCP_OBSERVE_TICKET") != req["ticket"]):
            raise Refusal("Invalid observation ticket.")
    elif not MARKERS.issubset(remove):
        raise Refusal("Invalid launch environment.")
    env = os.environ.copy()
    for k in remove:
        env.pop(k, None)
    env.update(put)
    return args, env, put[home]

def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv or argv[0] in {"--help", "-h"}:
        print(HELP)
        return 0
    if argv[0] not in OVERRIDES:
        print(HELP, file=sys.stderr)
        return 2
    tool, args = argv[0], argv[1:]
    path = shutil.which(tool)
    path = os.path.realpath(path) if path else None
    if path and path in (os.path.realpath(__file__), os.path.realpath(sys.argv[0])):
        print("ccp: native command resolves to the launcher.", file=sys.stderr)
        return 2
    try:
        req = {"request_id": str(uuid.uuid4()), "ticket": secrets.token_hex(32),
               "tool": tool, "argv": args, "cwd": os.getcwd(), "binary": path,
               "override_names": sorted(k for k in OVERRIDES[tool] if os.environ.get(k))}
        plan = post("launch", req)
        args, env, home = validate(plan, req)
        if not path:
            raise Refusal("Native executable not found on PATH.")
        print("ccp: {}/{} -> {}".format(tool, plan["label"], home), file=sys.stderr, flush=True)
        try:
            os.execve(path, [tool] + args, env)
        except OSError as exc:
            try:
                post("exec-failure", {"request_id": req["request_id"], "ticket": req["ticket"],
                                      "errno": exc.errno}, retry=False)
            except Refusal:
                pass
            print("ccp: exec failed: {}".format(exc.strerror), file=sys.stderr)
            return 127 if exc.errno == errno.ENOENT else 126
    except (Refusal, UnicodeError, OSError) as exc:
        print("ccp: {}".format("Arguments must be UTF-8." if isinstance(exc, UnicodeError) else exc), file=sys.stderr)
        return 1

if __name__ == "__main__":
    sys.exit(main())
