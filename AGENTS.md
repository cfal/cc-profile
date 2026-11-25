# Working on cc-profile

CCP routes installed native CLIs; it does not implement their login or inference APIs. Keep launcher and daemon changes in agreement: `src/invocation.rs` classifies argv, the daemon selects an account, and the launcher validates the plan before starting the child. A new chat may rotate. A resume must use its verified owner or fail, never silently become a new chat. The Default account is for unscoped maintenance, not for guessing ownership.

## Antigravity references

- Start with Google's [CLI changelog](https://antigravity.google/docs/changelog?tab=cli) for new flags and behavior. Check it whenever changing AgY integration or testing a native upgrade. Compare the installed native binary's root, `acp`, and `models` help separately; an `agy` on PATH may be CCP's launcher symlink. Do not run release checks on the user's launch path or reject a build solely for a new version number.
- The [Python SDK](https://github.com/google-antigravity/antigravity-sdk-python) is useful for understanding persistence, **not** as an argv or credential contract for `agy`. Its [local connection](https://github.com/google-antigravity/antigravity-sdk-python/blob/97c5c53d0741c601ea8c2401497cad3b5ee8dc98/google/antigravity/connections/local/local_connection.py) starts a different executable, `localharness`. Its [app data example](https://github.com/google-antigravity/antigravity-sdk-python/blob/97c5c53d0741c601ea8c2401497cad3b5ee8dc98/examples/getting_started/app_data_dir_override.py) uses an absolute directory for artifacts; its [persistence example](https://github.com/google-antigravity/antigravity-sdk-python/blob/97c5c53d0741c601ea8c2401497cad3b5ee8dc98/examples/getting_started/persistence.py) reuses both the trajectory `save_dir` and the assigned conversation ID. Neither example redirects CLI OAuth or defines ACP's `--app-data-dir`.
- [Antigravity on Linux](docs/antigravity.md) records CCP's verified behavior and limits. Keep it in sync with native evidence. Record the version, command, and whether a result came from a fixture, a signed-out native run, or a signed-in test.

## Integration rules

- Select storage by native command, not by client name. Keep the decision in `agy::Runtime::configure` so ACP login, interactive clients and daemon browse workers agree. On 1.5.17, ACP takes an absolute `--gemini-dir` and a relative `--app-data-dir`. Preserve the spawning process's `HOME` and `XDG_CONFIG_HOME` (including unset) for its tools; the ACP plugin secret file can therefore be shared across profiles, unlike the Google login. For TUI, print, models and quota, select the profile via child `HOME` and unset `XDG_CONFIG_HOME`. Keep the bus-less runtime in both cases. Do not globally switch symlinks, inherit a desktop keyring, or retry against the developer's account.
- Parse aliases as options, not by searching argv strings: a prompt or an option operand can contain a flag name. AgY terminal `--conversation`, `--resume` and `-r` select an exact saved UUID; ACP session IDs are opaque. A saved file establishes account ownership, not which startup created it. Never read transcript or credential contents to infer ownership.

## Verification

Use disposable caller and account homes. Test at least two accounts, including an exact resume when the other account is next to rotate. Check the native child actually used the intended paths, not merely that it accepted a flag. Keep provider calls, real credentials and the desktop bus out of automated tests. A signed-out smoke cannot prove authenticated restore or token renewal.

```sh
export CARGO_BUILD_JOBS=1
cargo test --locked --offline
cargo build --locked --offline --bins
export CCPD_TEST_BIN="$PWD/target/debug/ccpd"
python3 -m unittest discover -s tests -p test_agy.py
CCP_TEST_AGY=/absolute/path/to/native/agy python3 tests/native/test_agy.py
```

The optional Linux native suite requires `bwrap`; its TUI test also uses `hty`. Use `$HOME/tmp` for temporary checkouts on this VM and clean them up. Do not replace installed launchers, restart the user's daemon, or touch live accounts during fixture testing.
