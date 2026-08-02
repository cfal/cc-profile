#!/usr/bin/env python3
"""cc-profile (ccp) - switch Claude Code and Codex config-dir profiles."""
from __future__ import annotations

import json
import os
import re
import shutil
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


def require_link(t: Tool) -> Path:
    """link_path(), refusing to proceed when it is unset or nonsensical."""
    link = link_path(t)
    if link is None:
        die(f"{t.env} is not set.\n"
            f"  {t.name} keeps state outside its config dir unless it is set, so\n"
            f"  profiles would leak into each other. Add to your shell rc:\n\n"
            f'    export {t.env}="$HOME/{t.default}"\n')
    if link == profiles_root() or profiles_root() in link.parents:
        die(f"{t.env} points inside the profile root ({disp(link)}); it must point\n"
            f"  at the path {t.name} reads, e.g. $HOME/{t.default}")
    return link


def refuse_real_dir(t: Tool, link: Path) -> None:
    if not link.is_symlink() and link.exists():
        die(f"{disp(link)} is a real directory, not a symlink.\n"
            f"  Move it into a profile first:\n"
            f"    {PROG} migrate {t.name} <name>")


def relink(link: Path, target: Path) -> None:
    """Atomically point `link` at `target`.

    os.replace is rename(2): there is no window where the path is missing, it
    acts on the symlink itself rather than through it, and it raises
    IsADirectoryError when `link` is a real directory - a backstop against
    clobbering a live config dir. `ln -sfn` is not atomic; it unlinks first.
    """
    tmp = link.with_name(f".{link.name}.{PROG}.{os.getpid()}")
    tmp.unlink(missing_ok=True)
    os.symlink(target, tmp)
    try:
        os.replace(tmp, link)
    except OSError:
        tmp.unlink(missing_ok=True)
        raise


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


def _init_target(t: Tool) -> str | None:
    """Profile init/reinit should link to, or None when it is ambiguous."""
    names = profiles(t)
    if "default" in names or not names:
        return "default"
    return names[0] if len(names) == 1 else None


def cmd_init(force: bool) -> int:
    """Create the config-dir symlinks. `reinit` (force) also repoints existing ones."""
    rc = 0
    for t in TOOLS.values():
        link = require_link(t)
        if not link.is_symlink() and link.exists():
            print(f"{PROG}: {t.name}: {disp(link)} is a real directory, not a symlink\n"
                  f"  move it into a profile first:\n"
                  f"    {PROG} migrate {t.name} <name>", file=sys.stderr)
            rc = 1
            continue
        if link.is_symlink() and not force:
            target = Path(os.readlink(link))
            if current(t) is None:
                print(f"{PROG}: {t.name}: {disp(link)} points outside the profile root\n"
                      f"  -> {disp(target)}\n"
                      f"  run '{PROG} reinit' to repoint it", file=sys.stderr)
                rc = 1
                continue
            if target.is_dir():
                print(f"{t.name}: already linked -> {disp(target)}")
                continue
        name = _init_target(t)
        if name is None:
            print(f"{PROG}: {t.name}: several profiles and no 'default' - pick one:\n"
                  f"    {PROG} switch {t.name} <name>", file=sys.stderr)
            rc = 1
            continue
        target = config_dir(t, name)
        target.mkdir(parents=True, exist_ok=True)
        relink(link, target)
        print(f"{t.name}: {disp(link)} -> {disp(target)}")
    return rc


def cmd_switch(t: Tool, name: str) -> int:
    link = require_link(t)
    target = config_dir(t, name)
    if not target.is_dir():
        die(f"no {t.name} profile {name!r} at {disp(target)}\n"
            f"  create it:  {PROG} new {t.name} {name}")
    refuse_real_dir(t, link)
    prev = current(t)
    relink(link, target)
    print(f"{t.name}: {prev or '(none)'} -> {name}")
    print(f"  {disp(link)} -> {disp(target)}")
    return 0


def cmd_new(t: Tool, name: str) -> int:
    target = config_dir(t, name)
    if target.exists():
        die(f"{t.name} profile {name!r} already exists at {disp(target)}")
    target.mkdir(parents=True)
    print(f"created {t.name} profile {name!r}")
    print(f"  {disp(target)}")
    return 0


