#!/usr/bin/env python3
"""Self-contained checks for cc_profile.

Run:  python test_ccp.py     (also collectable by pytest)

Every test builds an isolated $HOME and profile root, so nothing here can
reach the real ~/.claude or ~/.codex.
"""
from __future__ import annotations

import contextlib
import errno
import importlib
import io
import json
import os
import shutil
import sys
import tempfile
import time
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


def bootstrap(*tools: str, name: str = "work") -> None:
    """Create and select a profile. init no longer invents one."""
    for t in tools or ("claude", "codex"):
        cp.main(["new", t, name])
        cp.main(["switch", t, name])


def xdev() -> OSError:
    """Force the cross-device path that rename cannot take."""
    return OSError(errno.EXDEV, "Invalid cross-device link")


# --- input validation ------------------------------------------------------

def test_name_validation_rejects_traversal():
    with sandbox(), quiet():
        for bad in ("..", ".", "", "a/b", "x/../y", "../../etc", "a b", "-x/.."):
            assert exits(cp.config_dir, cp.DIMS["claude"], bad), f"accepted {bad!r}"
        ok = cp.config_dir(cp.DIMS["claude"], "work-1_X")
        assert ok.parent.name == "work-1_X"


def test_requires_claude_config_dir():
    """Claude is reported and skipped; codex still makes progress."""
    with sandbox(claude_env=False) as d, quiet() as out:
        cp.main(["new", "codex", "work"])
        assert cp.main(["init"]) == 1
        assert "CLAUDE_CONFIG_DIR" in out.getvalue()
        assert not (d / "prof" / "claude").exists(), "must not set claude up"
        assert (d / ".codex").is_symlink(), "the other tool still proceeds"


def test_switch_requires_the_env_var_before_touching_anything():
    with sandbox(claude_env=False) as d, quiet():
        (d / "prof" / "claude" / "work" / "config").mkdir(parents=True)
        assert exits(cp.main, ["switch", "claude", "work"])
        assert not (d / "prof" / "claude" / "current").exists(), "must not move first"


def test_rejects_env_var_pointing_inside_profile_root():
    with sandbox() as d, quiet():
        os.environ["CLAUDE_CONFIG_DIR"] = str(cfg_of(d, "claude", "work"))
        assert exits(cp.require_link, cp.DIMS["claude"])


# --- the atomic swap -------------------------------------------------------

def test_relink_replaces_existing_symlink_without_debris():
    with sandbox() as d:
        a, b, link = d / "a", d / "b", d / "link"
        a.mkdir(); b.mkdir()
        cp.relink(link, a)
        assert cp.link_target(link) == a
        cp.relink(link, b)
        assert cp.link_target(link) == b
        assert link.is_symlink() and link.is_dir()
        assert not list(d.glob(".*ccp*")), "temp link left behind"
        assert not os.readlink(link).startswith("/"), "links must be relative"


def test_links_are_relative_so_the_root_can_be_moved():
    with sandbox() as d, quiet():
        _with_history(d)
        cp.main(["new", "claude", "other"])
        moved = d / "relocated"
        (d / "prof").rename(moved)
        # Everything inside the tree still resolves; only the outer link,
        # which necessarily names an absolute location, needs repointing.
        assert (moved / "claude" / "current").resolve().name == "work"
        chat = moved / "claude" / "work" / "config" / "projects" / "-repo" / "a.jsonl"
        assert chat.resolve().read_text() == "chat"


def test_links_into_a_config_dir_whose_parent_is_missing():
    """A custom CLAUDE_CONFIG_DIR can nest below directories that do not exist."""
    with sandbox(CLAUDE_CONFIG_DIR="") as d, quiet():
        os.environ["CLAUDE_CONFIG_DIR"] = str(d / "nested" / "deeper" / "claude")
        cp.main(["new", "claude", "work"])
        cp.main(["switch", "claude", "work"])
        link = d / "nested" / "deeper" / "claude"
        assert link.is_symlink()
        assert link.resolve() == cfg_of(d, "claude", "work").resolve()


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

def test_init_invents_no_profile_and_says_what_to_run():
    with sandbox() as d, quiet() as out:
        assert cp.main(["init"]) == 1
        assert not (d / "prof").exists(), "must create nothing"
        assert "migrate claude" in out.getvalue()


