#!/usr/bin/env python3
"""cc-profile (ccp) - switch Claude Code and Codex config-dir profiles."""
from __future__ import annotations

import json
import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import NoReturn

PROG = "ccp"
NAME_RE = re.compile(r"[A-Za-z0-9_-]+")


@dataclass(frozen=True)
class Tool:
    name: str       # profile namespace
    exe: str        # binary to exec
    env: str        # env var holding the tool's config dir
    default: str    # basename under $HOME when env is unset
    required: bool  # env var must be set


TOOLS = {
    # Claude requires the env var. Without it, .claude.json - which holds
    # projects, userID and oauthAccount - sits beside the config dir instead of
    # inside it, so profiles would keep sharing account state. Claude also
    # renames over that path, which destroys any symlink placed there, so the
    # env var is the only way to bring the file inside the profile.
    "claude": Tool("claude", "claude", "CLAUDE_CONFIG_DIR", ".claude", True),
    "codex": Tool("codex", "codex", "CODEX_HOME", ".codex", False),
}


def die(msg: str) -> NoReturn:
    print(f"{PROG}: {msg}", file=sys.stderr)
    raise SystemExit(1)


def home() -> Path:
    return Path(os.path.expanduser("~"))


def disp(p: Path) -> str:
    """Abbreviate $HOME to ~ so printed paths stay readable."""
    h, s = str(home()), str(p)
    return "~" + s[len(h):] if s == h or s.startswith(h + os.sep) else s


def tool_of(name: str) -> Tool:
    if name not in TOOLS:
        die(f"unknown tool {name!r} (expected: {', '.join(TOOLS)})")
    return TOOLS[name]


def valid(name: str) -> str:
    """Profile names become path components; reject anything that could escape."""
    if not NAME_RE.fullmatch(name or ""):
        die(f"invalid profile name {name!r} (allowed: letters, digits, '-', '_')")
    return name


def profiles_root() -> Path:
    if os.environ.get("CC_PROFILE_DIR"):
        return Path(os.environ["CC_PROFILE_DIR"]).expanduser()
    xdg = os.environ.get("XDG_CONFIG_HOME") or (home() / ".config")
    return Path(xdg).expanduser() / "cc-profile"


def config_dir(t: Tool, name: str) -> Path:
    return profiles_root() / t.name / valid(name) / "config"


def link_path(t: Tool) -> Path | None:
    """Where the symlink must live. None when a required env var is unset."""
    if os.environ.get(t.env):
        return Path(os.environ[t.env]).expanduser()
    return None if t.required else home() / t.default


def profiles(t: Tool) -> list[str]:
    root = profiles_root() / t.name
    if not root.is_dir():
        return []
    return sorted(p.name for p in root.iterdir() if (p / "config").is_dir())


def current(t: Tool) -> str | None:
    """Active profile, derived purely from the symlink. There is no state file."""
    link = link_path(t)
    if link is None or not link.is_symlink():
        return None
    try:
        rel = Path(os.readlink(link)).relative_to(profiles_root() / t.name)
    except ValueError:
        return None
    return rel.parts[0] if len(rel.parts) == 2 and rel.parts[1] == "config" else None


def cmd_status() -> int:
    for t in TOOLS.values():
        link = link_path(t)
        if link is None:
            print(f"{t.name:8} (unconfigured)")
            print(f"  {t.env} is not set - add to your shell rc:")
            print(f'    export {t.env}="$HOME/{t.default}"')
        elif not link.is_symlink():
            state = "a real directory" if link.exists() else "missing"
            print(f"{t.name:8} (not linked)")
            print(f"  {disp(link)} is {state}")
            print(f"  run:  {PROG} {'migrate ' + t.name + ' <name>' if link.exists() else 'init'}")
        else:
            target = Path(os.readlink(link))
            broken = "" if target.is_dir() else "   [BROKEN: target missing]"
            print(f"{t.name:8} {current(t) or '(outside profile root)'}{broken}")
            print(f"  {disp(link)} -> {disp(target)}")
        _note_orphan(t)
    return 0


def _note_orphan(t: Tool) -> None:
    """Flag ~/.claude.json when it exists but is not the file Claude reads.

    Written by anything that runs Claude without the user's shell env: the
    native updater, systemd units, cron. Harmless to Claude itself, but tools
    that read $HOME/.claude.json unconditionally will act on it instead of the
    live file, so it is worth surfacing. Never deleted - in a context without
    the env var it *is* the live file.
    """
    cfg = os.environ.get(t.env)
    if t.name != "claude" or not cfg:
        return
    orphan = home() / ".claude.json"
    if not orphan.is_file() or orphan == Path(cfg).expanduser() / ".claude.json":
        return
    try:
        n = len(json.loads(orphan.read_text()).get("projects", {}))
    except (OSError, ValueError):
        n = 0
    print(f"  note: {disp(orphan)} exists and is unused ({t.env} is set) - "
          f"{orphan.stat().st_size} B, {f'{n} projects' if n else 'no projects'}")


def cmd_list() -> int:
    for t in TOOLS.values():
        active, names = current(t), profiles(t)
        print(f"{t.name}  ({disp(profiles_root() / t.name)})")
        if not names:
            print("    (none)")
        for n in names:
            print(f"  {'*' if n == active else ' '} {n}")
    return 0


HELP = f"""\
{PROG} - switch Claude Code and Codex config-dir profiles

usage:
  {PROG}                              active profile for each tool
  {PROG} list                         list all profiles

  <tool> is 'claude' or 'codex'.

environment:
  CC_PROFILE_DIR     profile root (default: $XDG_CONFIG_HOME/cc-profile)
  CLAUDE_CONFIG_DIR  required - where Claude Code reads its config dir, so that
                     .claude.json lives inside the profile
  CODEX_HOME         optional - defaults to ~/.codex
"""


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv:
        return cmd_status()
    cmd, rest = argv[0], argv[1:]

    if cmd in ("-h", "--help", "help"):
        print(HELP, end="")
        return 0
    if cmd == "list" and not rest:
        return cmd_list()
    die(f"unknown command {cmd!r} - try:  {PROG} --help")


if __name__ == "__main__":
    raise SystemExit(main())
