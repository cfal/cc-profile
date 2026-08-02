#!/usr/bin/env python3
"""cc-profile (ccp) - switch Claude Code and Codex config-dir profiles."""
from __future__ import annotations

import errno
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

if sys.version_info < (3, 8):  # Path.unlink(missing_ok=) is the binding constraint
    raise SystemExit(f"{PROG} requires Python 3.8+, running {sys.version.split()[0]}")


@dataclass(frozen=True)
class Dim:
    """A switchable dimension: one symlink selecting one of several named dirs.

    Profiles and history sets differ only in where that symlink lives - a
    profile's is the path the CLI reads from its env var, a history set's is a
    `current` link inside the store - so both use the same machinery.
    """
    key: str              # what you type
    tool: str             # the CLI it belongs to
    history: bool
    exe: str = ""         # profile: binary for `run`
    env: str = ""         # profile: env var naming the config dir
    base: str = ""        # profile: $HOME basename when env is unset
    required: bool = False
    subdir: str = ""      # history: the dir inside config/ that lives here


DIMS = {
    # Claude requires the env var. Without it, .claude.json - which holds
    # projects, userID and oauthAccount - sits beside the config dir instead of
    # inside it, so profiles would keep sharing account state. Claude also
    # renames over that path, which destroys any symlink placed there, so the
    # env var is the only way to bring the file inside the profile.
    "claude": Dim("claude", "claude", False, exe="claude",
                  env="CLAUDE_CONFIG_DIR", base=".claude", required=True),
    "codex": Dim("codex", "codex", False, exe="codex",
                 env="CODEX_HOME", base=".codex"),
    # Transcripts, split out so history can follow the work rather than the
    # account. Directories only: SQLite keeps -wal/-shm beside the path it was
    # opened through, so symlinking codex's *.sqlite would strand the journal.
    "claude-history": Dim("claude-history", "claude", True, subdir="projects"),
    "codex-history": Dim("codex-history", "codex", True, subdir="sessions"),
}
PROFILES = [d for d in DIMS.values() if not d.history]
W = 15  # column width, sized for the longest key


def die(msg: str) -> NoReturn:
    print(f"{PROG}: {msg}", file=sys.stderr)
    raise SystemExit(1)


def home() -> Path:
    return Path(os.path.expanduser("~"))


def disp(p: Path) -> str:
    """Abbreviate $HOME to ~ so printed paths stay readable."""
    h, s = str(home()), str(p)
    return "~" + s[len(h):] if s == h or s.startswith(h + os.sep) else s


def dim_of(key: str) -> Dim:
    if key not in DIMS:
        die(f"unknown target {key!r} (expected: {', '.join(DIMS)})")
    return DIMS[key]


def valid(name: str) -> str:
    """Names become path components; reject anything that could escape."""
    if not NAME_RE.fullmatch(name or "") or name == "current":
        die(f"invalid name {name!r} (letters, digits, '-', '_'; 'current' is reserved)")
    return name


def profiles_root() -> Path:
    if os.environ.get("CC_PROFILE_DIR"):
        return Path(os.environ["CC_PROFILE_DIR"]).expanduser()
    xdg = os.environ.get("XDG_CONFIG_HOME") or (home() / ".config")
    return Path(xdg).expanduser() / "cc-profile"


def store(d: Dim) -> Path:
    """Directory holding this dimension's named entries, named for the key you type."""
    return profiles_root() / d.key


def marker(d: Dim) -> str:
    """Subdirectory whose presence makes an entry real rather than a stray dir."""
    return d.subdir if d.history else "config"


def target(d: Dim, name: str) -> Path:
    """What the selecting symlink points at."""
    p = store(d) / valid(name)
    return p if d.history else p / "config"


def config_dir(d: Dim, name: str) -> Path:
    return target(d, name)


def link_path(d: Dim) -> Path | None:
    """Where the selecting symlink lives. None when a required env var is unset."""
    if d.history:
        return store(d) / "current"
    if os.environ.get(d.env):
        return Path(os.environ[d.env]).expanduser()
    return None if d.required else home() / d.base