def test_init_selects_a_sole_profile_but_not_one_of_several():
    with sandbox() as d, quiet() as out:
        cp.main(["new", "claude", "onlyone"])
        assert cp.main(["init"]) == 1          # codex still has none
        assert cp.current(cp.DIMS["claude"]) == "onlyone", "sole entry is unambiguous"
        assert (d / ".claude").is_symlink()
        cp.main(["new", "claude", "second"])
        (d / "prof" / "claude" / "current").unlink()
        assert cp.main(["init"]) == 1
        assert "switch claude" in out.getvalue(), "must ask rather than guess"
        assert cp.current(cp.DIMS["claude"]) is None


def test_init_refuses_real_dir_and_leaves_it_intact():
    with sandbox() as d, quiet() as out:
        (d / ".codex").mkdir()
        (d / ".codex" / "auth.json").write_text("secret")
        bootstrap("claude")
        assert cp.main(["init"]) == 1
        assert not (d / ".codex").is_symlink()
        assert (d / ".codex" / "auth.json").read_text() == "secret"
        assert "migrate codex" in out.getvalue()
        # the other tool still made progress
        assert (d / ".claude").is_symlink()


def test_reinit_repairs_a_dangling_link():
    with sandbox() as d, quiet():
        bootstrap("claude")
        cp.main(["new", "claude", "spare"])
        shutil.rmtree(d / "prof" / "claude" / "work")
        assert not (d / ".claude").resolve().exists()
        assert cp.main(["reinit"]) == 1        # codex has no profile
        assert cp.current(cp.DIMS["claude"]) == "spare"
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
        def half_copied(src, dst, **kw):
            """What copytree leaves behind when it dies partway."""
            Path(dst).mkdir(parents=True, exist_ok=True)
            (Path(dst) / "partial.jsonl").write_text("x" * 100)
            raise OSError(errno.ENOSPC, "no space left on device")

        with mock.patch.object(cp.os, "rename", side_effect=xdev()), \
             mock.patch.object(cp.shutil, "copytree", half_copied):
            assert exits(cp.main, ["migrate", "codex", "default"])
        assert (d / ".codex" / "auth.json").read_text() == "secret", "source intact"
        assert not (d / ".codex").is_symlink(), "must not link to a failed copy"
        assert not cfg_of(d, "codex", "default").exists(), "partial copy left behind"


def test_migrate_keeps_the_copy_when_source_removal_fails():
    """Copy done, delete failed: the destination is now the only complete copy.

    Removing it here - the natural "roll back on error" reflex - would destroy
    the data outright, since the source is already partly gone.
    """
    with sandbox() as d, quiet():
        (d / ".codex").mkdir()
        (d / ".codex" / "auth.json").write_text("secret")
        real_rmtree = cp.shutil.rmtree

        def rmtree(p, *a, **kw):
            if Path(p) == d / ".codex":
                raise OSError(errno.EACCES, "permission denied")
            return real_rmtree(p, *a, **kw)

        with mock.patch.object(cp.os, "rename", side_effect=xdev()), \
             mock.patch.object(cp.shutil, "rmtree", rmtree):
            assert exits(cp.main, ["migrate", "codex", "default"])
        copied = cfg_of(d, "codex", "default") / "auth.json"
        assert copied.read_text() == "secret", "the complete copy must be kept"


def test_migrate_refuses_existing_profile():
    with sandbox() as d, quiet():
        (d / ".codex").mkdir()
        cp.main(["new", "codex", "taken"])
        assert exits(cp.main, ["migrate", "codex", "taken"])
        assert (d / ".codex").is_dir() and not (d / ".codex").is_symlink()


# --- switch ----------------------------------------------------------------

def test_switch_repoints_and_link_is_never_absent():
    with sandbox() as d, quiet():
        bootstrap()
        cp.main(["new", "claude", "spare"])
        assert cp.main(["switch", "claude", "spare"]) == 0
        assert (d / ".claude").is_symlink()
        assert cp.current(cp.DIMS["claude"]) == "spare"
        cp.main(["switch", "claude", "work"])
        assert cp.current(cp.DIMS["claude"]) == "work"


