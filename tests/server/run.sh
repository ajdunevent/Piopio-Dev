#!/bin/bash
# Starts a throwaway Prosody on 127.0.0.1:15222 with the accounts the tests use
# (alice, bob, carol, dave, erin, frank, grace, heidi, ivan, judy, kim, leo, mia, nora, owen, pat, quinn, rae, sam, tess and uma @localhost, password "password"), runs the given
# command, then stops the server.
#
#   tests/server/run.sh cargo test -p piopio-daemon --test messaging
set -eu

here=$(cd "$(dirname "$0")" && pwd)
PIOPIO_TEST_SERVER_DIR=$(mktemp -d)
export PIOPIO_TEST_SERVER_DIR
cfg="$PIOPIO_TEST_SERVER_DIR/prosody.cfg.lua"
# prosodyctl only checks this for files it may hand to the prosody user.
export USER="${USER:-$(id -un)}"
mkdir -p "$PIOPIO_TEST_SERVER_DIR/data" "$PIOPIO_TEST_SERVER_DIR/certs"
sed -e "s|@DIR@|$PIOPIO_TEST_SERVER_DIR|g" -e "s|@USER@|$USER|g" \
    "$here/prosody.cfg.lua" >"$cfg"

for user in alice bob carol dave erin frank grace heidi ivan judy kim leo mia nora owen pat quinn rae sam tess uma; do
    if ! out=$(prosodyctl --config "$cfg" register "$user" localhost password 2>&1); then
        echo "Could not create $user@localhost:" >&2
        echo "$out" >&2
        exit 1
    fi
done

prosody --config "$cfg" >"$PIOPIO_TEST_SERVER_DIR/stdout.log" 2>&1 &
pid=$!
cleanup() {
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    rm -rf "$PIOPIO_TEST_SERVER_DIR"
}
trap cleanup EXIT INT TERM

i=0
until (echo >/dev/tcp/127.0.0.1/15222) 2>/dev/null; do
    i=$((i + 1))
    if [ "$i" -gt 100 ]; then
        echo "Prosody did not start:" >&2
        cat "$PIOPIO_TEST_SERVER_DIR/stdout.log" "$PIOPIO_TEST_SERVER_DIR/prosody.log" >&2 || true
        exit 1
    fi
    sleep 0.1
done

export PIOPIO_TEST_SERVER=127.0.0.1:15222
status=0
"$@" || status=$?
if [ "$status" -ne 0 ] && [ -n "${PIOPIO_SHOW_SERVER_LOG:-}" ]; then
    tail -n 200 "$PIOPIO_TEST_SERVER_DIR/prosody.log" >&2
fi
exit "$status"