def entries(d: Dim) -> list[str]:
    s = store(d)
    if not s.is_dir():
        return []
    return sorted(p.name for p in s.iterdir()
                  if p.name != "current" and (p / marker(d)).is_dir())


def profiles(d: Dim) -> list[str]:
    return entries(d)


def current(d: Dim) -> str | None:
    """Active entry, derived purely from the symlink. There is no state file."""
    link = link_path(d)
    if link is None or not link.is_symlink():
        return None
    try:
        parts = Path(os.readlink(link)).relative_to(store(d)).parts
    except ValueError:
        return None
    if d.history:
        return parts[0] if len(parts) == 1 else None
    return parts[0] if len(parts) == 2 and parts[1] == "config" else None


def hist_of(d: Dim) -> Dim:
    return DIMS[f"{d.tool}-history"]


def hist_link(h: Dim, cfg: Path) -> tuple:
    """(link inside a profile's config dir, its target by way of `current`)."""
    return cfg / h.subdir, store(h) / "current" / h.subdir


def wire_history(d: Dim, cfg: Path) -> None:
    """Point a profile's history subdir at the shared `current`, when enabled.

    Never replaces a real directory: a profile holding its own transcripts keeps
    them until they are migrated deliberately.
    """
    h = hist_of(d)
    if not (store(h) / "current").is_symlink():
        return
    link, tgt = hist_link(h, cfg)
    if link.is_symlink() or not link.exists():
        relink(link, tgt)


def require_link(d: Dim) -> Path:
    """link_path(), refusing to proceed when it is unset or nonsensical."""
    link = link_path(d)
    if link is None:
        die(f"{d.env} is not set.\n"
            f"  {d.tool} keeps state outside its config dir unless it is set, so\n"
            f"  profiles would leak into each other. Add to your shell rc:\n\n"
            f'    export {d.env}="$HOME/{d.base}"\n')
    if not d.history and (link == profiles_root() or profiles_root() in link.parents):
        die(f"{d.env} points inside the profile root ({disp(link)}); it must point\n"
            f"  at the path {d.tool} reads, e.g. $HOME/{d.base}")
    return link


def _size(n: int) -> str:
    return f"{n / 1e9:.1f} GB" if n >= 1e9 else f"{n / 1e6:.0f} MB"


def refuse_real_dir(d: Dim, link: Path) -> None:
    if not link.is_symlink() and link.exists():
        die(f"{disp(link)} is a real directory, not a symlink.\n"
            f"  Move it into a profile first:\n"
            f"    {PROG} migrate {d.key} <name>")


def relink(link: Path, target: Path) -> None:
    """Atomically point `link` at `target`.

    os.replace is rename(2): there is no window where the path is missing, it
    acts on the symlink itself rather than through it, and it raises
    IsADirectoryError when `link` is a real directory - a backstop against
    clobbering a live config dir. `ln -sfn` is not atomic; it unlinks first.
    """
    # A custom CLAUDE_CONFIG_DIR/CODEX_HOME may nest below a directory that does
    # not exist yet; the tools create their config dir themselves, so match that.
    link.parent.mkdir(parents=True, exist_ok=True)
    tmp = link.with_name(f".{link.name}.{PROG}.{os.getpid()}")
    tmp.unlink(missing_ok=True)
    os.symlink(target, tmp)
    try:
        os.replace(tmp, link)
    except OSError:
        tmp.unlink(missing_ok=True)
        raise


PAD = " " * (W + 1)


def _rows(d: Dim, active: str | None) -> None:
    for n in entries(d):
        print(f"   {'*' if n == active else ' '} {n}")


