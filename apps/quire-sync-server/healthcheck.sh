#!/bin/bash
# The container's HEALTHCHECK: asks the running server for GET /health, which
# answers `ok` (no token needed; apps/quire-sync-server/src/lib.rs). The image
# has no curl, so this uses bash's /dev/tcp. It reads the address the server
# was told to listen on (QUIRE_SYNC_ADDR) and asks 127.0.0.1 when that is a
# wildcard address. Exit 0: healthy; anything else: not.
set -euo pipefail
addr="${QUIRE_SYNC_ADDR:-127.0.0.1:8787}"
host="${addr%:*}"
port="${addr##*:}"
host="${host#[}"
host="${host%]}"
case "$host" in ''|0.0.0.0|::) host=127.0.0.1 ;; esac
exec 3<> "/dev/tcp/$host/$port"
printf 'GET /health HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n' >&3
response="$(cat <&3)"
# A 200 status and the body `ok`.
case "$response" in
  "HTTP/1.1 200"*|"HTTP/1.0 200"*) ;;
  *) echo "unexpected answer: ${response%%$'\r'*}" >&2; exit 1 ;;
esac
[ "${response##*$'\n'}" = ok ]