def cmd_migrate(t: Tool, name: str) -> int:
    link = require_link(t)
    dest = config_dir(t, name)
    if link.is_symlink():
        die(f"{disp(link)} is already a symlink -> {disp(Path(os.readlink(link)))}\n"
            f"  nothing to migrate")
    if not link.exists():
        die(f"{disp(link)} does not exist - nothing to migrate\n"
            f"  create an empty profile instead:  {PROG} new {t.name} {name}")
    if dest.exists():
        die(f"{t.name} profile {name!r} already exists at {disp(dest)}")

    dest.parent.mkdir(parents=True, exist_ok=True)
    same_fs = link.stat().st_dev == dest.parent.stat().st_dev
    print(f"migrating {t.name} into profile {name!r}")
    print(f"  from  {disp(link)}")
    print(f"    to  {disp(dest)}")
    print("  " + ("(same filesystem - instant rename)" if same_fs else
                  "(crossing filesystems - copying, this may take a while)"))
    shutil.move(str(link), str(dest))
    _migrate_claude_json(t, dest)
    relink(link, dest)
    print(f"  link  {disp(link)} -> {disp(dest)}")
    print(f"done.  undo with:  rm {disp(link)} && mv {disp(dest)} {disp(link)}")
    return 0


def _migrate_claude_json(t: Tool, dest: Path) -> None:
    """Fold in ~/.claude.json when it is the live file.

    On a machine that never had CLAUDE_CONFIG_DIR set, the live .claude.json is
    at ~/.claude.json, outside the directory just moved. Leaving it behind would
    strand projects, onboarding and oauthAccount, and Claude would come up
    looking freshly installed. When both exist the inner one is live and the
    outer is an updater orphan, so never overwrite inner with outer.
    """
    if t.name != "claude":
        return
    inner, outer = dest / ".claude.json", home() / ".claude.json"
    if inner.exists():
        if outer.is_file():
            print(f"  note  {disp(outer)} left in place (stale orphan; "
                  f"{disp(inner)} is live)")
        return
    if outer.is_file() and not outer.is_symlink():
        print(f"  move  {disp(outer)}")
        print(f"    to  {disp(inner)}")
        shutil.move(str(outer), str(inner))


def cmd_run(t: Tool, name: str, args: list[str]) -> int:
    """Run the tool under one profile without disturbing the active symlink.

    Setting the env var is enough on its own, so this touches no links at all
    and cannot race with `switch`. execvp replaces this process, so signals,
    exit codes and Ctrl-C behave as if the tool had been invoked directly.
    """
    cfg = config_dir(t, name)
    if not cfg.is_dir():
        die(f"no {t.name} profile {name!r} at {disp(cfg)}\n"
            f"  create it:  {PROG} new {t.name} {name}")
    # stderr: the tool's stdout may be piped or parsed, e.g. `codex exec --json`.
    print(f"{PROG}: {t.env}={disp(cfg)}", file=sys.stderr)
    os.environ[t.env] = str(cfg)
    try:
        os.execvp(t.exe, [t.exe, *args])
    except FileNotFoundError:
        die(f"{t.exe!r} not found on PATH")


HELP = f"""\
{PROG} - switch Claude Code and Codex config-dir profiles

usage:
  {PROG}                              active profile for each tool
  {PROG} list                         list all profiles
  {PROG} init                         create the config-dir symlinks
  {PROG} reinit                       recreate/repair the symlinks
  {PROG} new     <tool> <name>        create an empty profile
  {PROG} migrate <tool> <name>        move an existing real config dir into a profile
  {PROG} switch  <tool> <name>        point the symlink at <name>
  {PROG} run     <tool> <name> [--] [args...]
                                      run <tool> under <name> for one
                                      invocation, leaving the active profile
                                      untouched

  <tool> is 'claude' or 'codex'.

environment:
  CC_PROFILE_DIR     profile root (default: $XDG_CONFIG_HOME/cc-profile)
  CLAUDE_CONFIG_DIR  required - where Claude Code reads its config dir, so that
                     .claude.json lives inside the profile
  CODEX_HOME         optional - defaults to ~/.codex
"""


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)

    # Sliced off before any parsing: everything after <name> belongs to the
    # child, so `ccp run claude work --help` reaches claude rather than us.
    if argv and argv[0] == "run":
        if len(argv) < 3:
            die(f"usage: {PROG} run <claude|codex> <name> [--] [args...]")
        _, tool, name, *rest = argv
        if rest and rest[0] == "--":
            rest = rest[1:]
        return cmd_run(tool_of(tool), name, rest)

    if not argv:
        return cmd_status()
    cmd, rest = argv[0], argv[1:]

    if cmd in ("-h", "--help", "help"):
        print(HELP, end="")
        return 0
    if cmd == "list" and not rest:
        return cmd_list()
    if cmd in ("init", "reinit") and not rest:
        return cmd_init(force=cmd == "reinit")
    if cmd in ("new", "migrate", "switch"):
        if len(rest) != 2:
            die(f"usage: {PROG} {cmd} <claude|codex> <name>")
        fn = {"new": cmd_new, "migrate": cmd_migrate, "switch": cmd_switch}[cmd]
        return fn(tool_of(rest[0]), rest[1])
    die(f"unknown command {cmd!r} - try:  {PROG} --help")


if __name__ == "__main__":
    raise SystemExit(main())
