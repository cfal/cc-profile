#!/usr/bin/env bash
# Run on a native Linux runner, from the repository root.
set -euo pipefail
target=$1
test "$target" = "$(uname -m)-unknown-linux-musl"

version=$(python3 - <<'PY'
import os
import tomllib
from pathlib import Path

package = tomllib.loads(Path("Cargo.toml").read_text())["package"]
locked = tomllib.loads(Path("Cargo.lock").read_text())["package"]
if [p["version"] for p in locked if p["name"] == package["name"] and "source" not in p] != [package["version"]]:
    raise SystemExit("Cargo.toml and Cargo.lock versions differ")
if os.environ.get("GITHUB_REF_TYPE") == "tag" and os.environ["GITHUB_REF_NAME"] != "v" + package["version"]:
    raise SystemExit("Release tag must be v" + package["version"])
print(package["version"])
PY
)

./docker-build.sh --target "$target" --bins
mkdir -p artifacts
archive="artifacts/cc-profile-$target.tar.gz"
tar -czf "$archive" -C "target/$target/release" ccp ccpd \
    -C "$PWD/web" LUCIDE-LICENSE.txt -C "$PWD/packaging" ccpd.service

smoke=$(mktemp -d)
trap 'rm -rf "$smoke"' EXIT
tar -xzf "$archive" -C "$smoke"
for bin in ccp ccpd; do
    test "$("$smoke/$bin" --version)" = "$bin $version"
    readelf -l -d "$smoke/$bin" > "$smoke/elf.txt"
    if grep -E 'INTERP|NEEDED' "$smoke/elf.txt"; then
        echo "$bin must be statically linked" >&2
        exit 1
    fi
done
CCPD_TEST_BIN="$smoke/ccpd" CCP_TEST_BIN="$smoke/ccp" PYTHONPATH=tests \
    python3 -m unittest test_service.Service.test_claude_exec_stickiness_and_fail_closed_controls