def test_switch_refuses_missing_profile_and_real_dir():
    with sandbox() as d, quiet():
        bootstrap()
        assert exits(cp.main, ["switch", "claude", "ghost"])
        (d / ".claude").unlink()
        (d / ".claude").mkdir()
        (d / ".claude" / "keep").write_text("x")
        cp.main(["new", "claude", "spare"])
        assert exits(cp.main, ["switch", "claude", "spare"])
        assert (d / ".claude" / "keep").read_text() == "x"


def test_current_is_none_for_link_outside_profile_root():
    with sandbox() as d, quiet():
        elsewhere = d / "elsewhere"
        elsewhere.mkdir()
        os.symlink(elsewhere, d / ".codex")
        assert cp.current(cp.DIMS["codex"]) is None
        assert cp.cmd_status() == 0  # must not crash


# --- rename ----------------------------------------------------------------

def test_rename_follows_the_active_profile():
    with sandbox() as d, quiet():
        bootstrap()
        assert cp.main(["rename", "claude", "work", "personal"]) == 0
        assert cp.current(cp.DIMS["claude"]) == "personal"
        assert (d / ".claude").resolve() == cfg_of(d, "claude", "personal").resolve()
        assert not (d / "prof" / "claude" / "work").exists()


def test_rename_moves_contents_and_leaves_inactive_links_alone():
    with sandbox() as d, quiet():
        bootstrap()
        cp.main(["new", "claude", "spare"])
        (cfg_of(d, "claude", "spare") / "settings.json").write_text('{"k":1}')
        before = os.readlink(d / ".claude")
        assert cp.main(["rename", "claude", "spare", "backup"]) == 0
        assert (cfg_of(d, "claude", "backup") / "settings.json").read_text() == '{"k":1}'
        assert os.readlink(d / ".claude") == before, "active link must not move"


def test_rename_follows_the_active_history_set():
    with sandbox() as d, quiet():
        _with_history(d)
        assert cp.main(["rename", "claude-history", "main", "archive"]) == 0
        assert cp.current(cp.DIMS["claude-history"]) == "archive"
        # profiles link by way of `current`, so they need no adjustment
        assert (d / ".claude" / "projects" / "-repo" / "a.jsonl").read_text() == "chat"


def test_rename_refuses_collisions_bad_names_and_missing_entries():
    with sandbox(), quiet():
        bootstrap()
        cp.main(["new", "claude", "other"])
        assert exits(cp.main, ["rename", "claude", "work", "other"])   # exists
        assert exits(cp.main, ["rename", "claude", "ghost", "x"])      # missing
        assert exits(cp.main, ["rename", "claude", "work", "a/b"])     # traversal
        assert exits(cp.main, ["rename", "claude", "work", "current"]) # reserved
        assert exits(cp.main, ["rename", "claude", "work", "work"])    # same
        assert cp.current(cp.DIMS["claude"]) == "work", "nothing moved"


# --- history as an independent dimension -----------------------------------

def hist_of(d: Path, tool: str, name: str) -> Path:
    return d / "prof" / f"{tool}-history" / name


def _with_history(d: Path):
    """A claude profile 'work' whose transcripts have been lifted into 'main'."""
    (d / ".claude" / "projects" / "-repo").mkdir(parents=True)
    (d / ".claude" / "projects" / "-repo" / "a.jsonl").write_text("chat")
    cp.main(["migrate", "claude", "work"])
    cp.main(["migrate", "claude-history", "main"])


def test_history_survives_a_profile_switch():
    """The whole point: transcripts follow the work, not the account."""
    with sandbox() as d, quiet():
        _with_history(d)
        cp.main(["new", "claude", "personal"])
        seen = d / ".claude" / "projects" / "-repo" / "a.jsonl"
        assert seen.read_text() == "chat"
        cp.main(["switch", "claude", "personal"])
        assert cp.current(cp.DIMS["claude"]) == "personal"
        assert seen.read_text() == "chat", "history must follow across accounts"


