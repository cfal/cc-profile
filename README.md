# cc-profile

`ccp` starts Claude Code or Codex with a config chosen by a local router. New chats rotate among enabled, ready configs. Resuming an exact session ID uses its original config or fails. Administrative changes, startup decisions, session mappings, proxies, and quota live in the web panel; the launcher never edits profiles.

## Install

The launcher and daemon are Rust executables for Linux. Building from source requires stable Rust, `pkg-config`, and SQLite development headers. At runtime the daemon uses system SQLite and `curl` for Claude quota; neither binary needs Python or uv. The verified launch grammar is for **Claude Code 2.1.329** and **Codex CLI 0.171.0**. Other builds can be used for maintenance, not trusted chat routing.

```sh
CARGO_BUILD_JOBS=1 cargo install --locked --path server --root "$HOME/.local"
ccp --version
ccpd
```

`ccp` remains the launcher; `cc-profile` is its compatibility alias. If upgrading from the Python tool, run `uv tool uninstall cc-profile` before the Cargo install so the old entry points do not shadow the binaries. To distribute without a Rust toolchain on the destination, build with `cargo build --release --locked --manifest-path server/Cargo.toml --bins` and ship the binaries from `server/target/release/` together for the target Linux architecture and system libraries. The web assets are embedded in `ccpd`.

Ensure `~/.local/bin` and the native tools are on PATH. The service resolves its own trusted executables at startup; use `ccpd --claude-bin /absolute/path --codex-bin /absolute/path` if its PATH differs from your shell. Restart after changing native binaries.

Open **http://127.0.0.1:8743**. On the first visit, set a panel password; subsequent visits use **Sign in**. No pairing token is needed. The first visitor can claim an unconfigured panel, so on a shared machine run **`ccpd set-password` before starting the service**. The same terminal command resets a forgotten password, even while the service is running, and signs out existing browsers without affecting launches. The prompt and the daemon startup log show the database path: use the same `XDG_STATE_HOME` in your shell and service so they address the same panel. Use `--port PORT` to change the panel port. For a VM, forward the same loopback port using SSH. Do not expose the panel publicly.

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

Restart with `systemctl --user restart ccpd` after changing PATH or updating either native executable. A verified build is required for new chats and resumes; a provider auto-update can require a cc-profile adapter update.

## Accounts

1. Open **Accounts**, choose Claude or Codex, then **Add config**. Register the absolute path of an existing, user-owned **physical config directory** (e.g. `/home/me/profiles/work/config`, not its parent or a `current` symlink). Registering does not copy, move, or sign in an account. Use an empty directory for a new profile. A label is editable; the physical home is not.
2. **Enable** the config to admit sessions. For maintenance, select **Set current** and run the native login command through `ccp`. Disable Current and it clears; no replacement is chosen. Changing Current never changes a session owner or advances rotation.
3. For **Codex new chats**, click **Install observer**, review the `hooks.json` entry, then approve that exact hook in the native `/hooks` screen using the scoped bootstrap command shown in Accounts. Configure native proxy variables first if required for bootstrap. Return and click **Check observer**. No hook is installed for Claude. Resume of an already mapped Codex chat does not require the observer.

For a workspace with local Codex settings, the router also checks whether the approved observer is enabled **in that workspace**. It asks the native metadata service rather than interpreting project TOML. Changes to user or system config trigger a recheck of prior approval on the next new chat. A rejected or unverified hook never admits a new chat on that config.

Simultaneous launches share a workspace check. A slow check may outlast the first launch; retry after it finishes. Linked worktrees are checked on each launch because their settings can live outside the working tree. A compatibility link at `~/.codex` can also supply settings to workspaces below your home, so those workspaces are checked rather than assumed ready.

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

`ccp` sends argv and cwd over a private Unix socket, applies an allowlisted environment patch, and execs the selected native binary in the same terminal. Its routing message goes to stderr, not the provider's stdout. If the server is unavailable, nothing launches. Remove conflicting inherited API-key, provider, or storage overrides rather than inadvertently charging another identity.

- **New:** deterministic round-robin per provider. Claude receives an unused UUID before exec. Codex chooses its own UUID; its trusted startup hook reports it, sometimes only on the first turn. A TUI closed at the idle prompt can leave a startup without a mapping.
- **Resume:** copy the command from **Sessions**, normally from the original workspace. Only exact known native UUIDs are supported. A disabled/missing owner is a logged refusal, never a transfer or a replacement chat. Re-enable the owner to continue. If the underlying transcript was deleted or the login changed, the native tool can still refuse.
- **Miscellaneous:** help/version flags, supported auth, doctor, features and MCP management commands use Current. No Current means no launch. In Claude, `--help` shows help; the word `help` alone is a chat prompt.
- **Unsupported:** `--last`, `claude -c`, pickers, forks, ephemeral/remote/background modes, hook or home overrides, and unrecognized routing-sensitive argument forms. The startup log is not the provider's activity history. In-app session navigation and raw native commands bypass the launcher.

