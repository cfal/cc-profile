#!/usr/bin/env python3
"""cc-profile (ccp) - switch Claude Code and Codex config-dir profiles."""
from __future__ import annotations

import base64
import errno
import json
import os
import re
import shutil
import sys
import time
import urllib.parse
from dataclasses import dataclass
from pathlib import Path
from typing import NoReturn

import curl

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


def entry(d: Dim, name: str) -> Path:
    """A named entry - what `current` points at."""
    return store(d) / valid(name)


def config_dir(d: Dim, name: str) -> Path:
    """The directory the tool actually reads, inside an entry."""
    return entry(d, name) / marker(d)


def sel_link(d: Dim) -> Path:
    """The `current` symlink. Switching repoints exactly this and nothing else."""
    return store(d) / "current"


def outer_link(d: Dim) -> Path | None:
    """Where the CLI looks for its config dir. None when a required var is unset."""
    if d.history:
        return None
    if os.environ.get(d.env):
        return Path(os.environ[d.env]).expanduser()
    return None if d.required else home() / d.base


def outer_target(d: Dim) -> Path:
    """Set once by init and never again: it names `current`, not any profile."""
    return sel_link(d) / marker(d)


def link_target(link: Path) -> Path:
    """Absolute path a symlink names, without following any further links."""
    t = Path(os.readlink(link))
    return t if t.is_absolute() else Path(os.path.normpath(link.parent / t))


def entries(d: Dim) -> list[str]:
    s = store(d)
    if not s.is_dir():
        return []
    return sorted(p.name for p in s.iterdir()
                  if p.name != "current" and (p / marker(d)).is_dir())


def current(d: Dim) -> str | None:
    """Active entry, derived purely from `current`. There is no state file."""
    link = sel_link(d)
    if not link.is_symlink():
        return None
    try:
        parts = link_target(link).relative_to(store(d)).parts
    except ValueError:
        return None
    return parts[0] if len(parts) == 1 else None


def hist_of(d: Dim) -> Dim:
    return DIMS[f"{d.tool}-history"]


def hist_link(h: Dim, cfg: Path) -> tuple:
    """(link inside a profile's config dir, its target by way of `current`)."""
    return cfg / h.subdir, sel_link(h) / h.subdir


def wire_history(d: Dim, cfg: Path) -> None:
    """Point a profile's history subdir at the shared `current`, when enabled.

    Never replaces a real directory: a profile holding its own transcripts keeps
    them until they are migrated deliberately.
    """
    h = hist_of(d)
    if not sel_link(h).is_symlink():
        return
    link, tgt = hist_link(h, cfg)
    if link.is_symlink() or not link.exists():
        relink(link, tgt)


def require_link(d: Dim) -> Path:
    """outer_link(), refusing to proceed when it is unset or nonsensical."""
    link = outer_link(d)
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
    # Relative, so the whole profile root can be moved without rewriting the
    # links inside it. `current -> work` beats naming an absolute path to a
    # sibling directory.
    os.symlink(os.path.relpath(target, link.parent), tmp)
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
        link, active = outer_link(t), current(t)

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
        elif link_target(link) != outer_target(t):
            # Points at some other tree, or at a profile directly rather than
            # through `current` - most likely a root that has been moved.
            print(f"{t.key:{W}} {disp(link)} -> {disp(link_target(link))}   [STALE]")
            print(f"{PAD}run:  {PROG} reinit")
            guided = True
        elif active is None:
            print(f"{t.key:{W}} {disp(link)}, no profile selected")
            print(f"{PAD}run:  {PROG} " + ("init" if entries(t) else f"new {t.key} <name>"))
            guided = True
        else:
            broken = not config_dir(t, active).is_dir()
            print(f"{t.key:{W}} {disp(link)} -> {active}"
                  f"{'   [BROKEN: profile missing]' if broken else ''}")
            if broken:
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
    link, active = sel_link(h), current(h)
    if not link.is_symlink():
        print(f"{h.key:{W}} not enabled - {h.subdir}/ lives inside each profile")
        print(f"{PAD}share it:  {PROG} migrate {h.key} <name>")
        return
    broken = active is None or not config_dir(h, active).is_dir()
    print(f"{h.key:{W}} {active or disp(link_target(link))}"
          f"{'   [BROKEN: set missing]' if broken else ''}")
    _rows(h, active)
    # A profile predating the history store keeps its own transcripts and is
    # silently not following `current`; say so rather than implying it is.
    if profile:
        lk, _ = hist_link(h, config_dir(DIMS[h.tool], profile))
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
    return config_dir(d, name).is_dir()


