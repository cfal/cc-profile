# cc-profile (`ccp`)

Switch Claude Code and Codex between named profiles — separate accounts, project
histories, MCP servers and settings.

Both CLIs keep all their state in one config directory and honour an environment
variable pointing at it. `ccp` makes that path a symlink and repoints it, so switching
is instant regardless of profile size. Nothing is copied.

## Install

```sh
uv tool install .
```

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
ccp                            active profile for each tool
ccp list                       list all profiles
ccp init                       create the config-dir symlinks
ccp reinit                     recreate/repair the symlinks
ccp new     <tool> <name>      create an empty profile
ccp migrate <tool> <name>      move an existing real config dir into a profile
ccp switch  <tool> <name>      point the symlink at <name>
ccp run     <tool> <name> [--] [args...]
```

```console
$ ccp new claude work
$ ccp switch claude work
claude: default -> work
  ~/.claude -> ~/.config/cc-profile/claude/work/config

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
~/.config/cc-profile/            override with CC_PROFILE_DIR
├── claude/<name>/config/        <- $CLAUDE_CONFIG_DIR points here
└── codex/<name>/config/         <- $CODEX_HOME points here
```

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