def test_history_switch_changes_transcripts_but_not_the_profile():
    with sandbox() as d, quiet():
        _with_history(d)
        cp.main(["new", "claude-history", "clean"])
        before = cp.current(cp.DIMS["claude"])
        cp.main(["switch", "claude-history", "clean"])
        assert cp.current(cp.DIMS["claude-history"]) == "clean"
        assert cp.current(cp.DIMS["claude"]) == before, "profile must not move"
        assert not (d / ".claude" / "projects" / "-repo").exists()
        cp.main(["switch", "claude-history", "main"])
        assert (d / ".claude" / "projects" / "-repo" / "a.jsonl").read_text() == "chat"


def test_new_profile_joins_the_shared_history():
    with sandbox() as d, quiet():
        _with_history(d)
        cp.main(["new", "claude", "later"])
        link = cfg_of(d, "claude", "later") / "projects"
        assert link.is_symlink()
        assert link.resolve() == (hist_of(d, "claude", "main") / "projects").resolve()


def test_new_profile_gets_a_real_dir_when_history_is_not_enabled():
    with sandbox() as d, quiet():
        bootstrap()
        cp.main(["new", "claude", "solo"])
        assert not (cfg_of(d, "claude", "solo") / "projects").is_symlink()


def test_migrate_history_never_clobbers_another_profiles_transcripts():
    """Only the active profile donates; the rest keep theirs and are reported."""
    with sandbox() as d, quiet() as out:
        (d / ".claude" / "projects").mkdir(parents=True)
        (d / ".claude" / "projects" / "mine.jsonl").write_text("active")
        cp.main(["migrate", "claude", "work"])
        other = cfg_of(d, "claude", "other") / "projects"
        other.mkdir(parents=True)
        (other / "theirs.jsonl").write_text("precious")
        cp.main(["migrate", "claude-history", "main"])
        assert (other / "theirs.jsonl").read_text() == "precious"
        assert not other.is_symlink(), "must not be relinked over real data"
        assert "other" in out.getvalue()


def test_migrate_history_needs_an_active_profile():
    with sandbox(), quiet():
        assert exits(cp.main, ["migrate", "claude-history", "main"])


def test_current_is_a_reserved_name():
    with sandbox(), quiet():
        assert exits(cp.main, ["new", "claude-history", "current"])


def test_run_rejects_a_history_set():
    with sandbox(), quiet():
        with mock.patch.object(os, "execvp") as ex:
            assert exits(cp.main, ["run", "claude-history", "main"])
        assert not ex.called


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
        bootstrap()
        cp.main(["new", "claude", "spare"])
        before = os.readlink(d / ".claude")
        with mock.patch.object(os, "execvp"):
            cp.main(["run", "claude", "spare"])
        assert os.readlink(d / ".claude") == before


def test_run_rejects_missing_profile_before_exec():
    with sandbox(), quiet():
        with mock.patch.object(os, "execvp") as ex:
            assert exits(cp.main, ["run", "claude", "ghost"])
        assert not ex.called


# --- usage -----------------------------------------------------------------

CLAUDE_USAGE_BODY = {
    "five_hour": {"utilization": 29.0, "resets_at": "2099-01-01T00:00:00+00:00"},
    "seven_day": {"utilization": 55.0, "resets_at": "2099-01-08T00:00:00+00:00"},
    "seven_day_opus": None,
}
CODEX_USAGE_BODY = {
    "plan_type": "pro",
    "rate_limit": {"primary_window": {"used_percent": 14,
                                      "limit_window_seconds": 604800,
                                      "reset_after_seconds": 452520},
                   "secondary_window": None},
    "additional_rate_limits": [],
}


def _creds(d: Path, name: str, expires_in_h: float = 6) -> None:
    cfg = cfg_of(d, "claude", name)
    cfg.mkdir(parents=True, exist_ok=True)
    (cfg / ".credentials.json").write_text(json.dumps({"claudeAiOauth": {
        "accessToken": "tok", "refreshToken": "rt",
        "expiresAt": int((time.time() + expires_in_h * 3600) * 1000)}}))


def test_usage_reports_each_profiles_windows():
    with sandbox() as d, quiet() as out:
        _creds(d, "work")
        cfg = cfg_of(d, "codex", "work"); cfg.mkdir(parents=True)
        (cfg / "auth.json").write_text(json.dumps({"tokens": {"access_token": "a.b.c"}}))
        with mock.patch.object(cp, "_get", lambda url, h: (
                200, CLAUDE_USAGE_BODY if "anthropic" in url else CODEX_USAGE_BODY)):
            assert cp.main(["usage"]) == 0
        text = out.getvalue()
        assert "5h 29%" in text and "7d 55%" in text, text
        assert "7d 14%" in text and "(pro)" in text, text