def _sole_entry(d: Dim) -> str | None:
    """The only entry, when there is exactly one. No name is treated as special."""
    names = entries(d)
    return names[0] if len(names) == 1 else None


def ensure_outer(t: Dim) -> Path:
    """Point $CLAUDE_CONFIG_DIR / $CODEX_HOME at `current`, idempotently.

    The outer link names `current`, never a profile, so it is written once and
    survives every later switch and rename.
    """
    link = require_link(t)
    refuse_real_dir(t, link)
    if not link.is_symlink() or link_target(link) != outer_target(t):
        relink(link, outer_target(t))
        print(f"{t.key}: {disp(link)} -> {disp(outer_target(t))}")
    return link


def cmd_init() -> int:
    """Repair the symlinks for whatever profiles already exist.

    Creates nothing: a profile comes from `migrate` or `new`, both of which link
    as they go, so this is only needed to repair drift - an installer that
    replaced a symlink, or a profile root that moved.
    """
    rc = 0
    for t in PROFILES:
        link = outer_link(t)
        if link is None:
            print(f"{PROG}: {t.key}: {t.env} is not set - export it first",
                  file=sys.stderr)
            rc = 1
            continue
        if not link.is_symlink() and link.exists():
            print(f"{PROG}: {t.key}: {disp(link)} is a real directory, not a symlink\n"
                  f"  move it into a profile first:\n"
                  f"    {PROG} migrate {t.key} <name>", file=sys.stderr)
            rc = 1
            continue

        name = current(t)
        if name is None or not has_entry(t, name):
            name = _sole_entry(t)
            if name is None:
                found = entries(t)
                print(f"{PROG}: {t.key}: " + (
                    f"no profiles yet\n    {PROG} migrate {t.key} <name>"
                    f"   moves your existing config dir into one"
                    if not found else
                    f"{len(found)} profiles, none selected - pick one\n"
                    f"    {PROG} switch {t.key} <name>"), file=sys.stderr)
                rc = 1
                continue
            relink(sel_link(t), entry(t, name))
            print(f"{t.key}: current -> {name}")
        ensure_outer(t)
        wire_history(t, config_dir(t, name))
    return rc


def cmd_switch(d: Dim, name: str) -> int:
    if not d.history:
        require_link(d)      # fail before moving `current`, not after
    if not has_entry(d, name):
        die(f"no {d.key} {name!r} at {disp(entry(d, name))}\n"
            f"  create it:  {PROG} new {d.key} {name}")
    prev = current(d)
    # Only `current` moves. The outer link names `current`, so it stays valid
    # and switching never touches anything outside the profile root.
    relink(sel_link(d), entry(d, name))
    print(f"{d.key}: {prev or '(none)'} -> {name}")
    print(f"  {disp(sel_link(d))} -> {name}")
    if not d.history:
        ensure_outer(d)
        wire_history(d, config_dir(d, name))
    return 0


def cmd_rename(d: Dim, name: str, new: str) -> int:
    old_dir, new_dir = entry(d, name), entry(d, new)
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
        # `current` still names the old directory. Nothing else does - the outer
        # link and every profile's history link go by way of `current`.
        relink(sel_link(d), new_dir)
        print(f"  link  {disp(sel_link(d))} -> {new}")
    return 0


def cmd_new(d: Dim, name: str) -> int:
    base = entry(d, name)
    if base.exists():
        die(f"{d.key} {name!r} already exists at {disp(base)}")
    (base / marker(d)).mkdir(parents=True)
    print(f"created {d.key} {name!r}")
    print(f"  {disp(base / marker(d))}")
    if not d.history:
        wire_history(d, config_dir(d, name))
        lk, ht = hist_link(hist_of(d), config_dir(d, name))
        if lk.is_symlink():
            print(f"  {disp(lk)} -> {disp(ht)}")
    return 0


def cmd_migrate(d: Dim, name: str) -> int:
    return _migrate_history(d, name) if d.history else _migrate_profile(d, name)


