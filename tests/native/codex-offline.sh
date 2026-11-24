#!/bin/sh
# CCP_TEST_CODEX_PACKAGE is the native Linux package directory containing bin/codex.
# Add CCP_TEST_GARCON=/path/to/garcon for the no-turn Garcon transport suite (requires Bun).
set -eu
: "${CCP_TEST_CODEX_PACKAGE:?set to the native Codex Linux package root}"
root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
daemon=${CCPD_TEST_BIN:-$root/target/debug/ccpd}
launcher=${CCP_TEST_BIN:-$(dirname "$daemon")/ccp}
mkdir -p "$HOME/tmp"
scratch=$(mktemp -d "$HOME/tmp/ccp-native.XXXXXXXX")
trap 'rm -r -- "$scratch"' EXIT
if [ -n "${CCP_TEST_GARCON:-}" ]; then
  set -- --ro-bind "$CCP_TEST_GARCON" /garcon --ro-bind "$(command -v bun)" /qa/bun \
    --setenv CCP_TEST_GARCON /garcon --setenv PYTHONPATH /tests:/tests/native \
    python3 -m unittest -v test_garcon_codex test_native.Native.test_codex_deferred_and_explicit_native_handshakes \
    test_launcher.Launcher.test_codex_is_the_native_process_for_chats_and_exact_resumes "$@"
else
  set -- --ro-bind "$(command -v hty)" /qa/hty sh -ec '
    codex --version
    exec python3 /tests/native/test_native.py -v -k codex "$@"
  ' sh "$@"
fi
bwrap --unshare-all --die-with-parent --new-session \
  --ro-bind /usr /usr --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64 \
  --proc /proc --dev /dev --tmpfs /tmp --dir /home/fixture --bind "$scratch" /work \
  --ro-bind "$CCP_TEST_CODEX_PACKAGE" /package --ro-bind "$launcher" /router/ccp --ro-bind "$daemon" /router/ccpd \
  --ro-bind "$root/tests" /tests --chdir /work \
  --clearenv --setenv PATH /package/bin:/qa:/usr/bin --setenv HOME /home/fixture --setenv TMPDIR /work \
  --setenv PYTHONDONTWRITEBYTECODE 1 --setenv TERM xterm-256color --setenv CCP_TEST_CODEX_OFFLINE 1 \
  --setenv CCPD_TEST_BIN /router/ccpd --setenv CCP_TEST_BIN /router/ccp "$@"