def test_usage_never_sends_an_expired_token():
    """Expiry is read locally: the usage endpoint is only reached once renewed."""
    with sandbox() as d, quiet() as out:
        _creds(d, "stale", expires_in_h=-1)
        with mock.patch.object(cp, "_post", lambda *a, **k: (503, {})), \
             mock.patch.object(cp, "_get") as g:
            cp.main(["usage", "claude"])
        assert not g.called, "must not call out with an expired token"
        assert "refresh failed" in out.getvalue(), out.getvalue()


def test_refresh_renews_in_place_and_usage_retries():
    with sandbox() as d, quiet() as out:
        _creds(d, "stale", expires_in_h=-1)
        path = cfg_of(d, "claude", "stale") / ".credentials.json"

        def fake_post(url, data, form, agent=""):
            assert url == cp.CLAUDE_TOKEN_URL and form is True
            assert data["grant_type"] == "refresh_token" and data["refresh_token"] == "rt"
            # Cloudflare returns 403 to an unrecognised agent string, so the
            # refresh must carry the same one the usage request does.
            assert agent == cp.CLAUDE_UA, f"missing user agent: {agent!r}"
            return 200, {"access_token": "NEW", "refresh_token": "RT2", "expires_in": 3600}

        with mock.patch.object(cp, "_post", fake_post), \
             mock.patch.object(cp, "_get", lambda url, h: (200, CLAUDE_USAGE_BODY)):
            assert cp.main(["usage", "claude"]) == 0
        saved = json.loads(path.read_text())["claudeAiOauth"]
        assert saved["accessToken"] == "NEW" and saved["refreshToken"] == "RT2"
        assert saved["expiresAt"] / 1000 > time.time()
        assert "5h 29%" in out.getvalue(), out.getvalue()


def test_refresh_keeps_the_old_refresh_token_when_none_is_returned():
    """Rotation is optional; dropping it would lock the profile out."""
    with sandbox() as d, quiet():
        _creds(d, "stale", expires_in_h=-1)
        path = cfg_of(d, "claude", "stale") / ".credentials.json"
        with mock.patch.object(cp, "_post",
                               lambda *a, **k: (200, {"access_token": "N", "expires_in": 60})), \
             mock.patch.object(cp, "_get", lambda url, h: (200, CLAUDE_USAGE_BODY)):
            cp.main(["usage", "claude"])
        assert json.loads(path.read_text())["claudeAiOauth"]["refreshToken"] == "rt"


def test_refresh_leaves_credentials_alone_when_it_fails():
    with sandbox() as d, quiet() as out:
        _creds(d, "stale", expires_in_h=-1)
        path = cfg_of(d, "claude", "stale") / ".credentials.json"
        before = path.read_text()
        for status, body, expect in ((400, {"error": "invalid_grant"}, "login expired"),
                                     (503, {}, "refresh failed: HTTP 503"),
                                     (None, {"error": "timed out"}, "unreachable")):
            with mock.patch.object(cp, "_post", lambda *a, s=status, b=body, **k: (s, b)), \
                 mock.patch.object(cp, "_get") as g:
                cp.main(["usage", "claude"])
            assert path.read_text() == before, f"{status} must not rewrite credentials"
            assert not g.called
            assert expect in out.getvalue(), (expect, out.getvalue())


def test_secret_writes_are_atomic_and_private():
    with sandbox() as d:
        p = d / "secret.json"
        cp._write_secret(p, {"a": 1})
        assert json.loads(p.read_text()) == {"a": 1}
        assert oct(p.stat().st_mode)[-3:] == "600", oct(p.stat().st_mode)
        assert not list(d.glob(".*ccp*")), "temp file left behind"


