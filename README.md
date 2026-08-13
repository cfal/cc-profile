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
ccp migrate claude work
ccp migrate codex work
```

Any name will do — no name is special. `migrate` moves *and* links in one step, so
there is nothing to run afterwards. `ccp init` creates no profiles; it only repairs
links that have drifted, and `reinit` is the same command.

## Use

```
ccp                            everything, active entries marked
ccp list                       same thing
ccp init, ccp reinit           repair the symlinks (same command)
ccp new     <what> <name>      create an empty profile or history set
ccp migrate <what> <name>      move existing real dirs into one
ccp switch  <what> <name>      point the symlink at <name>
ccp rename  <what> <name> <new>  rename one, following it if active
ccp run     <tool> <name|policy> [--] [args...]
ccp usage   [<tool>]           quota for every profile
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

## Picking a profile by quota

`run` takes a policy in place of a name, so a launch can choose whichever account
still has room:

```sh
ccp run claude a/b,c/d/e,f -- --model opus
```

`/` falls through to the next choice, `,` balances within one — read that as *"a, else
whichever of b and c has more room, else d, else balance e and f"*. Both characters are
shell-safe and neither can appear in a profile name, so no quoting and no config file.

A profile qualifies while **every** window it reports is below its limit. Among
qualifying profiles the one with the most room in its **fullest** window wins — that
window is the one that will actually stop you, so ranking by it picks the most usable
profile rather than the best-looking average. A profile at `5h 0% / 7d 90%` scores 10 and
loses to one at `5h 80% / 7d 10%`, which scores 20: the first is nearly out of weekly
quota and useless for days, the second gives you real work now.

Tiers are priced in order and the first with any usable profile wins, so a policy that
leads with a healthy profile costs one usage request. Exact ties — two equally fresh
profiles — are broken at random, so they alternate instead of the first-listed always
taking the work. Codex per-model caps are shown but not scored: one being full still
leaves the other models usable.

## Usage

```console
$ ccp usage
ccp: loading usage from claude/personal
ccp: loading usage from claude/work
ccp: loading usage from codex/work
claude
     personal     login expired - run:  ccp run claude personal
   * work         5h 38% (resets in 1.9h)   7d 57% (resets in 3.4d)
codex
   * work         7d 15% (resets in 5.2d)   (pro)
```

Every profile is fetched before anything is printed, so the table arrives in one piece
rather than a line at a time between progress and refresh messages. Progress goes to
stderr, so redirecting stdout gives you just the table.

Reads each profile's own stored credentials and asks the provider — so you can compare
accounts without switching. This is the only command that touches the network or reads
credential *contents*; bare `ccp` stays offline.

**Expired tokens are renewed automatically.** `ccp usage` posts the profile's refresh
token to the provider and writes the result back to that same file, then retries once:

| | endpoint |
|---|---|
| Claude | `POST https://platform.claude.com/v1/oauth/token` (form-encoded) |
| Codex | `POST https://auth.openai.com/oauth/token` (JSON) |

No CLI subcommand can do this — `claude auth status` reads local state, `claude doctor`
and `mcp list` only clear credentials they judge malformed, and codex's `login status`,
`doctor`, `mcp list` and `plugin list` all leave `auth.json` untouched. Both CLIs renew
lazily when about to call the API, which costs quota. These are the same endpoints and
client IDs CodexBar uses; they are undocumented and may change.

All requests go through the `curl` binary (`curl.py`) rather than urllib, because the
curl here is a TLS-impersonating build: these endpoints sit behind bot filters that treat
a browser fingerprint and a scripting language's HTTP stack very differently. The wrapper
also strips `http_proxy`/`CURL_*` from the child environment, so ambient settings cannot
silently redirect a credential request.

Requests identify themselves as `cc-profile/<version>`. That matters more than it
sounds: an absent agent string draws a Cloudflare 403, and *claiming to be Claude Code*
draws a 429 from the token endpoint on the very first attempt — the official client
evidently has its own enforcement there. A browser string is throttled too. An honest,
non-browser name is served normally, which is also why CodexBar works: it never
pretends to be anything else.

Credentials are rewritten through a 0600 temp file and renamed into place, so an
interrupted write cannot leave a half-file where the tokens were. A failed refresh — for
any reason — leaves the file untouched. When the provider returns no new refresh token it
did not rotate, so the existing one is kept; discarding it would lock the profile out.

A profile whose *refresh* token has also lapsed reads as `login expired`, since nothing
can renew that. Codex tokens last about a week and Claude's about six hours, so in
practice it is Claude profiles that go stale.

**One race remains.** Both providers may rotate the refresh token, so if a session is
running under the same profile when `ccp usage` renews it, that session's in-memory copy
becomes stale. It refreshes on its own schedule and would hit the same rotation anyway,
but the window is real.

Endpoints are Claude Code's and Codex's own (`api.anthropic.com/api/oauth/usage`,
`chatgpt.com/backend-api/wham/usage`). They are undocumented and may change. On macOS
Claude keeps credentials in the Keychain rather than in the profile, so this reports
Codex only there.

## Layout

```
~/.config/cc-profile/                override with CC_PROFILE_DIR
├── claude/
│   ├── current -> work              <- `ccp switch claude` repoints this
│   ├── work/config/
│   │        └── projects -> ../../../claude-history/current/projects
│   └── personal/config/
├── codex/
│   ├── current -> default
│   └── default/config/
└── claude-history/
    ├── current -> main              <- `ccp switch claude-history` repoints this
    └── main/projects/                  the real transcripts

~/.claude -> .config/cc-profile/claude/current/config     written once by init
~/.codex  -> .config/cc-profile/codex/current/config
```

Each store is named for the key you type, so `ccp switch claude-history main` acts on
`~/.config/cc-profile/claude-history/main`.

**Every link is relative**, and the outer two name `current` rather than any profile.
So switching or renaming touches one link inside the tree and nothing in `$HOME`, and
the whole root can be moved — everything inside keeps resolving, and a single
`ccp reinit` repoints the two outer links:

```console
$ mv ~/.config/cc-profile ~/profiles
$ CC_PROFILE_DIR=~/profiles ccp reinit
claude: ~/.claude -> ~/profiles/claude/current/config
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
