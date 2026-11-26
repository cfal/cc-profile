# cc-profile

Use multiple Claude Code, Codex, or Antigravity accounts on one Linux machine without switching logins between chats. Run `ccp codex`, `ccp claude`, or `ccp agy` in your project. A new chat uses an eligible account for **that provider**; resuming a known session uses its original account or fails. CCP never silently starts a replacement chat on another account.

`ccp` is the launcher. `ccpd` is the local router and serves the web panel. You still use the real provider CLI, its sign-in flow, and your existing subscription. You only need to install the providers you use.

## Quickstart

### 1. Install and start the router

Install at least one native CLI (`claude`, `codex`, or `agy`) and put it on your `PATH`. Already have matching Linux binaries for your architecture? Put both `ccp` and `ccpd` on your `PATH` and run `ccpd`. Otherwise, use the [Docker static build](#static-build) or build from source below with current stable Rust, a C toolchain, `pkg-config`, and SQLite development headers (`libsqlite3-dev` on Debian/Ubuntu).

```sh
git clone https://github.com/cfal/cc-profile.git
cd cc-profile
CARGO_BUILD_JOBS=1 cargo install --locked --path . --root "$HOME/.local"
export PATH="$HOME/.local/bin:$PATH"
ccpd
```

Leave `ccpd` running. Open **[http://127.0.0.1:8743](http://127.0.0.1:8743)** and set the panel password. This password protects the router; it is **not** a provider login. If the router is on a VM, run `ssh -L 8743:127.0.0.1:8743 user@your-vm` on your own machine and open the same URL locally. Do not expose an unclaimed panel or send its password over public HTTP.

To skip panel setup and sign-in, start `ccpd --no-auth`. Anyone who can reach the panel can then manage your router and accounts. Use this only on a trusted machine or behind an authenticated proxy. Restart without the flag to restore password protection.

### 2. Add your first account

1. Open **Accounts → Add profile**. Choose your provider, give it a label such as `Work`, and select **Create new profile**. To use an existing native config directory without copying it, select **Use existing directory** instead.
2. Sign in to the selected profile. For **Codex**, click **Sign in with ChatGPT** and complete verification in your browser. For **Claude** or **Antigravity**, copy the displayed `ccp login` command and run it in another terminal on the router machine. Codex also offers the terminal option.
3. Turn on **Enable profile**. Choose **Set as default** if you also want unscoped maintenance commands, such as `ccp agy models`, to use this account. An account need not be the default to receive new chats.

### 3. Start a chat

In another terminal on the same machine, open your project and run **one** command for the provider you configured. If the shell cannot find `ccp`, export `PATH="$HOME/.local/bin:$PATH"` in that terminal too.

```sh
ccp codex
ccp claude
ccp agy
```

The native CLI opens in your terminal. In **Dashboard → Startup logs**, check the account label and routing decision. To see the selection in the terminal instead, run, for example, `CC_PROFILE_DEBUG=1 ccp codex`.

That is all you need for one account. To try rotation, add and enable a second account **for the same provider**, choose **Settings → New chats → Round robin**, save the mode, and start two **new** chats. The startup log will show which account handled each. Resumes do not advance rotation.

## Launch and resume

| Task | Claude Code | Codex | Antigravity |
| --- | --- | --- | --- |
| Start an interactive chat | `ccp claude` | `ccp codex` | `ccp agy` |
| Run a one-shot | `ccp claude -p 'Summarize this project'` | `ccp codex exec --ephemeral 'Summarize this project'` | `ccp agy -p 'Summarize this project'` |
| Resume a terminal session | `ccp claude --resume <uuid>` | `ccp codex resume <uuid>` | `ccp agy --conversation <uuid>` |
| Check the default account | `ccp claude auth status` | `ccp codex login status` | `ccp agy models` (catalog access) |

**Sessions** is the easiest way to resume: find the session, copy its command, and run it from the intended workspace. AgY 1.5.16+ also accepts `--resume <uuid>` and `-r <uuid>`. Use an exact ID, not a picker or “most recent”: different accounts have different histories. For Codex and AgY, a startup may show **Unknown** even after a saved session appears. CCP can identify the account from the saved file without knowing which terminal created it.

**For editors and other clients:** configure the executable as `ccp` with arguments `["agy", "acp"]` or `["codex", "app-server"]` for the corresponding protocol. Clients that expect a provider-named executable can use a [launcher symlink on PATH](docs/reference.md#shells-and-automation). Leave the genuine provider binary installed; don't replace it with the symlink. See the [Garcon setup](docs/reference.md#garcon-executors) for executor settings and history limitations.

## Routing and quota

- **New chats:** The default mode, **Earliest expiry, most quota remaining**, prefers the soonest known quota reset, then the most remaining quota on a tie. **Settings** also offers the opposite tie-break, most or least quota remaining, round robin, and highest priority. Quota modes use verified Codex limits, Claude’s reported account limits, or the relevant AgY model pool (Gemini by default). A new selection waits up to two minutes total for stale readings. [Routing rules](docs/reference.md#choosing-a-profile)
- **Resumes:** A tracked session stays with its original account. Disabling that account blocks future launches and resumes without stopping chats already running. Re-enable it to resume; changing the default account does not transfer sessions.
- **Default account:** One optional default per provider, for commands that don't name a profile and don't start a chat. For a *specific* account, use the panel's numeric profile ID: `ccp login <id> --status`, or `ccp usage <id>` for native AgY quota. The profile ID and the native session UUID are different.
- **Quota refresh:** Dashboard and Accounts show remaining quota, reset times, and stale readings. While running, `ccpd` refreshes enabled accounts when their last successful read reaches 24 hours, even with the panel closed. Opening either page checks readings at least two minutes old; a successful refresh restarts the 24-hour clock. **Refresh quota** targets one account. Unknown does not mean empty. [Quota, proxies and Codex reset grants](docs/reference.md#proxies-and-quota)

An account can **Inherit** proxy settings, use a **Custom** proxy, or connect **Direct**. Inherit uses the launcher's environment for chats and the daemon's environment for panel checks. The browser used for login has its own network settings.

## Install and operate

### Static build

From the repository root, build a matched pair of Linux executables using Docker. Rust, Python, Node, and system SQLite are not required to run the static CCP binaries; the provider CLIs still have their own requirements.

```sh
CARGO_BUILD_JOBS=1 ./docker-build.sh --bins
# On x86-64:
release=target/x86_64-unknown-linux-musl/release
install -d "$HOME/.local/bin"
install -m 755 "$release/ccp" "$release/ccpd" "$HOME/.local/bin/"
```

The release profile uses `opt-level = 3`, fat LTO and stripped symbols. Panel assets are embedded in `ccpd`; there is no web build or separate asset directory to deploy. Start `ccpd` as in the [quickstart](#quickstart).

### Run as a service

For a persistent setup, stop the foreground daemon and install the included systemd user unit. It expects the binaries under `~/.local/bin`:

```sh
install -Dm644 packaging/ccpd.service "$HOME/.config/systemd/user/ccpd.service"
systemctl --user daemon-reload
systemctl --user enable --now ccpd
journalctl --user -u ccpd -n 10
```

Run the daemon and launcher as the **same Linux user**. The daemon does not source your shell configuration. See [service PATH and native binary overrides](docs/reference.md#user-service) if your tools come from a version manager. For remote access, prefer the SSH tunnel above or a TLS reverse proxy. [Panel access and password reset](docs/reference.md#panel-access)

## Storage and recovery

| Variable | Default | What goes there |
| --- | --- | --- |
| `CC_PROFILE_DATA_PATH` | `$XDG_CONFIG_HOME/cc-profile`, otherwise `~/.config/cc-profile` | Router database, profile registrations, session ownership and settings |
| `CC_PROFILE_ACCOUNTS_PATH` | `<data>/accounts` | New native profiles, one directory per `<tool>/<uuid>`; includes credentials and history |
| `CC_PROFILE_RUNTIME_PATH` | `$XDG_RUNTIME_DIR/cc-profile`, otherwise `<data>/runtime` | Private launcher socket (`ccpd.sock`) and locks |
| `CC_PROFILE_TEMP_PATH` | OS temporary directory | Short-lived worker directories under a private per-user subdirectory |

Overrides must be absolute paths. Set them consistently in your shell and service. Changing the accounts root affects **new** profiles; it does not move existing registrations.

**Moving a profile:** Close native clients using it, move the directory **on the same filesystem**, then open **Accounts → Edit directory** (pencil beside the path) on the original account. CCP verifies the directory identity and keeps its session mappings. A cross-filesystem `mv` makes a copy with a different identity; it is not supported by **Edit directory**.

**Backups and upgrades:** Back up both the router database and the native profile directories, including any registered outside the accounts root. Stop the daemon for a filesystem backup of its database, or use SQLite's backup API. Stop native writers before backing up their profiles. Install matching `ccp` and `ccpd` binaries, restart the daemon and reconnect protocol clients. Backups can contain credentials; keep them private. [Upgrade and recovery details](docs/reference.md#storage-and-recovery)

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| Launcher cannot reach the router | Start `ccpd`. Check that the daemon and shell use the same user and runtime path. Opening the panel over HTTP does not connect a remote launcher. |
| Native binary missing or wrong after an upgrade | Open **Settings → Binary versions** and refresh. Check both PATHs, including `node` for npm installs. The real executable must not be a link back to CCP. |
| New chats work, but status or model discovery fails | Set an enabled **Default** account for that provider. For AgY, **Refresh sign-in status** checks the selected profile's catalog; a local login file alone does not prove access. |
| A resume is refused | Check the exact ID in **Sessions**, that the owner is enabled and still points to the original directory, and that the native saved session still exists. Don't register a replacement account to fix a sticky session. |
| Commands see a different home under AgY | Plain AgY chats, print, models and quota use the account's `HOME`. ACP and ACP login preserve the caller's `HOME` and `XDG_CONFIG_HOME` for tools; storage flags select the Google account. ACP profiles can share plugin credentials. See [Antigravity on Linux](docs/antigravity.md). |
| Forgot the panel password | Run `ccpd set-password` as the daemon user with the same data path. This signs out browsers, not native agents. |

See the [full reference](docs/reference.md) for supported invocation forms, protocol browse limits, provider-specific behavior, and [verification commands](docs/reference.md#verification).
