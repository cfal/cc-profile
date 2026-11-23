#!/bin/sh
# CCP_TEST_CODEX_PACKAGE is the pinned Linux package directory containing bin/codex.
set -eu
: "${CCP_TEST_CODEX_PACKAGE:?set to the Codex 0.171.0 Linux package root}"
root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
daemon=${CCPD_TEST_BIN:-$root/target/debug/ccpd}
launcher=${CCP_TEST_BIN:-$(dirname "$daemon")/ccp}
mkdir -p "$HOME/tmp"
scratch=$(mktemp -d "$HOME/tmp/ccp-native.XXXXXXXX")
trap 'rm -r -- "$scratch"' EXIT
bwrap --unshare-all --die-with-parent --new-session \
  --ro-bind /usr /usr --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64 \
  --proc /proc --dev /dev --tmpfs /tmp --dir /home/fixture --bind "$scratch" /work \
  --ro-bind "$CCP_TEST_CODEX_PACKAGE" /package --ro-bind "$launcher" /router/ccp --ro-bind "$daemon" /router/ccpd \
  --ro-bind "$(command -v hty)" /qa/hty --ro-bind "$root/tests" /tests --chdir /work \
  --clearenv --setenv PATH /package/bin:/qa:/usr/bin --setenv HOME /home/fixture --setenv TMPDIR /work \
  --setenv PYTHONDONTWRITEBYTECODE 1 --setenv TERM xterm-256color --setenv CCP_TEST_CODEX_OFFLINE 1 \
  --setenv CCPD_TEST_BIN /router/ccpd --setenv CCP_TEST_BIN /router/ccp \
  sh -ec '
    version=$(codex --version)
    [ "$version" = "codex-cli 0.171.0" ] || { echo "Expected codex-cli 0.171.0; got $version" >&2; exit 1; }
    exec python3 /tests/native/test_native.py -v -k codex "$@"
  ' sh "$@"
