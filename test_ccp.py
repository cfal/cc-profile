#!/usr/bin/env python3
"""Self-contained checks for cc_profile.

Run:  python test_ccp.py     (also collectable by pytest)

Every test builds an isolated $HOME and profile root, so nothing here can
reach the real ~/.claude or ~/.codex.
"""
from __future__ import annotations

import contextlib
import importlib
import io
import json
import os
import shutil
import sys
import tempfile
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cc_profile as cp


@contextlib.contextmanager
def sandbox(claude_env: bool = True, **extra: str):
    """Fresh $HOME and profile root, with the module reloaded against them."""
    with tempfile.TemporaryDirectory() as tmp:
        d = Path(tmp)
        env = {"HOME": str(d), "PATH": os.environ.get("PATH", "/usr/bin"),
               "CC_PROFILE_DIR": str(d / "prof"), **extra}
        if claude_env:
            env["CLAUDE_CONFIG_DIR"] = str(d / ".claude")
        with mock.patch.dict(os.environ, env, clear=True):
            importlib.reload(cp)
            yield d


@contextlib.contextmanager
def quiet():
    """Swallow command output; tests assert on the filesystem, not on prose."""
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf):
        yield buf


def exits(fn, *a, **kw) -> bool:
    try:
        fn(*a, **kw)
    except SystemExit:
        return True
    return False


def cfg_of(d: Path, tool: str, name: str) -> Path:
    return d / "prof" / tool / name / "config"


# --- input validation ------------------------------------------------------

def test_name_validation_rejects_traversal():
    with sandbox(), quiet():
        for bad in ("..", ".", "", "a/b", "x/../y", "../../etc", "a b", "-x/.."):
            assert exits(cp.config_dir, cp.TOOLS["claude"], bad), f"accepted {bad!r}"
        ok = cp.config_dir(cp.TOOLS["claude"], "work-1_X")
        assert ok.parent.name == "work-1_X"


def test_requires_claude_config_dir():
    with sandbox(claude_env=False) as d, quiet():
        assert exits(cp.main, ["init"])
        assert not (d / "prof").exists(), "must not create anything"


def test_rejects_env_var_pointing_inside_profile_root():
    with sandbox() as d, quiet():
        os.environ["CLAUDE_CONFIG_DIR"] = str(cfg_of(d, "claude", "work"))
        assert exits(cp.require_link, cp.TOOLS["claude"])


# --- the atomic swap -------------------------------------------------------

def test_relink_replaces_existing_symlink_without_debris():
    with sandbox() as d:
        a, b, link = d / "a", d / "b", d / "link"
        a.mkdir(); b.mkdir()
        cp.relink(link, a)
        assert os.readlink(link) == str(a)
        cp.relink(link, b)
        assert os.readlink(link) == str(b)
        assert link.is_symlink() and link.is_dir()
        assert not list(d.glob(".*ccp*")), "temp link left behind"


def test_relink_refuses_to_clobber_a_real_directory():
    with sandbox() as d:
        real, target = d / "real", d / "target"
        real.mkdir(); target.mkdir()
        (real / "keep").write_text("precious")
        try:
            cp.relink(real, target)
            assert False, "should have raised"
        except OSError:
            pass
        assert (real / "keep").read_text() == "precious"
        assert not real.is_symlink()


# --- init / reinit ---------------------------------------------------------

def test_init_creates_default_and_links_both_tools():
    with sandbox() as d, quiet():
        assert cp.main(["init"]) == 0
        for tool, link in (("claude", d / ".claude"), ("codex", d / ".codex")):
            assert link.is_symlink()
            assert link.resolve() == cfg_of(d, tool, "default").resolve()
            assert cp.current(cp.TOOLS[tool]) == "default"


def test_init_refuses_real_dir_and_leaves_it_intact():
    with sandbox() as d, quiet() as out:
        (d / ".codex").mkdir()
        (d / ".codex" / "auth.json").write_text("secret")
        assert cp.main(["init"]) == 1
        assert not (d / ".codex").is_symlink()
        assert (d / ".codex" / "auth.json").read_text() == "secret"
        assert "migrate codex" in out.getvalue()
        # the other tool still made progress
        assert (d / ".claude").is_symlink()


