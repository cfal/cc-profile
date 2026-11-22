# cc-profile

`ccp` starts Claude Code, Codex, or Antigravity ACP with a config chosen by a local router. New chats rotate among enabled, ready configs. Resuming an exact session ID uses its original config or fails. Administrative changes, startup decisions, session mappings, proxies, and quota live in the web panel; the launcher never edits profiles.

## Install

The launcher and daemon are Rust executables for Linux. A native build requires stable Rust, `pkg-config`, and SQLite development headers, and the resulting daemon uses system SQLite. The static build below needs only Docker on the build machine and includes SQLite. Neither build needs Python or uv at runtime. The daemon uses the `curl` executable for Claude quota. The verified launch grammar is for **Claude Code 2.1.329**, **Codex CLI 0.171.0**, and **Antigravity CLI 1.5.14**. Other Claude or Codex builds can be used for maintenance, not trusted chat routing. Antigravity's filesystem login behavior is version-specific; even setup requires the verified version.

```sh
CARGO_BUILD_JOBS=1 cargo install --locked --path . --root "$HOME/.local"
ccp --version
ccpd
```

`ccp` remains the launcher; `cc-profile` is its compatibility alias. If upgrading from the Python tool, run `uv tool uninstall cc-profile` before the Cargo install so the old entry points do not shadow the binaries.

For distribution, build stripped, static Linux executables from the repository root:

```sh
./docker-build.sh --bins
```

On x86-64 the outputs are `target/x86_64-unknown-linux-musl/release/{ccp,ccpd,cc-profile}`. Ship `ccp` and `ccpd` to a Linux machine of the same architecture; the alias is optional. The panel assets are embedded in `ccpd`.

Ensure `~/.local/bin` and the native tools are on PATH. The service resolves its own trusted executables at startup; use `ccpd --claude-bin /absolute/path --codex-bin /absolute/path --agy-bin /absolute/path` if its PATH differs from your shell. Restart after changing native binaries.

Open **http://127.0.0.1:8743**. On the first visit, set a panel password; subsequent visits use **Sign in**. No pairing token is needed. The first visitor can claim an unconfigured panel, so on a shared machine run **`ccpd set-password` before starting the service**. The same terminal command resets a forgotten password, even while the service is running, and signs out existing browsers without affecting launches. The prompt and the daemon startup log show the database path: use the same `XDG_STATE_HOME` in your shell and service so they address the same panel. Use `--port PORT` to change the panel port. For a VM, forward the same loopback port using SSH. Do not expose the panel publicly.

Sign-in survives reloads in the same tab. A separately opened tab signs in independently.

For a persistent service after installing the binary:

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

Restart with `systemctl --user restart ccpd` after changing PATH or updating a native executable. A verified build is required for new chats and resumes; a provider auto-update can require a cc-profile adapter update.

## Accounts

1. In **Accounts**, choose Claude, Codex, or Antigravity, click **Add profile**, and give it a label such as Work or Personal. **Create new profile** makes a private, empty directory under `$XDG_DATA_HOME/cc-profile/profiles/` (default `~/.local/share/cc-profile/profiles/`). Alternatively, **Use existing directory** registers an existing physical config directory in place. Nothing is copied. Renaming the label never moves the directory.
2. Sign in while the profile is still disabled. For **Codex**, click **Sign in with ChatGPT**, open the OpenAI verification link, and enter the displayed code. Choose the intended ChatGPT account, especially when adding a second profile in the same browser. Device login must be allowed by the account or workspace. The native Codex app-server saves the login; the panel displays the result. One device sign-in can run at a time. Reopening the profile resumes the display; cancel or retry there. A daemon restart ends a pending attempt. For **Claude**, copy the displayed `ccp login <id>` command and run it in your terminal. It opens the native Claude subscription login for that profile with its proxy settings. For Claude and Codex, `ccp login <id>` runs native sign-in in the selected profile with its proxy settings, and `ccp login <id> --status` checks the result.
3. Select **Enable profile** when ready. Repeat for the second profile. New chats rotate among enabled, ready profiles; resumes stay with the original one. Disabling a profile blocks both new chats and resumes without stopping running chats. Setup does not change **Current**, allocate a chat, or advance rotation. **Set current** is only needed for unscoped maintenance commands (e.g. `ccp claude auth status`). Disabling Current clears it without selecting a replacement.

### Antigravity ACP

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

For a new chat, the TUI or exec client connects to a private app-server on this machine. Both inherit the selected profile and proxy; no credentials are copied. The launcher records the first persistent root thread that Codex announces, not later in-app navigation. An ID observed before the first turn is not proof that Codex saved the thread. Exact resumes go straight to the native client on the saved profile. User-configured hooks are not disabled.