Interactive Claude can dispatch a subcommand even after `--`. The router refuses ambiguous forms such as `ccp claude -- rc` and `ccp claude -- update`; to use a verb as a literal one-word prompt, use print mode, e.g. `ccp claude -p -- rc`.

**Startups** records decisions, denials, observed IDs and reported exec failures, not running processes or successful model calls. Full argv, prompts, tokens and proxy URLs are not logged. **Sessions** is a separately paginated, permanent mapping table, not a transcript viewer. Disabling a config blocks future admissions; it cannot revoke a delivered plan or stop an existing process.

## Proxies and quota

Accounts supports **Inherit**, **Custom** (`HTTP_PROXY` / `HTTPS_PROXY` per directory), and **Direct**. Custom writes matching upper/lowercase variables, removes ambient catch-all proxies and bypasses loopback. The CLI passes the settings to the child; ccpd is not an inference proxy. Refresh failures do not trigger a direct-egress fallback. Proxy credentials are write-only in the UI but necessarily available inside the child's environment and the private server database.

Quota is **informational**. Accounts shows percentage left, absolute/local reset times, and freshness. Missing business windows are unknown, not exhausted or unlimited. Explicit credits and spend controls are shown when reported. No spending or redeeming reset credits.

When Custom has only an HTTP proxy, that address is used for HTTPS requests too. Set an explicit HTTPS proxy to override it. Launches and quota checks use this same rule.

- Codex: native `account/rateLimits/read` in the selected `CODEX_HOME`. Subscription authentication is required; no model turn is started.
- Claude: Linux file-backed OAuth token in the selected `CLAUDE_CONFIG_DIR`, read-only GET to the vendor usage endpoint. Expired credentials require native renewal; the dashboard does not rotate a refresh token behind an active session.
- Accounts opening refreshes stale enabled rows; explicit Refresh can check one disabled row. Results are cached for five minutes, two upstream jobs maximum, single-flight per config with backoff. Stale last-good values are labelled. A changed login or proxy can retry an authentication or connection failure immediately; provider rate limits still apply. Neither quota nor provider connectivity participates in routing. Inherit uses the service environment for quota, the caller's environment for launches.

## Storage and recovery

`$XDG_STATE_HOME/cc-profile/router.sqlite3` (default `~/.local/state/cc-profile/`) holds the salted Argon2id panel password verifier, configs, rotation cursors, mappings, startup records and quota snapshots. `$XDG_RUNTIME_DIR/cc-profile/` holds the private launch socket; without an absolute XDG runtime directory use `runtime/` under the state directory. Both directories must be private to the user. Protect backups: proxy credentials are stored under OS permissions, not encrypted by ccpd. The server does not store provider OAuth tokens in SQLite. Upgrading from token pairing preserves router records and requires initial password setup; old tokens are not accepted.

This is a clean replacement: no old commands, profile migration, transcript scan, or adoption of historical session IDs. Existing config directories can be registered in place. Set up new mappings by launching through `ccp`. Startups expire after 90 days, unobserved Codex tickets after 24 hours; mappings do not expire. Back up the DB with SQLite's backup API, or stop the service before making a complete filesystem backup. Restore rather than inventing ownership from startup history.

After moving `ccpd`, reinstall and approve the observer. The router replaces its previously recorded hook, not custom hooks. For hooks installed before command tracking, review the old entry in `hooks.json` and remove it manually. An interrupted file or link replacement may also leave a temporary `.ccp-<uuid>` file or `.ccp-link-<uuid>` symlink. With the daemon stopped, inspect and remove only the individual leftover. The router does not sweep user-owned directories by filename pattern.

## Verification

```sh
CARGO_BUILD_JOBS=1 cargo test --locked --manifest-path server/Cargo.toml
CARGO_BUILD_JOBS=1 cargo build --locked --manifest-path server/Cargo.toml
python3 -m unittest discover -s tests -v
python3 tests/native/test_native.py -v  # optional: requires the verified native versions
uv run --no-project --with playwright python -m playwright install chromium --only-shell
uv run --no-project --with playwright python tests/panel/test_panel.py
```

All tests use disposable homes. Native smoke tests use no real credentials; they verify UUID assignment/observation and resume, not live subscription balances. Panel assets are embedded, with no runtime Node dependencies. Icons are from Lucide 0.468.0; the license is included under `server/web/`.