def cmd_status() -> int:
    """Everything, in one listing: `ccp` and `ccp list` are the same command.

    Each tool shows its config profiles then its history sets, active entries
    marked. Where there is nothing to list - not linked yet, or empty - the
    guidance for fixing that takes the list's place.
    """
    print(disp(profiles_root()))
    for t in PROFILES:
        print()
        link, active = link_path(t), current(t)

        if link is None:
            print(f"{t.key:{W}} {t.env} is not set")
            print(f'{PAD}export {t.env}="$HOME/{t.base}"   # add to your shell rc')
            continue
        guided = False
        if not link.is_symlink():
            problem = "is a real directory - not linked" if link.exists() else "is missing"
            print(f"{t.key:{W}} {disp(link)} {problem}")
            print(f"{PAD}run:  {PROG} "
                  f"{'migrate ' + t.key + ' <name>' if link.exists() else 'init'}")
            guided = True
        else:
            tgt = Path(os.readlink(link))
            # Outside the profile root there is no name to show, so show the path.
            print(f"{t.key:{W}} {disp(link)} -> {active or disp(tgt)}"
                  f"{'' if tgt.is_dir() else '   [BROKEN: profile missing]'}")
            if not tgt.is_dir():
                print(f"{PAD}run:  {PROG} reinit")
                guided = True

        _note_orphan(t)
        _rows(t, active)
        # Suppressed when something more urgent was already suggested: `new` and
        # `migrate` lead opposite ways, and offering both invites abandoning a
        # real config dir for an empty profile.
        if not entries(t) and not guided:
            print(f"{PAD}no profiles yet:  {PROG} new {t.key} <name>")
        _show_history(hist_of(t), active)
    return 0


def _show_history(h: Dim, profile: str | None) -> None:
    link, active = link_path(h), current(h)
    if not link.is_symlink():
        print(f"{h.key:{W}} not enabled - {h.subdir}/ lives inside each profile")
        print(f"{PAD}share it:  {PROG} migrate {h.key} <name>")
        return
    tgt = Path(os.readlink(link))
    print(f"{h.key:{W}} {active or disp(tgt)}"
          f"{'' if tgt.is_dir() else '   [BROKEN: set missing]'}")
    _rows(h, active)
    # A profile predating the history store keeps its own transcripts and is
    # silently not following `current`; say so rather than implying it is.
    if profile:
        lk, _ = hist_link(h, target(DIMS[h.tool], profile))
        if lk.exists() and not lk.is_symlink():
            print(f"{PAD}note: profile {profile!r} holds its own {h.subdir}/ "
                  f"and is not using this")


def _note_orphan(t: Dim) -> None:
    """Flag ~/.claude.json when it exists but is not the file Claude reads.

    Written by anything that runs Claude without the user's shell env: the
    native updater, systemd units, cron. Harmless to Claude itself, but tools
    that read $HOME/.claude.json unconditionally will act on it instead of the
    live file, so it is worth surfacing. Never deleted - in a context without
    the env var it *is* the live file.
    """
    cfg = os.environ.get(t.env)
    if t.tool != "claude" or not cfg:
        return
    orphan = home() / ".claude.json"
    if not orphan.is_file() or orphan == Path(cfg).expanduser() / ".claude.json":
        return
    try:
        n = len(json.loads(orphan.read_text()).get("projects", {}))
    except (OSError, ValueError):
        n = 0
    print(f"{PAD}note: {disp(orphan)} exists and is unused ({t.env} is set) - "
          f"{orphan.stat().st_size} B, {f'{n} projects' if n else 'no projects'}")


def has_entry(d: Dim, name: str) -> bool:
    return (store(d) / valid(name) / marker(d)).is_dir()


def _init_target(d: Dim) -> str | None:
    """Entry init/reinit should link to, or None when it is ambiguous."""
    names = entries(d)
    if "default" in names or not names:
        return "default"
    return names[0] if len(names) == 1 else None