def test_reinit_repairs_a_dangling_link():
    with sandbox() as d, quiet():
        cp.main(["init"])
        cp.main(["new", "claude", "work"])
        cp.main(["switch", "claude", "work"])
        shutil.rmtree(d / "prof" / "claude" / "work")
        assert not (d / ".claude").resolve().exists()
        assert cp.main(["reinit"]) == 0
        assert (d / ".claude").resolve().is_dir()


# --- migrate ---------------------------------------------------------------

def test_migrate_moves_dir_and_links():
    with sandbox() as d, quiet():
        (d / ".codex").mkdir()
        (d / ".codex" / "auth.json").write_text("secret")
        assert cp.main(["migrate", "codex", "default"]) == 0
        cfg = cfg_of(d, "codex", "default")
        assert (cfg / "auth.json").read_text() == "secret"
        assert (d / ".codex").is_symlink()
        assert (d / ".codex").resolve() == cfg.resolve()


def test_migrate_folds_in_outer_claude_json():
    """Machine that never set CLAUDE_CONFIG_DIR: the live file is in $HOME."""
    with sandbox() as d, quiet():
        (d / ".claude").mkdir()
        (d / ".claude" / "settings.json").write_text('{"a":1}')
        (d / ".claude.json").write_text(json.dumps(
            {"projects": {"/x": {}, "/y": {}}, "userID": "u1"}))
        assert cp.main(["migrate", "claude", "default"]) == 0
        cfg = cfg_of(d, "claude", "default")
        assert (cfg / "settings.json").read_text() == '{"a":1}'
        moved = json.loads((cfg / ".claude.json").read_text())
        assert len(moved["projects"]) == 2 and moved["userID"] == "u1"
        assert not (d / ".claude.json").exists(), "outer should be consumed"


def test_migrate_keeps_live_inner_claude_json_over_orphan():
    """Both present: inner is live, outer is an updater orphan. Never swap them."""
    with sandbox() as d, quiet():
        (d / ".claude").mkdir()
        (d / ".claude" / ".claude.json").write_text(json.dumps(
            {"projects": {"/a": {}}, "marker": "live"}))
        (d / ".claude.json").write_text(json.dumps({"installMethod": "native"}))
        assert cp.main(["migrate", "claude", "default"]) == 0
        inner = json.loads((cfg_of(d, "claude", "default") / ".claude.json").read_text())
        assert inner["marker"] == "live", "live file must survive"
        assert (d / ".claude.json").exists(), "orphan must not be deleted"


def test_migrate_rolls_back_partial_copy_on_failure():
    """A cross-device move can die mid-copy; leave nothing behind but the source."""
    with sandbox() as d, quiet():
        (d / ".codex").mkdir()
        (d / ".codex" / "auth.json").write_text("secret")
        def half_copied(src, dst):
            """What shutil.move really leaves behind when copytree dies."""
            Path(dst).mkdir(parents=True, exist_ok=True)
            (Path(dst) / "partial.jsonl").write_text("x" * 100)
            raise OSError("no space left on device")

        with mock.patch.object(cp.shutil, "move", half_copied):
            assert exits(cp.main, ["migrate", "codex", "default"])
        assert (d / ".codex" / "auth.json").read_text() == "secret", "source intact"
        assert not (d / ".codex").is_symlink(), "must not link to a failed copy"
        assert not cfg_of(d, "codex", "default").exists(), "partial copy left behind"


def test_migrate_refuses_existing_profile():
    with sandbox() as d, quiet():
        (d / ".codex").mkdir()
        cp.main(["new", "codex", "taken"])
        assert exits(cp.main, ["migrate", "codex", "taken"])
        assert (d / ".codex").is_dir() and not (d / ".codex").is_symlink()


# --- switch ----------------------------------------------------------------