def _migrate_profile(t: Dim, name: str) -> int:
    link = require_link(t)
    dest = config_dir(t, name)
    if link.is_symlink():
        die(f"{disp(link)} is already a symlink -> {disp(link_target(link))}\n"
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
    relink(sel_link(t), entry(t, name))
    print(f"  link  {disp(sel_link(t))} -> {name}")
    ensure_outer(t)
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
    src = config_dir(t, active) / h.subdir
    base = entry(h, name)
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

    relink(sel_link(h), base)
    print(f"  link  {disp(sel_link(h))} -> {name}")

    kept = []
    for p in entries(t):
        cfg = config_dir(t, p)
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


USAGE_TIMEOUT = 20
CLAUDE_USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
CODEX_USAGE_URL = "https://chatgpt.com/backend-api/wham/usage"


def _read_json(p: Path):
    try:
        return json.loads(p.read_text())
    except (OSError, ValueError):
        return None


def _send(url: str, headers: dict, data: bytes = None) -> tuple:
    """(status, parsed body). status None means the request never completed.

    Goes through the curl binary rather than urllib: it is a TLS-impersonating
    build here, and these endpoints sit behind bot filters that treat a
    scripting-language fingerprint very differently from a browser's.
    """
    try:
        r = curl.request(url, "POST" if data is not None else "GET",
                         headers=headers, data=data, timeout=USAGE_TIMEOUT)
    except curl.CurlError as e:
        return None, {"error": str(e)}
    try:
        return r.status, r.json()
    except ValueError:
        # Keep the text: an edge rejection is HTML, and losing it turns a
        # diagnosable "Cloudflare denied the agent string" into a bare 403.
        return r.status, {"error": " ".join(r.text.split())[:120]}


def _get(url: str, headers: dict) -> tuple:
    status, body = _send(url, headers)
    return status, body if status == 200 else (body or {}).get("error", body)


def _left(seconds: float) -> str:
    if seconds <= 0:
        return "now"
    value, unit = ((seconds / 86400, "d") if seconds >= 86400
                   else (seconds / 3600, "h"))
    return f"{value:.1f}".rstrip("0").rstrip(".") + unit


def _jwt_exp(token: str):
    """`exp` from a JWT payload. Read locally so a dead token is never sent."""
    try:
        part = token.split(".")[1]
        return json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))["exp"]
    except Exception:
        return None


def _window(label: str, w, reset_key: str = "resets_at"):
    if not isinstance(w, dict) or w.get("utilization") is None:
        return None
    out = f"{label} {w['utilization']:.0f}%"
    raw = w.get(reset_key)
    if isinstance(raw, str):
        try:
            from datetime import datetime, timezone
            left = datetime.fromisoformat(raw) - datetime.now(timezone.utc)
            out += f" (resets in {_left(left.total_seconds())})"
        except ValueError:
            pass
    return out


def _http_note(status, body) -> str:
    if status == 401:
        return "unauthorized - log in again"
    if status == 429:
        return "rate limited by the provider, try again shortly"
    if status is None:
        return f"unreachable: {body}"
    return f"HTTP {status}"


# Commands that make the CLI renew its own credentials. Delegating means we
# never parse or write the credential files ourselves, and only one party ever
# holds the refresh token - which matters because both providers rotate it.
#
# Refreshing is done here rather than by nudging the CLI, because no CLI
# subcommand renews without calling the API: `claude auth status` reads local
# state, `claude doctor` and `mcp list` only clear credentials they judge
# malformed, and codex's login status/doctor/mcp list/plugin list all leave
# auth.json untouched. Both CLIs renew lazily when about to make a request.
# These are the same endpoints and client IDs CodexBar uses.
# Anthropic sits behind Cloudflare, which rejects an unrecognised agent string
# with a 403 before the request ever reaches the API. Shared by both requests so
# they cannot drift apart - the usage GET had it and the refresh POST did not,
# which is exactly how that 403 arose.
CLAUDE_UA = "claude-code/2.1.227"
CLAUDE_TOKEN_URL = "https://platform.claude.com/v1/oauth/token"
CLAUDE_CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
CODEX_TOKEN_URL = "https://auth.openai.com/oauth/token"
CODEX_CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann"