Use `-C` for the workspace and `-m` to override the model. Interactive launches keep the native folder trust prompt. New `exec` launches require a Git workspace unless you explicitly pass `--skip-git-repo-check`. Configure sandbox and approval policy in the profile: the router refuses per-launch policy overrides, `--add-dir`, `--no-daemon`, and `--output-schema` for new managed chats rather than silently ignoring them.

A direct app-server selects a profile **once per process**, not per request. The client uses native stdio; diagnostics go to stderr. Successful persistent root `thread/start` replies are associated with that profile; a resume does not create an ownership claim. Unknown or disabled owners are refused before starting a `--for-session` server. Arbitrary listeners and cross-profile routing on one connection are not supported. Ownership inspection is bounded to 2 MiB per frame, 256 bytes per string request ID, and 1024 outstanding starts. Beyond those limits, frames still pass unchanged but a mapping can be missed.

Config identity includes the directory's device, inode and creation time. This detects reuse of a deleted directory's inode without writing markers into provider homes. Use a filesystem that exposes creation time (such as ext4). When upgrading an older router, make sure the original directories are mounted: the first open pins the creation time of homes matching their previously recorded device and inode. Restore any offline original and restart the daemon to finish pinning it.

**Compatibility links** are optional. Supply an existing profile root and the absolute outer link path (e.g. `~/.codex` expanded). Accounts previews the paths and flags drift. Set current atomically updates relative links; clearing it can leave the managed outer link dangling. A real file/directory or unrecognized link is not replaced. Managed launches always use the physical home, regardless of these links. For an unwrapped `claude` using the compatibility path, explicitly export `CLAUDE_CONFIG_DIR="$HOME/.claude"` so its global JSON is stored inside that directory.

## Launch and resume

```sh
ccp claude --model opus
ccp codex exec --json 'review these changes'
ccp claude --resume 60000000-0000-4000-8000-000000000002
ccp codex resume 01a1fb8e-61fb-71f1-9831-4c38d904ad22
ccp codex exec resume 01a1fb8e-61fb-71f1-9831-4c38d904ad22 'continue'
ccp claude auth status
ccp codex login status
```

`ccp` sends argv and cwd over a private Unix socket, applies an allowlisted environment patch, and runs the selected native tool in the same terminal. Its routing message goes to stderr, not the provider's stdout. If the server is unavailable, nothing launches. Remove conflicting inherited API-key, provider, or storage overrides rather than inadvertently charging another identity.

- **New:** deterministic round-robin per provider. Claude receives an unused UUID before exec. Codex chooses its own UUID; the launcher records it from the native app-server protocol without a startup hook. Antigravity selects a profile per ACP connection and binds each successful `session/new` to it.
- **Resume:** copy the command from **Sessions**, normally from the original workspace. Only exact known native IDs are supported (opaque strings for ACP, UUIDs for Claude and Codex). A disabled/missing owner is a logged refusal, never a transfer or a replacement chat. Re-enable the owner to continue. If the underlying transcript was deleted or the login changed, the native tool can still refuse.
- **Miscellaneous:** help/version flags, supported auth, doctor, features and MCP management commands use Current. No Current means no launch. In Claude, `--help` shows help; the word `help` alone is a chat prompt.
- **Unsupported:** `--last`, `claude -c`, pickers, forks, ephemeral/remote/background modes, hook or home overrides, and unrecognized routing-sensitive argument forms. The startup log is not the provider's activity history. In-app session navigation and raw native commands bypass the launcher.

Interactive Claude can dispatch a subcommand even after `--`. The router refuses ambiguous forms such as `ccp claude -- rc` and `ccp claude -- update`; to use a verb as a literal one-word prompt, use print mode, e.g. `ccp claude -p -- rc`.

The **Dashboard** shows account quotas first, then **Startup logs** with routing decisions, denials, observed IDs and reported exec failures. The logs do not track running processes or successful model calls. Full argv, prompts, tokens and proxy URLs are not logged. **Sessions** is a separately paginated, permanent mapping table, not a transcript viewer. Disabling a config blocks future admissions; it cannot revoke a delivered plan or stop an existing process.

## Proxies and quota

Accounts supports **Inherit**, **Custom** (`HTTP_PROXY` / `HTTPS_PROXY` per directory), and **Direct**. These settings apply to native chats, sign-in, and quota checks. Inherit uses the caller's shell for CLI launches and the daemon environment for dashboard requests. A verification page opened in your browser uses the browser's own network settings. Custom writes matching upper/lowercase variables, removes ambient catch-all proxies and bypasses loopback. The CLI passes the settings to the child; ccpd is not an inference proxy. Refresh failures do not trigger a direct-egress fallback.

