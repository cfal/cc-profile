# cc-profile

`ccp` starts Claude Code, Codex, or Antigravity with a config chosen by a local router. By default, new launches prefer the eligible profile with the most verified quota remaining. When no profile has an account-wide reading the router can trust, it rotates among eligible profiles. Resuming a tracked session ID uses its original config or fails. Administrative changes, startup decisions, session mappings, proxies, and quota live in the web panel; the launcher never edits profiles.

## Install

The launcher and daemon are Rust executables for Linux. A native build requires stable Rust, `pkg-config`, and SQLite development headers, and the resulting daemon uses system SQLite. The static build below needs only Docker on the build machine and includes SQLite. Neither build needs Python or uv at runtime. The adapters were originally tested with **Claude Code 2.1.329**, **Codex CLI 0.171.0**, and **Antigravity CLI 1.5.14**. These are test baselines, not required versions. CCP uses the installed CLI and validates the actual protocol replies for managed features.

```sh
CARGO_BUILD_JOBS=1 cargo install --locked --path . --root "$HOME/.local"
ccp --version
ccpd
```

If upgrading from the Python tool, run `uv tool uninstall cc-profile` before the Cargo install so the old entry points do not shadow the binaries.

For distribution, build stripped, static Linux executables from the repository root:

```sh
./docker-build.sh --bins
```

On x86-64 the outputs are `target/x86_64-unknown-linux-musl/release/{ccp,ccpd}`. Ship both binaries to a Linux machine of the same architecture. The panel assets are embedded in `ccpd`.

Ensure `~/.local/bin` and the native tools are on PATH. The service resolves native executables using its own PATH; use `ccpd --claude-bin /absolute/path --codex-bin /absolute/path --agy-bin /absolute/path` if its PATH differs from your shell. Replacing a native executable or retargeting its installation symlink triggers a new version probe on the next operation; no daemon restart is needed.

To route interactive shell commands, add these to your shell startup file:

```sh
alias codex='ccp codex'
alias claude='ccp claude'
alias agy='ccp agy'
```

For programs that don't expand shell aliases, use a separate directory of launcher links instead. For the install above:

```sh
mkdir -p "$HOME/.local/ccp-bin"
ln -s "$HOME/.local/bin/ccp" "$HOME/.local/ccp-bin/codex"
ln -s "$HOME/.local/bin/ccp" "$HOME/.local/ccp-bin/claude"
ln -s "$HOME/.local/bin/ccp" "$HOME/.local/ccp-bin/agy"
export PATH="$HOME/.local/ccp-bin:$PATH"
```

Link to the same `ccp` installation you use for direct commands. Keep the real native tools later on PATH; do not replace them with the links. The launcher and daemon must resolve the same native executable. A daemon `--*-bin` override must identify the native entrypoint, not a launcher link. Native npm and shell entrypoints work normally; scripts that call back into `ccp` are not supported as aliases.

The panel defaults to **http://127.0.0.1:8743**. In a container, run `ccpd --listen 0.0.0.0` and publish the port with Docker's `-p 127.0.0.1:8743:8743` for host-only access. Open the published address in your browser. `--listen IP` accepts an IPv4 or IPv6 address; `--port PORT` changes the internal port. Mapped ports, custom hostnames, and HTTPS reverse proxies need no panel configuration. Only the panel listens on TCP; launches still use the private Unix socket.

On the first visit, set a panel password; subsequent visits use **Sign in**. This is the same on every listen address. Anyone who can reach an unconfigured panel can claim it, so keep it on a trusted network during setup or optionally run `ccpd set-password` first. Use HTTPS at a reverse proxy or an SSH tunnel on untrusted networks: `ccpd` serves plain HTTP. The same terminal command resets a forgotten password, even while the service is running, and signs out existing browsers without affecting launches. The prompt and startup log show the database path; use the same `XDG_STATE_HOME` in your shell and service.

Sign-in survives reloads in the same tab. A separately opened tab signs in independently. The theme button in the header cycles through system, light, and dark for this browser. In **Settings**, change the panel password using the current one. Changing it signs out all tabs; it does not stop running agents.

`ccp` does not start a missing daemon or select a different one. To keep the configured daemon running without a separate terminal on Linux, install the user service:

```sh
mkdir -p ~/.config/systemd/user
install -m 644 packaging/ccpd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ccpd
journalctl --user -u ccpd -n 10
```

Stop any foreground instance first. The user unit does not source shell startup files. For an npm/fnm installation, both the native launcher **and its Node interpreter** must be on the service PATH; an absolute `--codex-bin` alone will not fix a missing `node`. Run `command -v node` in your shell, then use `systemctl --user edit ccpd` with its **stable installation directory**, not a temporary multishell path:

```ini
[Service]
Environment=PATH=/absolute/node/installation/bin:%h/.local/bin:/usr/local/bin:/usr/bin:/bin
```

Restart with `systemctl --user restart ccpd` after changing the service's environment or updating CCP itself. A provider version change alone does not block chats, resumes or model discovery. If a provider changes a command, protocol or storage layout, the affected feature may still need an adapter update. A reset grant listed before the Codex executable changed must be listed again before redemption.

## Accounts

1. In **Accounts**, choose Claude, Codex, or Antigravity, click **Add profile**, and give it a label such as Work or Personal. **Create new profile** makes a private, empty directory under `$XDG_DATA_HOME/cc-profile/profiles/` (default `~/.local/share/cc-profile/profiles/`). Alternatively, **Use existing directory** registers an existing physical config directory in place. Nothing is copied. Renaming the label never moves the directory.
2. Sign in while the profile is still disabled. For **Codex**, click **Sign in with ChatGPT**, open the OpenAI verification link, and enter the displayed code. Choose the intended ChatGPT account, especially when adding a second profile in the same browser. Device login must be allowed by the account or workspace. The native Codex app-server saves the login; the panel displays the result. One device sign-in can run at a time. Reopening the profile resumes the display; cancel or retry there. A daemon restart ends a pending attempt. For **Claude**, copy the displayed `ccp login <id>` command and run it in your terminal. It opens the native Claude subscription login for that profile with its proxy settings. For Claude and Codex, `ccp login <id>` runs native sign-in in the selected profile with its proxy settings, and `ccp login <id> --status` checks the result.
3. Select **Enable profile** when ready. Repeat for the second profile. New chats use the configured routing mode; resumes stay with the original profile. Disabling a profile blocks both new chats and resumes without stopping running chats. Setup does not change **Current**, allocate a chat, or advance rotation. **Set current** is only needed for unscoped maintenance commands (e.g. `ccp claude auth status`). Disabling Current clears it without selecting a replacement.

Selecting an account shows its **Sign-in status**. For Claude and Codex, the daemon asks the native CLI about that profile and shows its email when reported. These checks use the profile's proxy settings and may contact the provider; they do not start a chat. For Antigravity, the panel checks only whether this profile has a private saved login file. It does not open the token, validate it, or infer an email. Refresh the status after signing in from a terminal.

### Choosing a profile

The **Routing mode** in Settings applies across providers. A new chat selects among eligible profiles of its own provider. **Most quota remaining** (the default) and **Least quota remaining** compare the smallest remaining percentage across the account's applicable limits. **Round robin** cycles through eligible profiles; **Highest priority** uses the numeric priority set in Accounts (0 to 100; higher wins). Ties rotate. Upgrades keep the stored routing mode, including Round robin. The new default applies only to fresh databases and upgrades from versions that had no routing setting.

**Earliest expiry** prefers the soonest upcoming automatic reset of any account-wide window with quota left. Next come profiles with a positive account window but no reset date, then profiles with unknown quota. A spend cap alone does not establish a positive window. This mode does not use credit balances, grants or subscription expiry dates.

In every mode, a verified, fresh Codex account-wide reading of zero excludes that profile from new admissions. If all eligible profiles are known to be exhausted, the launch is refused. Missing, stale, or already-reset readings are unknown, not zero: usage-based modes prefer measured profiles but can still use an unmeasured one. Sticky resumes stay with their owner even if its quota is exhausted; the provider can still refuse the request.

Claude Code's native usage report can contain cached numbers without a provider observation time. The panel displays them, but the router neither ranks nor excludes Claude profiles using those numbers. For Claude and Antigravity, all quota-based modes therefore rotate among eligible profiles.

Codex quota can be displayed without being used for routing. The router ranks a Codex profile only when the native app-server confirms file-backed login and the login and config still match the reading. Keyring and auto storage, a project config override, or an ambiguous managed policy make the score unknown. Ordinary model or trust settings in the profile do not by themselves disqualify it. Antigravity reports limits per model group, not a single account-wide allowance; its quota is for display only.

### Antigravity

Run `ccp agy` to open the native terminal UI in the current workspace with the selected profile's private HOME, filesystem login environment, and proxy. **Sessions** discovers saved AgY conversation files in registered profiles and offers `ccp agy --conversation=<uuid>`. File discovery does not identify which launch created a conversation or its workspace: a new TUI startup still has no verified ID. Run the copied command from the intended workspace. Every terminal resume rechecks the file and refuses missing, conflicting or disabled owners without rotating. Native AgY validates the contents; ccp never reads transcripts.

