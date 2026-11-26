# cc-profile reference

For installation and your first chat, start with the [quickstart](../README.md#quickstart). This reference covers service configuration, exact routing behavior, client integrations, and recovery. Run shell examples from the repository root unless otherwise noted.

## Native executables

The adapters were originally tested with **Claude Code 2.1.329**, **Codex CLI 0.171.0**, and **Antigravity CLI 1.5.14**. These are test baselines, not required versions. CCP uses the installed CLI and validates the actual protocol replies for managed features.

If upgrading from the Python tool, run `uv tool uninstall cc-profile` before the Cargo install so the old entry points do not shadow the binaries.

Ensure `~/.local/bin` and the native tools are on PATH. The service resolves native executables using its own PATH; use `ccpd --claude-bin /absolute/path --codex-bin /absolute/path --agy-bin /absolute/path` if its PATH differs from your shell. Replacing a native executable or retargeting its installation symlink triggers a new version probe on the next operation; no daemon restart is needed. **Settings → Binary versions** shows what the daemon sees. Refresh there to run the three native version checks again, including after a package upgrade that left its launcher unchanged.

### Shells and automation

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

### Panel access

The panel defaults to **http://127.0.0.1:8743**. In a container, run `ccpd --listen 0.0.0.0` and publish the port with Docker's `-p 127.0.0.1:8743:8743` for host-only access. Open the published address in your browser. `--listen IP` accepts an IPv4 or IPv6 address; `--port PORT` changes the internal port. Mapped ports, custom hostnames, and HTTPS reverse proxies need no panel configuration. Only the panel listens on TCP; launches still use the private Unix socket.

On the first visit, set a panel password; subsequent visits use **Sign in**. This is the same on every listen address. Anyone who can reach an unconfigured panel can claim it, so keep it on a trusted network during setup or optionally run `ccpd set-password` first. Use HTTPS at a reverse proxy or an SSH tunnel on untrusted networks: `ccpd` serves plain HTTP. The same terminal command resets a forgotten password, even while the service is running, and signs out existing browsers without affecting launches. The prompt and startup log show the database path; use the same [directory settings](#storage-and-recovery) in your shell and service.

Sign-in survives reloads in the same tab. A separately opened tab signs in independently. The theme button in the header cycles through system, light, and dark for this browser. In **Settings**, change the panel password using the current one. Changing it signs out all tabs; it does not stop running agents.

`ccpd --no-auth` disables password checks for the entire HTTP panel and API for that run. The daemon prints a startup warning; no setup, sign-in, or browser session is required. Even on loopback, other local users who can reach the port have full panel access. If you rely on a proxy for authentication, restrict direct access to the daemon. Restart without the flag to require the existing password, or first-visit setup if none was set. Provider logins and the private launch socket are unchanged.

### User service

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

1. **Accounts** opens on **All**, showing profiles from all three providers. Click **Add profile**, choose the provider and give the profile a label such as Work or Personal. From a provider tab, that provider is already selected. **Create new profile** makes a private, empty directory at `<accounts>/<tool>/<uuid>` (for example, `~/.config/cc-profile/accounts/agy/<uuid>`). Alternatively, **Use existing directory** registers an existing physical config directory in place. Nothing is copied. Renaming the label never moves the directory.
2. Sign in while the profile is still disabled. For **Codex**, click **Sign in with ChatGPT**, open the OpenAI verification link, and enter the displayed code. Choose the intended ChatGPT account, especially when adding a second profile in the same browser. Device login must be allowed by the account or workspace. The native Codex app-server saves the login; the panel displays the result. One device sign-in can run at a time. Reopening the profile resumes the display; cancel or retry there. A daemon restart ends a pending attempt. For **Claude**, copy the displayed `ccp login <id>` command and run it in your terminal. It opens the native Claude subscription login for that profile with its proxy settings. For Claude and Codex, `ccp login <id>` runs native sign-in in the selected profile with its proxy settings, and `ccp login <id> --status` checks the result.
3. Select **Enable profile** when ready. Repeat for the second profile. New chats use the configured routing mode; resumes stay with the original profile. **Set as default** chooses this provider's account for maintenance commands that don't name a profile (e.g. `ccp claude auth status` or `agy models`), not for new chats. Disabling the default account clears that selection without picking a replacement. Disabling any profile blocks new chats and resumes without stopping running chats.

Selecting an account shows its **Sign-in status**. For Claude and Codex, the daemon asks the native CLI about that profile and shows its email when reported. For Antigravity, opening a profile checks the saved login file locally. **Refresh sign-in status** runs native `agy models` for the selected profile, even if disabled, using its own proxy. **Catalog accessible** means the CLI returned a valid catalog, not that every model is available for chat. Only an explicit sign-in rejection is shown as **Sign-in required**; a timeout, proxy error, or unrecognized reply leaves access unverified. CCP does not read the token, infer an email, or start a chat.

### Choosing a profile

The default routing mode is **Earliest expiry, most quota remaining**. It picks the profile whose positive applicable quota window resets soonest. If two profiles reset at the same time, it picks the one with more quota left. **Earliest expiry, least quota remaining** uses the same reset order but picks the one with less quota left on a tie. Remaining quota is the smallest percentage left across the profile's applicable limits, not just the window resetting first. Exact ties rotate.

Both expiry modes use dated windows before positive undated windows, then rotate among profiles with unknown quota. A spend cap alone does not establish a positive window. Neither mode uses credit, grant or subscription expiry dates. The former **Earliest expiry** setting upgrades to **Earliest expiry, most quota remaining**; other stored choices are preserved.

**Most quota remaining** and **Least quota remaining** compare capacity without considering expiry. **Round robin** cycles through eligible profiles; **Highest priority** uses the numeric priority set in Accounts (0 to 100; higher wins).

In every mode, a verified, fresh Codex account-wide reading of zero excludes that profile from new admissions. In quota-based modes, a reported zero on an applicable Claude or Antigravity window also excludes the profile. If all eligible profiles report exhausted quota, the launch is refused. Missing, stale, or already-reset readings are unknown, not zero: usage-based modes prefer measured profiles but can still use an unmeasured one. Sticky resumes stay with their owner even if its quota is exhausted; the provider can still refuse the request.

Claude quota modes use the tightest remaining account-wide window reported by the native CLI (not a model-specific limit). In Claude Code 2.1.332, a new CLI process can reuse a profile snapshot less than a minute old or, on a failed read, return a last-known value up to an hour old. The response gives no provider observation time or supported force-refresh option. CCP does not edit Claude's cache; freshness means when CCP checked.

Codex quota can be displayed without being used for routing. The router ranks a Codex profile only when the native app-server confirms file-backed login and the login and config still match the reading. Keyring and auto storage, a project config override, or an ambiguous managed policy make the score unknown. Ordinary model or trust settings in the profile do not by themselves disqualify it. Antigravity quota modes compare windows for the model pool selected at launch: Gemini Models by default (including ACP), Claude and GPT models for `claude-*` or `gpt-*`, and the separate Gemini 4 Argon pool for `gemini-4-argon` variants. An unrecognized model or missing pool has unknown quota. Switching models after launch never moves a session.

### Antigravity

Run `ccp agy` to open the native terminal UI in the current workspace with the selected profile's private HOME, filesystem login environment, and proxy. **Sessions** discovers saved AgY conversation files in registered profiles and offers `ccp agy --conversation=<uuid>`. File discovery does not identify which launch created a conversation or its workspace: a new TUI startup still has no verified ID. Run the copied command from the intended workspace. Every terminal resume rechecks the file and refuses missing, conflicting or disabled owners without rotating. Native AgY validates the contents; ccp never reads transcripts.

On AgY 1.5.16+, `ccp agy --resume <uuid>` and `ccp agy -r <uuid>` are equivalent to `--conversation`. CCP does not guess which account to use for `--continue`.

Use `ccp login <profile-id>` for native Google sign-in, then configure the harness to launch `ccp agy acp`. ACP (including login) preserves the caller's `HOME` and `XDG_CONFIG_HOME` for tools. CCP supplies the selected profile's native storage flags for Google login and history; the separate plugin secret file can be shared. Plain terminal chats, print, models and quota do not accept those flags on 1.5.17, so CCP sets their `HOME` to the account directory and unsets `XDG_CONFIG_HOME`. Both paths use file credentials, not the desktop keyring. See [Antigravity setup and verification limits](antigravity.md).

### Codex launches

With matching `ccp` and `ccpd` binaries and an enabled Codex profile, these are supported launches:

```sh
ccp codex
ccp codex exec --json 'Review these changes'
thread_id=01a1fb8e-61fb-71f1-9831-4c38d904ad22  # replace with the ID from Sessions
ccp codex resume "$thread_id"
ccp codex app-server                       # native JSONL protocol over stdin/stdout
ccp codex app-server --for-session "$thread_id"   # client still sends thread/resume
```

Ordinary TUI, exec and exact resume commands run native Codex in the selected `CODEX_HOME`. CCP does not start an auxiliary app-server, add a remote endpoint, or change the model or working directory arguments. Codex itself can start or reuse a daemon for that profile. A running TUI daemon can keep its original proxy after you edit the profile; use native `--no-daemon` when a particular TUI launch needs the new proxy immediately. CCP does not restart native daemons.

Codex handles folder trust, Git repository checks and its local client options. Use `-C` for the workspace and `-m` for the model. Routing-sensitive arguments such as arbitrary remote endpoints, identity or profile overrides, and implicit resume selectors remain unsupported.

When Codex saves a rollout, the daemon uses its filename to record the thread ID and profile, including forks and subagents. It watches the known active and archived session directories and reconciles missed changes. It does not open transcripts or infer a workspace, root chat or startup. Two concurrent launches may therefore both show **Unknown** in Startup logs while both saved IDs appear in **Sessions**. Files removed before observation can be missed. A file-only resume with a pruned rollout, conflicting owners or incomplete verification is refused, never routed to an arbitrary profile.

Exact-owner checks include disabled profiles and use a bounded filename index. If an initial import is still running, retry after indexing. Histories exceeding 128 registered Codex profiles, 16,384 directory traversal steps, or 256 candidate filenames for one thread can exceed the verification limits. A slow filesystem can also exhaust the lookup time budget. None of these checks runs synchronously for a new chat.

An unpinned `codex app-server` waits for a thread operation before selecting an account. CCP obtains the real `initialize` reply from a temporary, signed-out native process. It reports a private, absolute `codexHome` under CCP's temporary directory, **not** the project cwd or an account directory. Do not scan that directory for files; use the protocol's thread list and returned thread paths. An initialize-only connection neither advances rotation nor creates a Startup row. If the selected server's handshake differs in another field, CCP returns an error before forwarding the thread operation.

- **Before binding:** `thread/list` browses all enabled accounts, even without a default account. CCP merges native pages; an incomplete listing, conflicting owner or unavailable participant returns an error rather than a partial list. Model, skill, config and account reads use the **Default** profile without binding the connection. They do not describe all accounts.
- **Binding:** `thread/start` uses normal routing. `thread/resume`, `thread/fork` and thread history reads select the verified owner of the exact source ID; an unknown, missing or disabled owner is refused. A supplied rollout path does not override the ID. CCP replays only the handshake on the selected server and forwards the operation once. Both a successful persistent root start and a verified native fork can establish a new ID.
- **After binding:** the connection stays on its selected profile. Use a new connection for a thread on another profile. Native turn events, server requests and client responses stay on stdio. Use `--for-session <uuid>` to select the owner **before** initializing instead: that transparent bridge reports the real home and forwards native selectors as supplied. Neither form is an OS sandbox.
- **Message sizes:** the deferred bridge rejects individual client messages over 16 MiB without disconnecting. Large native messages stream on a bound connection unless a pending thread list, start or fork needs ownership verification. In that case CCP closes the connection rather than release an unverified result. Private initialization and pre-bind utility replies are limited to 16 MiB.

Cross-profile browse preserves native thread rows and filters. It supports forward pages ordered by creation or update time, up to 500 results per page; continue on the same connection within 60 seconds. One full collection runs per router runtime at a time, with limits of 32 enabled profiles, 2,048 matching rows, 8 MiB of retained data, and 35 seconds. Narrow the filters when a limit is reached. In **Sessions**, **Listed by Codex** identifies a native listing, not a launch or an active thread.

Config identity includes the directory's device, inode and creation time. This detects reuse of a deleted directory's inode without writing markers into provider homes. Use a filesystem that exposes creation time (such as ext4). When upgrading an older router, make sure the original directories are mounted: the first open pins the creation time of homes matching their previously recorded device and inode. Restore any offline original and restart the daemon to finish pinning it.

**Upgrading from compatibility links:** the router no longer creates or updates config-directory symlinks such as `~/.codex`. Existing links are left untouched and no longer follow the **default account**. Managed launches use the registered directory directly. Executable links such as `codex` pointing to `ccp` still work.

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
- **Antigravity ACP:** A plain `agy acp` connection defers selection until the first new/load/resume request. Garcon can cold-reconnect by session ID without special launcher arguments. Before binding, `session/list` can browse enabled profiles on demand without advancing rotation. See [Antigravity setup and limits](antigravity.md).
- **Resume:** copy the command from **Sessions**, normally from the original workspace. Only exact known native IDs are supported (opaque strings for ACP, UUIDs for Claude and Codex). A disabled/missing owner is a logged refusal, never a transfer or a replacement chat. Re-enable the owner to continue. If the underlying transcript was deleted or the login changed, the native tool can still refuse.
- **Miscellaneous:** help/version flags, supported auth, doctor, features and MCP management commands use the default account. Without a default account, these commands do not launch. In Claude, `--help` shows help; the word `help` alone is a chat prompt.
- **Unsupported:** `--last`, `claude -c`, pickers, CLI fork selectors, remote/background modes, hook or home overrides, and unrecognized routing-sensitive argument forms. The startup log is not the provider's activity history. In-app session navigation and raw native commands bypass the launcher.

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

### Garcon executors

Use CCP for subscription-backed chats. In the **executor** environment, point `CLAUDE_BINARY` and `GARCON_CODEX_CLI` at the absolute paths to the `claude` and `codex` executable links to `ccp`, not at bare `ccp` or a shell command. The genuine native executables must also be available to the launcher. Set a default account for each provider so that version and sign-in probes can run; their results describe that account, not the whole pool. Use the native binaries directly for Garcon endpoints that supply their own API key or base URL.

Garcon integrations that access transcripts need the selected profile, not just a working executable:

- **Claude:** For bidirectional `--print` with `stream-json` input and output, a new `--session-id <uuid>` is reserved on the selected account. Garcon can reconnect with `--resume <uuid>`; ordinary one-shots remain nonpersistent. Garcon [constructs the native transcript path before launching Claude](https://github.com/cfal/garcon/blob/fd008f12ab4118668bacc33289823ff68c560560/server-agents/claude/src/agents/claude/execution.ts#L43-L66), so file-based history, usage and forks may miss a routed transcript even when reconnect works.
- **Codex:** Garcon's stock [app-server command](https://github.com/cfal/garcon/blob/fd008f12ab4118668bacc33289823ff68c560560/server-agents/codex/src/agents/codex/app-server/client.ts#L327-L347) can start, resume or fork without a launcher-only flag. Garcon sends the thread ID after initialization; CCP selects the account then. Ephemeral background queries are also supported. Tools that scan a single filesystem home directly for usage history still see only that home.

## Proxies and quota

Accounts supports **Inherit**, **Custom** (`HTTP_PROXY` / `HTTPS_PROXY` per directory), and **Direct**. These settings apply to native chats, sign-in, and quota checks. Inherit uses the caller's shell for CLI launches and the daemon environment for dashboard requests. A verification page opened in your browser uses the browser's own network settings. Custom writes matching upper/lowercase variables, removes ambient catch-all proxies and bypasses loopback. The CLI passes the settings to the child; ccpd is not an inference proxy. Refresh failures do not trigger a direct-egress fallback.

Proxy URLs are visible as you type. The panel API and copied commands omit saved credentials. The native process receives the full proxy URL, including credentials, in its environment. Tools that inherit that environment can read it too.

Quota is a **best-effort snapshot**, not a reservation. Accounts shows percentage left, absolute/local reset times, and freshness. Missing business windows are unknown, not exhausted or unlimited. Explicit credits and spend controls are shown when reported.

For Codex, **Earned resets available** is a count, separate from spending credits and the next automatic window reset. In Dashboard or Accounts, choose **List reset grants** on the intended profile to get current grant details. The provider may report a count without listing every grant. Choose **Review**, check the profile, grant and expiry, then **Redeem this grant**. Neither opening the panel nor refreshing quota spends a grant, and the router does not count grants as usable quota. If a redemption cannot be confirmed, **Retry same grant** sends only the original selection; it never switches to another grant. Reloading or signing out closes that retry prompt, but the daemon retains the attempt key across restarts. A missing grant is not proof that a timed-out redemption succeeded. You can also check the profile in native Codex under `/usage`.

Custom uses one proxy URL for both HTTP and HTTPS. Leaving the field blank while editing keeps its saved credentials; switching to Direct or Inherit clears it. Databases with separate proxy columns are not upgraded by this version.

- Codex: native `account/rateLimits/read` in the selected `CODEX_HOME`. Subscription authentication is required; no model turn is started.
- Antigravity: Dashboard and Accounts show remaining percentages and reset times for recognized model pools inline. Only the relevant pool affects quota-based routing. Exhausting one pool does not exhaust the others. `ccp usage <id>` still prints the native human-readable report.
- Claude: the native CLI handles OAuth, including token renewal, in the selected file-backed `CLAUDE_CONFIG_DIR`. The daemon sends `get_usage` with `skip_behaviors: true`: no prompt, transcript scan or model turn. The CLI can make account and usage requests through the profile's proxy settings. The daemon checks only credential-file metadata; it does not read tokens or fall back to direct HTTP. If the file changes during the check, the daemon discards that result; use **Refresh quota** to read it again. **CLI checked** is when the CLI reported the numbers, not when the provider last updated them.
- Even with the panel closed, `ccpd` checks enabled profiles for all three providers when their last successful quota read is 24 hours old. It catches up on unchecked or overdue profiles at daemon startup and notices newly enabled profiles within a minute. Any successful read recorded by the daemon starts that profile's next 24 hours. A cache hit or failed attempt does not; failures respect retry backoff.
- Opening Dashboard or Accounts still refreshes stale enabled profiles using a two-minute cache; a profile's own refresh button can also check it while disabled. The daily, panel and routing checks share the same workers: at most two upstream jobs and one check per profile at a time. Stale last-good values are labelled. A changed login or proxy can retry an authentication or connection failure immediately; provider rate limits still apply. Before choosing an account in a quota-based mode, CCP refreshes stale or missing readings for enabled profiles of that provider, joining any checks already running. The entire launch waits at most two minutes, never two minutes per profile. On failure, backoff or timeout, it uses the fresh readings available, rotating if none are usable. It does not retry until success. Exact resumes, maintenance commands, round robin and priority do not wait for quota. Inherit uses the service environment for quota, the caller's environment for launches.

## Storage and recovery

| Contents | Default directory | Override |
| --- | --- | --- |
| Data: router database (`router.sqlite3`), initialization marker and service lock | `$XDG_CONFIG_HOME/cc-profile` or `~/.config/cc-profile` | `CC_PROFILE_DATA_PATH` |
| Accounts: new provider homes under `<tool>/<uuid>`, including native credentials and history | `<data>/accounts` | `CC_PROFILE_ACCOUNTS_PATH` |
| Runtime: `ccpd.sock`, `listener.lock`, `codex-browse.lock` | `$XDG_RUNTIME_DIR/cc-profile`, or `<data>/runtime` without an absolute XDG runtime directory | `CC_PROFILE_RUNTIME_PATH` |
| Temporary base: native process scratch space | OS temporary directory (`$TMPDIR` or `/tmp` on Linux) | `CC_PROFILE_TEMP_PATH` |

All overrides are optional and must be absolute. Setting only `CC_PROFILE_DATA_PATH` changes the default for both the database and new accounts. Data, accounts and runtime directories are private to your user (mode `0700`). The temporary base may be shared: CCP uses a private `cc-profile-<uid>/` inside it. Per-process `agy-<id>/` and `codex-init-<id>/` directories are removed on normal exit. The empty `codex-router-home/` is reused for Codex handshakes. There is no account data to back up in runtime or scratch.

Set any runtime override for **both** the daemon and its launchers (including Garcon) so they find the same `ccpd.sock`. For separate instances, use distinct data and runtime directories and HTTP ports. The launcher **executable** itself can be anywhere on `PATH`.

There is no path migration or socket alias. If you previously set `CC_PROFILE_PATH` or `CC_PROFILE_RUNTIME_DIR`, change your configuration to `CC_PROFILE_DATA_PATH` or `CC_PROFILE_RUNTIME_PATH`, respectively; the old names are ignored. Deploy the launcher and daemon together and restart the daemon: clients now connect only to `ccpd.sock`. Existing accounts, including those in `~/.local/share/cc-profile/profiles/`, continue to use their recorded absolute paths. Nothing is moved or rediscovered. An empty data directory starts an unclaimed panel: run `ccpd set-password` with the same environment before exposing it on a network.

The database holds the salted Argon2id panel password verifier, configs, rotation cursors, mappings, startup records and quota snapshots. Protect backups: proxy credentials are stored under OS permissions, not encrypted by ccpd. The server does not store provider OAuth tokens in SQLite. Upgrading from token pairing preserves router records and requires initial password setup; old tokens are not accepted.

After moving an account directory on the same filesystem, open the existing account and click the pencil (**Edit directory**) beside **Directory**. Enter its new absolute path. CCP verifies the original directory's device, inode and creation time and updates only its registration; the account ID and sticky sessions stay the same. CCP does not move files or redirect already-running clients. A copied directory or a cross-filesystem move has a different identity and cannot be used this way.

Codex 0.172.0 can retain the old absolute rollout path in its own thread index. If Codex reports that the old transcript file does not exist, the CCP owner mapping is still intact. In an offline test, starting `ccp codex app-server --for-session <uuid>` and sending an explicit native `thread/resume` with that `threadId` and the rollout's new absolute `path` in the same account restored subsequent ID-only resumes. Editing the directory does not rewrite the native index.

Existing config directories can be registered in place. Codex and AgY saved filenames can be discovered retrospectively. No transcript contents are read. Startups expire after 90 days; session mappings do not expire, even after a file disappears.

**Upgrade:** Back up the router DB using SQLite's backup API, or stop the service before a filesystem backup. Install matching `ccp` and `ccpd` binaries and restart the daemon. Open fresh AgY ACP and Codex app-server connections after upgrading: their managed launchers require provider-specific v4 endpoints and do not fall back to legacy admission. Other launches retain protocol v3. Existing owners, profiles, rotation and startup history survive; a pending launch without a UUID becomes unobserved. For rollback, restore the pre-upgrade DB with the previous binaries. When upgrading a much older hook-based installation, remove only cc-profile's old `SessionStart` command (`'/path/to/ccpd' observe`, matcher `startup`) from native `hooks.json`. CCP does not change providers' hooks.

## Verification

The root Cargo package builds both executables: [the launcher](../src/bin/ccp.rs) and [the daemon](../src/bin/ccpd/main.rs). Daemon-only HTTP handlers live beside its entry point; both binaries use [the internal library](../src/lib.rs) for the launch protocol. Panel assets are in [web/](../web/), and Rust and Python integration tests are in [tests/](../tests/).

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

All tests use disposable homes. Native smoke tests use no real credentials; they verify UUID assignment/observation and resume, not live subscription balances. Panel assets are embedded, with no runtime Node dependencies. Icons are from Lucide 0.468.0; the [license](../web/LUCIDE-LICENSE.txt) is included.