def cmd_init(force: bool) -> int:
    """Create the config-dir symlinks. `reinit` (force) also repoints existing ones."""
    rc = 0
    for t in PROFILES:
        link = require_link(t)
        if not link.is_symlink() and link.exists():
            print(f"{PROG}: {t.key}: {disp(link)} is a real directory, not a symlink\n"
                  f"  move it into a profile first:\n"
                  f"    {PROG} migrate {t.key} <name>", file=sys.stderr)
            rc = 1
            continue
        if link.is_symlink() and not force:
            tgt = Path(os.readlink(link))
            if current(t) is None:
                print(f"{PROG}: {t.key}: {disp(link)} points outside the profile root\n"
                      f"  -> {disp(tgt)}\n"
                      f"  run '{PROG} reinit' to repoint it", file=sys.stderr)
                rc = 1
                continue
            if tgt.is_dir():
                print(f"{t.key}: already linked -> {disp(tgt)}")
                wire_history(t, tgt)
                continue
        name = _init_target(t)
        if name is None:
            print(f"{PROG}: {t.key}: several profiles and no 'default' - pick one:\n"
                  f"    {PROG} switch {t.key} <name>", file=sys.stderr)
            rc = 1
            continue
        tgt = config_dir(t, name)
        tgt.mkdir(parents=True, exist_ok=True)
        relink(link, tgt)
        wire_history(t, tgt)
        print(f"{t.key}: {disp(link)} -> {disp(tgt)}")
    return rc


def cmd_switch(d: Dim, name: str) -> int:
    link = require_link(d)
    if not has_entry(d, name):
        die(f"no {d.key} {name!r} at {disp(store(d) / name)}\n"
            f"  create it:  {PROG} new {d.key} {name}")
    if not d.history:
        refuse_real_dir(d, link)
    prev, tgt = current(d), target(d, name)
    relink(link, tgt)
    print(f"{d.key}: {prev or '(none)'} -> {name}")
    print(f"  {disp(link)} -> {disp(tgt)}")
    return 0


def cmd_rename(d: Dim, name: str, new: str) -> int:
    old_dir, new_dir = store(d) / valid(name), store(d) / valid(new)
    if name == new:
        die(f"{name!r} is already the name")
    if not has_entry(d, name):
        die(f"no {d.key} {name!r} at {disp(old_dir)}")
    if new_dir.exists():
        die(f"{d.key} {new!r} already exists at {disp(new_dir)}")

    was_active = current(d) == name
    os.rename(old_dir, new_dir)          # same store, so never cross-device
    print(f"renamed {d.key} {name!r} -> {new!r}")
    print(f"  from  {disp(old_dir)}")
    print(f"    to  {disp(new_dir)}")
    if was_active:
        # The selecting symlink still names the old directory, so it dangles
        # until this repoint. Profiles linking a history set go by way of
        # `current` rather than the name, so they need no adjustment.
        link = require_link(d)
        relink(link, target(d, new))
        print(f"  link  {disp(link)} -> {disp(target(d, new))}")
    return 0


def cmd_new(d: Dim, name: str) -> int:
    base = store(d) / valid(name)
    if base.exists():
        die(f"{d.key} {name!r} already exists at {disp(base)}")
    (base / marker(d)).mkdir(parents=True)
    print(f"created {d.key} {name!r}")
    print(f"  {disp(base / marker(d))}")
    if not d.history:
        wire_history(d, target(d, name))
        lk, ht = hist_link(hist_of(d), target(d, name))
        if lk.is_symlink():
            print(f"  {disp(lk)} -> {disp(ht)}")
    return 0


def cmd_migrate(d: Dim, name: str) -> int:
    return _migrate_history(d, name) if d.history else _migrate_profile(d, name)


def _migrate_profile(t: Dim, name: str) -> int:
    link = require_link(t)
    dest = config_dir(t, name)
    if link.is_symlink():
        die(f"{disp(link)} is already a symlink -> {disp(Path(os.readlink(link)))}\n"
            f"  nothing to migrate")
    if not link.exists():
        die(f"{disp(link)} does not exist - nothing to migrate\n"
            f"  create an empty profile instead:  {PROG} new {t.key} {name}")
    if dest.exists():
        die(f"{t.key} profile {name!r} already exists at {disp(dest)}")

    print(f"migrating {t.key} into profile {name!r}")
    print(f"  from  {disp(link)}")
    print(f"    to  {disp(dest)}")
    _check_space(link)
    dest.parent.mkdir(parents=True, exist_ok=True)
    _move_dir(t, name, link, dest)
    _migrate_claude_json(t, dest)
    relink(link, dest)
    print(f"  link  {disp(link)} -> {disp(dest)}")
    wire_history(t, dest)
    print(f"done.  undo with:  rm {disp(link)} && mv {disp(dest)} {disp(link)}")
    return 0