def test_usage_reports_a_lapsed_refresh_token_as_needing_login():
    """Nothing can renew it, so do not suggest a nudge that cannot work."""
    with sandbox() as d, quiet() as out:
        cfg = cfg_of(d, "claude", "old"); cfg.mkdir(parents=True)
        past = int((time.time() - 86400) * 1000)
        (cfg / ".credentials.json").write_text(json.dumps({"claudeAiOauth": {
            "accessToken": "t", "expiresAt": past, "refreshTokenExpiresAt": past}}))
        with mock.patch.object(cp, "_post") as post:
            cp.main(["usage", "claude"])
        assert not post.called, "no request can renew a dead login"
        assert "login expired" in out.getvalue(), out.getvalue()


def test_usage_refreshes_at_most_once():
    """A refresh that reports success but changes nothing must not loop."""
    with sandbox() as d, quiet() as out:
        _creds(d, "dead", expires_in_h=-1)
        calls = []
        with mock.patch.object(cp, "_post",
                               lambda *a, **k: calls.append(a) or (200, {"access_token": "N",
                                                                        "expires_in": -99})), \
             mock.patch.object(cp, "_get") as g:
            cp.main(["usage", "claude"])
        assert len(calls) == 1, f"one attempt only, got {len(calls)}"
        assert not g.called, "still expired, so nothing goes on the wire"
        assert "still expired" in out.getvalue(), out.getvalue()


def test_codex_refresh_posts_json_and_preserves_account_id():
    with sandbox() as d, quiet():
        cfg = cfg_of(d, "codex", "stale"); cfg.mkdir(parents=True)
        dead = f"h.{cp.base64.urlsafe_b64encode(b'{\"exp\":1}').decode().rstrip('=')}.s"
        (cfg / "auth.json").write_text(json.dumps(
            {"auth_mode": "chatgpt",
             "tokens": {"access_token": dead, "refresh_token": "rt", "account_id": "acc-1"}}))

        def fake_post(url, data, form, agent=""):
            assert url == cp.CODEX_TOKEN_URL and form is False, (url, form)
            assert data["client_id"] == cp.CODEX_CLIENT_ID
            return 200, {"access_token": "NEW", "id_token": "ID2"}

        with mock.patch.object(cp, "_post", fake_post), \
             mock.patch.object(cp, "_get", lambda url, h: (200, CODEX_USAGE_BODY)):
            cp.main(["usage", "codex"])
        saved = json.loads((cfg / "auth.json").read_text())
        assert saved["tokens"]["access_token"] == "NEW"
        assert saved["tokens"]["refresh_token"] == "rt", "kept when not rotated"
        assert saved["tokens"]["account_id"] == "acc-1", "untouched fields survive"
        assert saved["auth_mode"] == "chatgpt"


def test_usage_reports_missing_credentials_and_http_errors():
    with sandbox() as d, quiet() as out:
        cfg_of(d, "claude", "empty").mkdir(parents=True)
        _creds(d, "denied")
        with mock.patch.object(cp, "_get", lambda url, h: (401, None)):
            cp.main(["usage", "claude"])
        text = out.getvalue()
        assert "not logged in" in text and "unauthorized" in text, text


def test_usage_writes_nothing():
    with sandbox() as d, quiet():
        _creds(d, "work")
        before = sorted(p.relative_to(d) for p in d.rglob("*"))
        stamps = {p: p.stat().st_mtime for p in d.rglob("*") if p.is_file()}
        with mock.patch.object(cp, "_get", lambda url, h: (200, CLAUDE_USAGE_BODY)):
            cp.main(["usage"])
        assert sorted(p.relative_to(d) for p in d.rglob("*")) == before
        assert {p: p.stat().st_mtime for p in d.rglob("*") if p.is_file()} == stamps


# --- invariants ------------------------------------------------------------

def test_no_metadata_files_are_written():
    """State is derived from readlink/listdir; nothing else may appear."""
    with sandbox() as d, quiet():
        bootstrap()
        cp.main(["new", "claude", "spare"])
        root = d / "prof"
        allowed = {(t, n, "config") for t in ("claude", "codex")
                   for n in ("work", "spare")}
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
        # BaseException: a stray die() raises SystemExit, which must fail the
        # test rather than silently end the whole run.
        except BaseException as e:  # noqa: BLE001 - report and keep going
            failed.append(t.__name__)
            print(f"  FAIL  {t.__name__}: {type(e).__name__}: {e}")
    print(f"\n{len(tests) - len(failed)}/{len(tests)} passed")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