Proxy URLs are visible as you type. The panel API and copied commands omit saved credentials. The native process receives the full proxy URL, including credentials, in its environment. Tools that inherit that environment can read it too.

Quota is **informational**. Accounts shows percentage left, absolute/local reset times, and freshness. Missing business windows are unknown, not exhausted or unlimited. Explicit credits and spend controls are shown when reported. No spending or redeeming reset credits.

Custom uses one proxy URL for both HTTP and HTTPS. Leaving the field blank while editing keeps its saved credentials; switching to Direct or Inherit clears it. Databases with separate proxy columns are not upgraded by this version.

- Codex: native `account/rateLimits/read` in the selected `CODEX_HOME`. Subscription authentication is required; no model turn is started.
- Antigravity: explicit native text report via the card or `ccp usage <id>`; not included in background polling.
- Claude: Linux file-backed OAuth token in the selected `CLAUDE_CONFIG_DIR`, read-only GET to the vendor usage endpoint. Expired credentials require native renewal; the dashboard does not rotate a refresh token behind an active session.
- Opening Dashboard or Accounts refreshes stale enabled Claude and Codex profiles; a profile's own refresh button can also check it while disabled. Both views share the same five-minute cache, with at most two upstream jobs and one check per profile at a time. Stale last-good values are labelled. A changed login or proxy can retry an authentication or connection failure immediately; provider rate limits still apply. Neither quota nor provider connectivity participates in routing. Inherit uses the service environment for quota, the caller's environment for launches.

## Storage and recovery

`$XDG_STATE_HOME/cc-profile/router.sqlite3` (default `~/.local/state/cc-profile/`) holds the salted Argon2id panel password verifier, configs, rotation cursors, mappings, startup records and quota snapshots. `$XDG_RUNTIME_DIR/cc-profile/` holds the private launch socket; without an absolute XDG runtime directory use `runtime/` under the state directory. Both directories must be private to the user. Protect backups: proxy credentials are stored under OS permissions, not encrypted by ccpd. The server does not store provider OAuth tokens in SQLite. Upgrading from token pairing preserves router records and requires initial password setup; old tokens are not accepted.

This is a clean replacement: no old commands, profile migration, transcript scan, or adoption of historical session IDs. Existing config directories can be registered in place. Set up new mappings by launching through `ccp`. Startups expire after 90 days, unobserved Codex tickets after 24 hours; mappings do not expire. Back up the DB with SQLite's backup API, or stop the service before making a complete filesystem backup. Restore rather than inventing ownership from startup history.

**Upgrading from the observer:** update `ccp` and `ccpd` together and restart the daemon. Protocol v2 rejects mixed binaries rather than launching without tracking. Existing session owners, profiles, and rotation survive; a pending launch without a UUID cannot be recovered. Remove only cc-profile's old `SessionStart` command (`'/path/to/ccpd' observe`, matcher `startup`) from each profile's `hooks.json`. The callback no longer exists; the upgrade does not edit native files or disable other hooks.

## Verification

The root Cargo package builds both executables: [the launcher](src/bin/ccp.rs) and [the daemon](src/bin/ccpd/main.rs). Daemon-only HTTP handlers live beside its entry point; both binaries use [the internal library](src/lib.rs) for the launch protocol. Panel assets are in [web/](web/), and Rust and Python integration tests are in [tests/](tests/).

```sh
CARGO_BUILD_JOBS=1 cargo test --locked
CARGO_BUILD_JOBS=1 cargo build --locked --bins
python3 -m unittest discover -s tests -v
python3 tests/native/test_native.py -v  # optional: requires the verified native versions
CCP_TEST_AGY=/path/to/agy python3 tests/native/test_agy.py -v  # optional: requires Linux bwrap
uv run --no-project --with playwright python -m playwright install chromium --only-shell
uv run --no-project --with playwright python tests/panel/test_panel.py
```

To exercise the static build instead of the debug build, set `CCPD_TEST_BIN` to the absolute path of the release `ccpd` when running the Python suite. It uses the `ccp` beside that binary.

All tests use disposable homes. Native smoke tests use no real credentials; they verify UUID assignment/observation and resume, not live subscription balances. Panel assets are embedded, with no runtime Node dependencies. Icons are from Lucide 0.468.0; the [license](web/LUCIDE-LICENSE.txt) is included.