def test_switch_repoints_and_link_is_never_absent():
    with sandbox() as d, quiet():
        cp.main(["init"])
        cp.main(["new", "claude", "work"])
        assert cp.main(["switch", "claude", "work"]) == 0
        assert (d / ".claude").is_symlink()
        assert cp.current(cp.TOOLS["claude"]) == "work"
        cp.main(["switch", "claude", "default"])
        assert cp.current(cp.TOOLS["claude"]) == "default"


def test_switch_refuses_missing_profile_and_real_dir():
    with sandbox() as d, quiet():
        cp.main(["init"])
        assert exits(cp.main, ["switch", "claude", "ghost"])
        (d / ".claude").unlink()
        (d / ".claude").mkdir()
        (d / ".claude" / "keep").write_text("x")
        cp.main(["new", "claude", "work"])
        assert exits(cp.main, ["switch", "claude", "work"])
        assert (d / ".claude" / "keep").read_text() == "x"


def test_current_is_none_for_link_outside_profile_root():
    with sandbox() as d, quiet():
        elsewhere = d / "elsewhere"
        elsewhere.mkdir()
        os.symlink(elsewhere, d / ".codex")
        assert cp.current(cp.TOOLS["codex"]) is None
        assert cp.cmd_status() == 0  # must not crash


# --- run -------------------------------------------------------------------

def test_run_sets_env_and_execs():
    with sandbox() as d, quiet():
        cp.main(["new", "codex", "work"])
        with mock.patch.object(os, "execvp") as ex:
            cp.main(["run", "codex", "work", "exec", "--json"])
        assert ex.call_args[0][1] == ["codex", "exec", "--json"]
        assert os.environ["CODEX_HOME"] == str(cfg_of(d, "codex", "work"))


def test_run_passes_flags_through_untouched():
    with sandbox() as d, quiet():
        cp.main(["new", "claude", "work"])
        for args, want in (
            (["run", "claude", "work"], ["claude"]),
            (["run", "claude", "work", "-p", "hi"], ["claude", "-p", "hi"]),
            (["run", "claude", "work", "--", "--help"], ["claude", "--help"]),
            (["run", "claude", "work", "--help"], ["claude", "--help"]),
        ):
            with mock.patch.object(os, "execvp") as ex:
                cp.main(args)
            assert ex.call_args[0][1] == want, f"{args} -> {ex.call_args[0][1]}"


def test_run_leaves_the_active_profile_alone():
    with sandbox() as d, quiet():
        cp.main(["init"])
        cp.main(["new", "claude", "work"])
        before = os.readlink(d / ".claude")
        with mock.patch.object(os, "execvp"):
            cp.main(["run", "claude", "work"])
        assert os.readlink(d / ".claude") == before


def test_run_rejects_missing_profile_before_exec():
    with sandbox(), quiet():
        with mock.patch.object(os, "execvp") as ex:
            assert exits(cp.main, ["run", "claude", "ghost"])
        assert not ex.called


# --- invariants ------------------------------------------------------------

def test_no_metadata_files_are_written():
    """State is derived from readlink/listdir; nothing else may appear."""
    with sandbox() as d, quiet():
        cp.main(["init"])
        cp.main(["new", "claude", "work"])
        cp.main(["switch", "claude", "work"])
        root = d / "prof"
        allowed = {(t, n, "config") for t in ("claude", "codex")
                   for n in ("default", "work")}
        for p in root.rglob("*"):
            parts = p.relative_to(root).parts
            assert p.is_dir(), f"unexpected file {p}"
            assert len(parts) < 3 or parts in allowed, f"unexpected path {parts}"


def main() -> int:
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    failed = []
    for t in tests:
        try:
            t()
            print(f"  ok    {t.__name__}")
        except Exception as e:  # noqa: BLE001 - report and keep going
            failed.append(t.__name__)
            print(f"  FAIL  {t.__name__}: {type(e).__name__}: {e}")
    print(f"\n{len(tests) - len(failed)}/{len(tests)} passed")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
