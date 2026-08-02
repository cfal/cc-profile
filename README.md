# cc-profile (`ccp`)

Switch Claude Code and Codex between named profiles — separate accounts, MCP servers
and settings — with chat history as a **second, independent** dimension.

Both CLIs keep all their state in one config directory and honour an environment
variable pointing at it. `ccp` makes that path a symlink and repoints it, so switching
is instant regardless of profile size. Nothing is copied.

## Install

Symlink the script onto your PATH:

```sh
ln -s "$PWD/cc_profile.py" ~/.local/bin/ccp
```

That is the whole install. It is one stdlib-only file with a `#!/usr/bin/env python3`
shebang, so there is no package, no virtualenv and no build step — it runs from the
checkout and picks up edits immediately. Python 3.8 or newer.

Then add this to your shell rc — `ccp` requires it and will tell you so:

```sh
export CLAUDE_CONFIG_DIR="$HOME/.claude"
```

Claude splits its state between the config directory and a `.claude.json` holding
`projects`, `userID` and `oauthAccount`. With the variable unset that file is written
*beside* the config directory, so profiles would keep sharing account state. Setting it
moves the file inside, where the symlink covers it.

`CODEX_HOME` is optional and defaults to `~/.codex`; Codex keeps nothing outside its
directory.

## First run

You almost certainly have real config directories already. Move them into a profile —
this prints every path it touches, and the command that undoes it:

```sh
ccp migrate claude default
ccp migrate codex default
```

`migrate` moves *and* links in one step; there is no `init` to run afterwards. Use
`ccp init` only on a machine with nothing to migrate.

## Use

```
ccp                            everything, active entries marked
ccp list                       same thing
ccp init                       create the config-dir symlinks
ccp reinit                     recreate/repair the symlinks
ccp new     <what> <name>      create an empty profile or history set
ccp migrate <what> <name>      move existing real dirs into one
ccp switch  <what> <name>      point the symlink at <name>
ccp rename  <what> <name> <new>  rename one, following it if active
ccp run     <tool> <name> [--] [args...]
```

`<what>` is `claude`, `codex`, `claude-history` or `codex-history`.

## Two dimensions

A **config profile** holds the account, settings and MCP config. A **history set** holds
the transcripts `--resume` reads — Claude's `projects/`, Codex's `sessions/`. They switch
independently, so the same chats can follow you across accounts, or you can keep a
separate set for a clean room:

```sh
ccp migrate claude-history main    # lift the active profile's transcripts out
ccp switch  claude personal        # different account, same chats
ccp new     claude-history demo
ccp switch  claude-history demo    # same account, empty history
```

Every profile points at one `current` link, so switching history is a single atomic
repoint rather than one per profile. New profiles join the shared set automatically;
a profile that already holds its own transcripts is never relinked over and is
reported instead.

History sets are directories only. Codex's `logs_*.sqlite` and `memories_*.sqlite` stay
with the profile — SQLite puts `-wal`/`-shm` beside the path it opened, so symlinking a
database would strand its journal.

```console
$ ccp
~/.config/cc-profile

claude          ~/.claude -> work
     personal
   * work
claude-history  main
     demo
   * main

codex           ~/.codex -> default
   * default
codex-history   not enabled - sessions/ lives inside each profile
                share it:  ccp migrate codex-history <name>

$ ccp switch claude personal
claude: work -> personal
  ~/.claude -> ~/.config/cc-profile/claude/personal/config

$ ccp run codex personal exec 'summarise this repo'   # one invocation, no switch
```

`run` sets the environment variable for a single invocation and `exec`s. It touches no
symlink, so it cannot race with `switch` and concurrent runs under different profiles
are safe. Everything after `<name>` goes to the child untouched — `ccp run claude work
--help` prints Claude's help, not ours.

A new profile is empty, so the tool onboards from scratch and you log in again. There is
no `clone`.

## Layout

```
~/.config/cc-profile/                     override with CC_PROFILE_DIR
├── claude/<name>/config/                 <- $CLAUDE_CONFIG_DIR points here
│                    └── projects ────────┐  (when history is enabled)
├── codex/<name>/config/                  │  <- $CODEX_HOME points here
└── claude-history/                       │
    ├── current -> main  <────────────────┘  switch repoints this one link
    └── main/projects/                       the real transcripts
```

Each store is named for the key you type, so `ccp switch claude-history main` acts on
`~/.config/cc-profile/claude-history/main`.

No index, lockfile or state file. The active profile is `readlink()` on the config path
and the profile list is `listdir()`, so nothing can fall out of sync — and you can move,
rename or delete a profile directory by hand.

## Caveats

**macOS switches everything except the account.** Claude Code stores credentials in the
Keychain there rather than in the config directory, so sessions, projects, settings and
MCP config follow the profile but the login does not. On Linux credentials live in
`~/.claude/.credentials.json`, inside the directory, and account switching works. Codex
is file-backed by default on both, unless `cli_auth_credentials_store = "keyring"`.

**Quit running sessions before switching.** A live process keeps its open file handles,
so writes may land in the profile it started under.

**Other profile switchers will refuse to run.** `aisw` and `cc-switch` deliberately
reject symlinks at config paths as a hardening measure. They fail rather than corrupt
anything, but they cannot be used alongside `ccp`.

**Windows is unsupported** — symlink creation needs Developer Mode or elevation.

`~/.claude.json` may reappear in `$HOME` after a Claude update, because the installer
runs without your shell environment. It is inert while `CLAUDE_CONFIG_DIR` is set; `ccp`
reports it and never deletes it, since in a context lacking the variable it is the live
file.

## Tests

```sh
python test_ccp.py
```