def _check_space(src: Path) -> None:
    """Refuse a cross-filesystem move that cannot finish, before starting it."""
    root = profiles_root()
    root.mkdir(parents=True, exist_ok=True)
    if src.stat().st_dev == root.stat().st_dev:
        print("  (same filesystem - instant rename)")
        return
    # These dirs get large - Codex sessions run to gigabytes - so check before
    # spending minutes on a copy that cannot complete.
    need = sum(f.stat().st_size for f in src.rglob("*") if f.is_file())
    free = shutil.disk_usage(root).free
    print(f"  (crossing filesystems - copying {_size(need)}, "
          f"{_size(free)} free at the destination)")
    if need > free:
        die(f"not enough space at {disp(root)}: "
            f"need {_size(need)}, have {_size(free)}")


def _migrate_history(h: Dim, name: str) -> int:
    """Lift the active profile's transcripts into a shared, switchable set.

    Only the active profile donates: merging several profiles' transcripts is
    not reversible, so the others keep theirs and are reported instead.
    """
    t = DIMS[h.tool]
    active = current(t)
    if active is None:
        die(f"no active {h.tool} profile to take {h.subdir}/ from\n"
            f"  run this first:  {PROG} migrate {h.tool} <name>")
    src = target(t, active) / h.subdir
    base = store(h) / valid(name)
    if base.exists():
        die(f"{h.key} {name!r} already exists at {disp(base)}")
    if src.is_symlink():
        die(f"{disp(src)} is already a symlink - {h.key} is set up\n"
            f"  switch sets with:  {PROG} switch {h.key} <name>")

    print(f"migrating {h.tool} {h.subdir}/ into {h.key} {name!r}")
    print(f"  from  {disp(src)}")
    print(f"    to  {disp(base / h.subdir)}")
    base.mkdir(parents=True)
    if src.is_dir():
        _check_space(src)
        _move_dir(h, name, src, base / h.subdir)
    else:
        (base / h.subdir).mkdir()
        print("  (nothing there yet - created empty)")

    relink(store(h) / "current", base)
    print(f"  link  {disp(store(h) / 'current')} -> {disp(base)}")

    kept = []
    for p in entries(t):
        cfg = target(t, p)
        lk, tgt = hist_link(h, cfg)
        if lk.is_symlink() or not lk.exists():
            relink(lk, tgt)
            print(f"  link  {disp(lk)} -> {disp(tgt)}")
        else:
            kept.append(p)
    if kept:
        print(f"  note  these keep their own {h.subdir}/ and will not follow "
              f"{h.key}: {', '.join(kept)}")
    return 0


def _move_dir(t: Dim, name: str, src: Path, dest: Path) -> None:
    """Move a config dir, preferring an atomic rename.

    shutil.move does this too, but it reports both phases as one exception and
    the correct recovery is opposite for each: a failed copy must be cleaned up,
    while a failed *delete* must not be, because by then the destination is the
    only complete copy and removing it would lose the data outright.
    """
    try:
        os.rename(src, dest)
        return
    except OSError as e:
        if e.errno != errno.EXDEV:
            raise

    try:
        shutil.copytree(src, dest, symlinks=True)
    except OSError as e:
        # Nothing has been deleted yet, so dropping the partial copy restores
        # the pre-migration state exactly and frees the name for a retry.
        shutil.rmtree(dest, ignore_errors=True)
        die(f"{e}\n"
            f"  copy failed - {disp(src)} is unchanged\n"
            f"  removed the partial copy at {disp(dest)}")

    try:
        shutil.rmtree(src)
    except OSError as e:
        die(f"{e}\n"
            f"  copied to {disp(dest)}, but could not remove {disp(src)}.\n"
            f"  The copy is complete and has been kept - remove the original by\n"
            f"  hand, then:  {PROG} switch {t.key} {name}")