def _post(url: str, data: dict, form: bool, agent: str = "") -> tuple:
    body, ctype = ((urllib.parse.urlencode(data).encode(),
                    "application/x-www-form-urlencoded") if form else
                   (json.dumps(data).encode(), "application/json"))
    headers = {"Content-Type": ctype, "Accept": "application/json"}
    if agent:
        headers["User-Agent"] = agent
    return _send(url, headers, body)


def _write_secret(path: Path, doc: dict) -> None:
    """Replace a credential file atomically, never leaving it world-readable.

    Written to a 0600 temp file and renamed over the target, so an interrupted
    write cannot leave a half-file where the credentials used to be.
    """
    tmp = path.with_name(f".{path.name}.{PROG}.{os.getpid()}")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        with os.fdopen(fd, "w") as f:
            json.dump(doc, f, indent=2)
        os.replace(tmp, path)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise


def _refresh_failure(status, body: dict) -> str:
    """Terminal failures need a re-login; everything else is worth retrying."""
    code = (body or {}).get("error", "")
    if status in (400, 401) and code in ("invalid_grant", "invalid_request", ""):
        return "login expired"
    if status is None:
        return f"refresh unreachable: {(body or {}).get('error', '?')}"
    if status == 429:
        return "refresh rate limited - try again shortly"
    return f"refresh failed: HTTP {status}{f' ({code})' if code else ''}"


def _refresh_claude(cfg: Path) -> str:
    """Renew in place. Empty string on success, else why not.

    The new refresh token is written back to the same file the CLI reads, so the
    rotation lands where the next session will look for it. A response without a
    new refresh token means this provider did not rotate: keep the existing one.
    """
    path = cfg / ".credentials.json"
    doc = _read_json(path) or {}
    oauth = doc.get("claudeAiOauth") or {}
    token = oauth.get("refreshToken")
    if not token:
        return "no refresh token"
    status, body = _post(CLAUDE_TOKEN_URL, {
        "grant_type": "refresh_token",
        "refresh_token": token,
        "client_id": CLAUDE_CLIENT_ID}, form=True, agent=CLAUDE_UA)
    if status != 200 or not body.get("access_token"):
        return _refresh_failure(status, body)
    oauth["accessToken"] = body["access_token"]
    oauth["refreshToken"] = body.get("refresh_token") or token
    oauth["expiresAt"] = int((time.time() + float(body.get("expires_in", 0))) * 1000)
    doc["claudeAiOauth"] = oauth
    _write_secret(path, doc)
    return ""


def _refresh_codex(cfg: Path) -> str:
    path = cfg / "auth.json"
    doc = _read_json(path) or {}
    tokens = doc.get("tokens") or {}
    token = tokens.get("refresh_token")
    if not token:
        return "no refresh token"
    status, body = _post(CODEX_TOKEN_URL, {
        "client_id": CODEX_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": token,
        "scope": "openid profile email"}, form=False)
    if status != 200 or not body.get("access_token"):
        return _refresh_failure(status, body)
    tokens["access_token"] = body["access_token"]
    tokens["refresh_token"] = body.get("refresh_token") or token
    if body.get("id_token"):
        tokens["id_token"] = body["id_token"]
    doc["tokens"] = tokens
    doc["last_refresh"] = time.strftime("%Y-%m-%dT%H:%M:%S.000000Z", time.gmtime())
    _write_secret(path, doc)
    return ""


def _refresh(d: Dim, cfg: Path) -> str:
    print(f"{PROG}: refreshing {d.key}/{cfg.parent.name}", file=sys.stderr)
    return (_refresh_claude if d.tool == "claude" else _refresh_codex)(cfg)