Use `ccp login <profile-id>` for native Google sign-in, then configure the harness to launch `ccp agy acp`. Use `ccp agy acp --for-session <id>` before sending a load request to reopen a session. The native processes use a private profile home and select filesystem credential storage instead of the desktop keyring. See [Antigravity setup, storage findings and verification limits](docs/antigravity.md) for the harness contract and upgrade caveats.

### Codex launches

With matching `ccp` and `ccpd` binaries and Codex 0.171.0, enable a profile, then run:

```sh
ccp codex
ccp codex exec --json 'Review these changes'
ccp codex resume <uuid>
ccp codex app-server                       # native JSONL protocol over stdin/stdout
ccp codex app-server --for-session <uuid>   # select the owner; client still sends thread/resume
```

Ordinary TUI, exec and exact resume commands run native Codex in the selected `CODEX_HOME`. CCP does not start an auxiliary app-server, add a remote endpoint, or change the model or working directory arguments. Codex itself can start or reuse a daemon for that profile. A running TUI daemon can keep its original proxy after you edit the profile; use native `--no-daemon` when a particular TUI launch needs the new proxy immediately. CCP does not restart native daemons.

Codex handles folder trust, Git repository checks and its local client options. Use `-C` for the workspace and `-m` for the model. Routing-sensitive arguments such as arbitrary remote endpoints, identity or profile overrides, and implicit resume selectors remain unsupported.

When Codex saves a rollout, the daemon uses its filename to record the thread ID and profile, including forks and subagents. It watches the known active and archived session directories and reconciles missed changes. It does not open transcripts or infer a workspace, root chat or startup. Two concurrent launches may therefore both show **Unknown** in Startup logs while both saved IDs appear in **Sessions**. Files removed before observation can be missed. A file-only resume with a pruned rollout, conflicting owners or incomplete verification is refused, never routed to an arbitrary profile.

Exact-owner checks include disabled profiles and use a bounded filename index. If an initial import is still running, retry after indexing. Histories exceeding 128 registered Codex profiles, 16,384 directory traversal steps, or 256 candidate filenames for one thread can exceed the verification limits. A slow filesystem can also exhaust the lookup time budget. None of these checks runs synchronously for a new chat.

A direct app-server selects a profile **once per process**, not per request. The client uses native stdio; diagnostics go to stderr. Successful persistent root `thread/start` replies are associated with that profile; a resume does not create an ownership claim. Unknown or disabled owners are refused before starting a `--for-session` server. Arbitrary listeners and cross-profile routing on one connection are not supported. Ownership inspection is bounded to 2 MiB per frame, 256 bytes per string request ID, and 1024 outstanding starts. Beyond those limits, frames still pass unchanged but a mapping can be missed.

Config identity includes the directory's device, inode and creation time. This detects reuse of a deleted directory's inode without writing markers into provider homes. Use a filesystem that exposes creation time (such as ext4). When upgrading an older router, make sure the original directories are mounted: the first open pins the creation time of homes matching their previously recorded device and inode. Restore any offline original and restart the daemon to finish pinning it.

**Upgrading from compatibility links:** the router no longer creates or updates config-directory symlinks such as `~/.codex`. Existing links are left untouched and no longer follow **Current**. Managed launches use the registered directory directly. Executable links such as `codex` pointing to `ccp` still work.

## Launch and resume

```sh
ccp claude --model opus
ccp codex exec --json 'review these changes'
ccp claude --permission-mode plan --resume 60000000-0000-4000-8000-000000000002 --model opus 'continue'
ccp codex resume 01a1fb8e-61fb-71f1-9831-4c38d904ad22
ccp codex exec resume 01a1fb8e-61fb-71f1-9831-4c38d904ad22 'continue'
ccp claude auth status
ccp codex login status
```

`ccp` sends argv and cwd over a private Unix socket, applies an allowlisted environment patch, and runs the selected native tool in the same terminal. By default it does not print the selected profile. Set `CC_PROFILE_DEBUG=1` on the launcher to show the profile label and directory on stderr. Native output and launcher errors do not depend on this setting. If the server is unavailable, nothing launches. Remove conflicting inherited API-key, provider, or storage overrides rather than inadvertently charging another identity.