def _migrate_claude_json(t: Dim, dest: Path) -> None:
    """Fold in ~/.claude.json when it is the live file.

    On a machine that never had CLAUDE_CONFIG_DIR set, the live .claude.json is
    at ~/.claude.json, outside the directory just moved. Leaving it behind would
    strand projects, onboarding and oauthAccount, and Claude would come up
    looking freshly installed. When both exist the inner one is live and the
    outer is an updater orphan, so never overwrite inner with outer.
    """
    if t.tool != "claude":
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


def cmd_run(t: Dim, name: str, args: list[str]) -> int:
    """Run the tool under one profile without disturbing the active symlink.

    Setting the env var is enough on its own, so this touches no links at all
    and cannot race with `switch`. execvp replaces this process, so signals,
    exit codes and Ctrl-C behave as if the tool had been invoked directly.
    """
    if t.history:
        die(f"{t.key} is a history set, not something to run")
    cfg = config_dir(t, name)
    if not cfg.is_dir():
        die(f"no {t.key} profile {name!r} at {disp(cfg)}\n"
            f"  create it:  {PROG} new {t.key} {name}")
    # stderr: the tool's stdout may be piped or parsed, e.g. `codex exec --json`.
    print(f"{PROG}: {t.env}={disp(cfg)}", file=sys.stderr)
    os.environ[t.env] = str(cfg)
    try:
        os.execvp(t.exe, [t.exe, *args])
    except FileNotFoundError:
        die(f"{t.exe!r} not found on PATH")


HELP = f"""\
{PROG} - switch Claude Code and Codex config dirs and chat history

usage:
  {PROG}                              everything, active entries marked
  {PROG} list                         same thing
  {PROG} init                         create the config-dir symlinks
  {PROG} reinit                       recreate/repair the symlinks
  {PROG} new     <what> <name>        create an empty profile or history set
  {PROG} migrate <what> <name>        move existing real dirs into one
  {PROG} switch  <what> <name>        point the symlink at <name>
  {PROG} rename  <what> <name> <new>  rename one, following it if it is active
  {PROG} run     <tool> <name> [--] [args...]
                                      run <tool> under <name> for one
                                      invocation, leaving the active profile
                                      untouched

  <tool> is 'claude' or 'codex'.
  <what> is a tool, or 'claude-history' / 'codex-history'.

  Config profiles and history sets switch independently: a profile holds the
  account, settings and MCP config; a history set holds the transcripts that
  --resume reads. So the same chats can follow you across accounts, or you can
  keep a separate set for a clean room.

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
        return cmd_run(dim_of(tool), name, rest)

    if not argv:
        return cmd_status()
    cmd, rest = argv[0], argv[1:]

    if cmd in ("-h", "--help", "help"):
        print(HELP, end="")
        return 0
    if cmd == "list" and not rest:
        return cmd_status()
    if cmd in ("init", "reinit") and not rest:
        return cmd_init(force=cmd == "reinit")
    if cmd == "rename":
        if len(rest) != 3:
            die(f"usage: {PROG} rename <{'|'.join(DIMS)}> <name> <newname>")
        return cmd_rename(dim_of(rest[0]), rest[1], rest[2])
    if cmd in ("new", "migrate", "switch"):
        if len(rest) != 2:
            die(f"usage: {PROG} {cmd} <{'|'.join(DIMS)}> <name>")
        fn = {"new": cmd_new, "migrate": cmd_migrate, "switch": cmd_switch}[cmd]
        return fn(dim_of(rest[0]), rest[1])
    die(f"unknown command {cmd!r} - try:  {PROG} --help")


if __name__ == "__main__":
    raise SystemExit(main())