def _claude_usage(cfg: Path, retry: bool = True) -> str:
    oauth = (_read_json(cfg / ".credentials.json") or {}).get("claudeAiOauth") or {}
    token = oauth.get("accessToken")
    if not token:
        return "not logged in"
    exp = oauth.get("expiresAt")
    if isinstance(exp, (int, float)) and exp / 1000 <= time.time():
        # A lapsed refresh token cannot be renewed by anything, so say so rather
        # than suggesting a nudge that cannot work.
        ref = oauth.get("refreshTokenExpiresAt")
        if isinstance(ref, (int, float)) and ref / 1000 <= time.time():
            return f"login expired - run:  {PROG} run claude {cfg.parent.name}"
        if not retry:
            return "still expired after refresh"
        why = _refresh(DIMS["claude"], cfg)
        if why:
            return f"{why} - run:  {PROG} run claude {cfg.parent.name}" \
                if why == "login expired" else why
        return _claude_usage(cfg, retry=False)          # bounded: one attempt
    status, body = _get(CLAUDE_USAGE_URL, {
        "Authorization": f"Bearer {token}",
        "Accept": "application/json",
        # Undocumented endpoint: Claude Code's own beta header and agent string.
        "anthropic-beta": "oauth-2025-04-20",
        "User-Agent": CLAUDE_UA,
    })
    if status != 200:
        return _http_note(status, body)
    parts = [p for p in (_window("5h", body.get("five_hour")),
                         _window("7d", body.get("seven_day")),
                         _window("7d-opus", body.get("seven_day_opus"))) if p]
    return "   ".join(parts) or "no limits reported"


def _codex_usage(cfg: Path, retry: bool = True) -> str:
    tokens = (_read_json(cfg / "auth.json") or {}).get("tokens") or {}
    token = tokens.get("access_token")
    if not token:
        return "not logged in"
    exp = _jwt_exp(token)
    if exp and exp <= time.time():
        if not retry:
            return "still expired after refresh"
        why = _refresh(DIMS["codex"], cfg)
        if why:
            return f"{why} - run:  {PROG} run codex {cfg.parent.name}" \
                if why == "login expired" else why
        return _codex_usage(cfg, retry=False)
    headers = {"Authorization": f"Bearer {token}", "Accept": "application/json",
               "User-Agent": f"{PROG}"}
    if tokens.get("account_id"):
        headers["ChatGPT-Account-Id"] = tokens["account_id"]
    status, body = _get(CODEX_USAGE_URL, headers)
    if status != 200:
        return _http_note(status, body)
    rl = body.get("rate_limit") or {}
    parts = []
    for label, key in (("primary", "primary_window"), ("secondary", "secondary_window")):
        w = rl.get(key)
        if isinstance(w, dict) and w.get("used_percent") is not None:
            span = _left(w.get("limit_window_seconds") or 0)
            left = w.get("reset_after_seconds")
            parts.append(f"{span} {w['used_percent']:.0f}%" +
                         (f" (resets in {_left(left)})" if left else ""))
    for extra in body.get("additional_rate_limits") or []:
        w = (extra.get("rate_limit") or {}).get("primary_window") or {}
        if w.get("used_percent"):
            parts.append(f"{extra.get('limit_name', '?')} {w['used_percent']:.0f}%")
    if body.get("plan_type"):
        parts.append(f"({body['plan_type']})")
    return "   ".join(parts) or "no limits reported"


def cmd_usage(only: str = "") -> int:
    """Per-profile quota, read straight from each profile's stored credentials.

    Never refreshes: a rotated refresh token would be written back into a
    profile another process may be using, and Anthropic's rotate-on-refresh
    would invalidate the copy that process holds. Reading is enough while a
    token is live - Codex tokens last about a week, Claude's about six hours.
    """
    fetch = {"claude": _claude_usage, "codex": _codex_usage}
    for t in PROFILES:
        if only and t.key != only:
            continue
        active, names = current(t), entries(t)
        print(f"{t.key}")
        if not names:
            print(f"{PAD}no profiles")
            continue
        for n in names:
            print(f"   {'*' if n == active else ' '} {n:12} {fetch[t.tool](config_dir(t, n))}")
    return 0


HELP = f"""\
{PROG} - switch Claude Code and Codex config dirs and chat history

usage:
  {PROG}                              everything, active entries marked
  {PROG} list                         same thing
  {PROG} init, {PROG} reinit              repair the symlinks (same command;
                                      creates no profiles)
  {PROG} new     <what> <name>        create an empty profile or history set
  {PROG} migrate <what> <name>        move existing real dirs into one
  {PROG} switch  <what> <name>        point the symlink at <name>
  {PROG} rename  <what> <name> <new>  rename one, following it if it is active
  {PROG} usage   [<tool>]             quota for every profile (read-only; never
                                      refreshes a token)
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
        return cmd_init()
    if cmd == "usage":
        if len(rest) > 1 or (rest and rest[0] not in ("claude", "codex")):
            die(f"usage: {PROG} usage [claude|codex]")
        return cmd_usage(rest[0] if rest else "")
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