- **New:** select a profile using the configured routing mode. Claude receives an unused UUID before exec. Codex and Antigravity terminal sessions are discovered from saved filenames. Explicit Codex app-server and Antigravity ACP connections also record successful native start replies.
- **Antigravity ACP:** A plain `agy acp` connection defers selection until the first new/load/resume request. Garcon can cold-reconnect by session ID without special launcher arguments. Before binding, `session/list` can browse enabled profiles on demand without advancing rotation. See [Antigravity setup and limits](docs/antigravity.md).
- **Resume:** copy the command from **Sessions**, normally from the original workspace. Only exact known native IDs are supported (opaque strings for ACP, UUIDs for Claude and Codex). A disabled/missing owner is a logged refusal, never a transfer or a replacement chat. Re-enable the owner to continue. If the underlying transcript was deleted or the login changed, the native tool can still refuse.
- **Miscellaneous:** help/version flags, supported auth, doctor, features and MCP management commands use Current. No Current means no launch. In Claude, `--help` shows help; the word `help` alone is a chat prompt.
- **Unsupported:** `--last`, `claude -c`, pickers, forks, remote/background modes, hook or home overrides, and unrecognized routing-sensitive argument forms. The startup log is not the provider's activity history. In-app session navigation and raw native commands bypass the launcher.

Interactive Claude can dispatch a subcommand even after `--`. The router refuses ambiguous forms such as `ccp claude -- rc` and `ccp claude -- update`; to use a verb as a literal one-word prompt, use print mode, e.g. `ccp claude -p -- rc`.

### One-shot prompts

```sh
ccp claude --model opus -p 'Summarize these changes' --output-format json
ccp agy --print --sandbox 'Summarize these changes' --output-format=json
ccp codex exec --ephemeral --sandbox read-only --json 'Summarize these changes'
git diff | ccp claude --print
ccp agy -p -- '--an option-like prompt'
```

One-shots select a profile just like a new chat, use the caller's streams and return the native exit status. The router records a startup decision but neither assigns nor tracks a session ID. Claude and AgY accept `-p` or `--print` and can read a piped prompt when no text is supplied. Use `--option=value` for a flag-like option value and `--` before a flag-like positional prompt. Claude runs with `--no-session-persistence`; an explicit `-p --resume <uuid>` is instead a tracked resume on the original profile. AgY may still save native history that can be discovered separately; use `ccp usage <profile-id>` for its native quota report.

Codex uses its own `exec --ephemeral` (or `e --ephemeral`) command. It accepts a prompt or stdin. Put `--ephemeral` after `exec`; it cannot be combined with a resume. Plain `exec` sessions are discoverable if Codex saves them. Codex `-p` means `--profile`, not print; neither a native config-profile override nor a synthetic `--print` is supported.

The **Dashboard** shows account quotas first, then **Startup logs** with routing decisions, denials, observed IDs and reported exec failures. The logs do not track running processes or successful model calls. Full argv, prompts, tokens and proxy URLs are not logged. **Sessions** is a separately paginated, permanent mapping table, not a transcript viewer. Disabling a config blocks future admissions; it cannot revoke a delivered plan or stop an existing process.

## Proxies and quota

Accounts supports **Inherit**, **Custom** (`HTTP_PROXY` / `HTTPS_PROXY` per directory), and **Direct**. These settings apply to native chats, sign-in, and quota checks. Inherit uses the caller's shell for CLI launches and the daemon environment for dashboard requests. A verification page opened in your browser uses the browser's own network settings. Custom writes matching upper/lowercase variables, removes ambient catch-all proxies and bypasses loopback. The CLI passes the settings to the child; ccpd is not an inference proxy. Refresh failures do not trigger a direct-egress fallback.

Proxy URLs are visible as you type. The panel API and copied commands omit saved credentials. The native process receives the full proxy URL, including credentials, in its environment. Tools that inherit that environment can read it too.

Quota is a **best-effort snapshot**, not a reservation. Accounts shows percentage left, absolute/local reset times, and freshness. Missing business windows are unknown, not exhausted or unlimited. Explicit credits and spend controls are shown when reported.

For Codex, **Earned resets available** is a count, separate from spending credits and the next automatic window reset. In Dashboard or Accounts, choose **List reset grants** on the intended profile to get current grant details. The provider may report a count without listing every grant. Choose **Review**, check the profile, grant and expiry, then **Redeem this grant**. Neither opening the panel nor refreshing quota spends a grant, and the router does not count grants as usable quota. If a redemption cannot be confirmed, **Retry same grant** sends only the original selection; it never switches to another grant. Reloading or signing out closes that retry prompt, but the daemon retains the attempt key across restarts. A missing grant is not proof that a timed-out redemption succeeded. You can also check the profile in native Codex under `/usage`.

Custom uses one proxy URL for both HTTP and HTTPS. Leaving the field blank while editing keeps its saved credentials; switching to Direct or Inherit clears it. Databases with separate proxy columns are not upgraded by this version.

- Codex: native `account/rateLimits/read` in the selected `CODEX_HOME`. Subscription authentication is required; no model turn is started.
- Antigravity: Dashboard and Accounts show remaining percentages and reset times for recognized model pools inline. These are not account-wide limits: even an empty pool does not affect routing, and launches never refresh them. `ccp usage <id>` still prints the native human-readable report.
- Claude: the native CLI handles OAuth, including token renewal, in the selected file-backed `CLAUDE_CONFIG_DIR`. The daemon sends `get_usage` with `skip_behaviors: true`: no prompt, transcript scan or model turn. The CLI can make account and usage requests through the profile's proxy settings. The daemon checks only credential-file metadata; it does not read tokens or fall back to direct HTTP. If the file changes during the check, the daemon discards that result; use **Refresh quota** to read it again. **CLI checked** is when the CLI reported the numbers, not when the provider last updated them.
- Even with the panel closed, `ccpd` checks enabled profiles for all three providers when their last successful quota read is 24 hours old. It catches up on unchecked or overdue profiles at daemon startup and notices newly enabled profiles within a minute. Any successful read recorded by the daemon starts that profile's next 24 hours. A cache hit or failed attempt does not; failures respect retry backoff.
- Opening Dashboard or Accounts still refreshes stale enabled profiles using a five-minute cache; a profile's own refresh button can also check it while disabled. The daily, panel and routing checks share the same workers: at most two upstream jobs and one check per profile at a time. Stale last-good values are labelled. A changed login or proxy can retry an authentication or connection failure immediately; provider rate limits still apply. For Codex, usage-based routing also requests a stale-data refresh in the background when admitting a new launch. The launch uses already-cached observations; it never waits for a provider quota check. Inherit uses the service environment for quota, the caller's environment for launches.

## Storage and recovery

`$XDG_STATE_HOME/cc-profile/router.sqlite3` (default `~/.local/state/cc-profile/`) holds the salted Argon2id panel password verifier, configs, rotation cursors, mappings, startup records and quota snapshots. `$XDG_RUNTIME_DIR/cc-profile/` holds the private launch socket; without an absolute XDG runtime directory use `runtime/` under the state directory. Both directories must be private to the user. Protect backups: proxy credentials are stored under OS permissions, not encrypted by ccpd. The server does not store provider OAuth tokens in SQLite. Upgrading from token pairing preserves router records and requires initial password setup; old tokens are not accepted.

Existing config directories can be registered in place. Codex and AgY saved filenames can be discovered retrospectively. No transcript contents are read. Startups expire after 90 days; session mappings do not expire, even after a file disappears.

**Upgrade:** Back up the router DB using SQLite's backup API, or stop the service before a filesystem backup. Install matching `ccp` and `ccpd` binaries and restart the daemon. Open a fresh ACP connection after upgrading: managed AgY ACP requires its v4 endpoints and never falls back to legacy admission. Other launches retain protocol v3. Existing owners, profiles, rotation and startup history survive; a pending launch without a UUID becomes unobserved. For rollback, restore the pre-upgrade DB with the previous binaries. When upgrading a much older hook-based installation, remove only cc-profile's old `SessionStart` command (`'/path/to/ccpd' observe`, matcher `startup`) from native `hooks.json`. CCP does not change providers' hooks.

## Verification

The root Cargo package builds both executables: [the launcher](src/bin/ccp.rs) and [the daemon](src/bin/ccpd/main.rs). Daemon-only HTTP handlers live beside its entry point; both binaries use [the internal library](src/lib.rs) for the launch protocol. Panel assets are in [web/](web/), and Rust and Python integration tests are in [tests/](tests/).

```sh
CARGO_BUILD_JOBS=1 cargo test --locked
CARGO_BUILD_JOBS=1 cargo build --locked --bins
python3 -m unittest discover -s tests -v
python3 tests/native/test_native.py -v  # optional: requires installed native CLIs
CCP_TEST_AGY=/path/to/agy python3 tests/native/test_agy.py -v  # optional: requires Linux bwrap
uv run --no-project --with playwright python -m playwright install chromium --only-shell
uv run --no-project --with playwright python tests/panel/test_panel.py
```

To exercise the static build instead of the debug build, set `CCPD_TEST_BIN` to the absolute path of the release `ccpd` when running the Python suite. It uses the `ccp` beside that binary.

All tests use disposable homes. Native smoke tests use no real credentials; they verify UUID assignment/observation and resume, not live subscription balances. Panel assets are embedded, with no runtime Node dependencies. Icons are from Lucide 0.468.0; the [license](web/LUCIDE-LICENSE.txt) is included.
