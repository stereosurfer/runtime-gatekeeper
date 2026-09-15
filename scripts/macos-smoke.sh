#!/bin/sh
# Read-only macOS 27 compatibility smoke test for the user-level deployment.
set -eu

label=${RUNTIME_GATEKEEPER_LABEL:-com.stereosurfer.runtime-gatekeeper}
runtime_dir=${RUNTIME_GATEKEEPER_DIR:-"$HOME/.runtime-gatekeeper"}
port=${RUNTIME_GATEKEEPER_PORT:-47831}
uid=$(id -u)
plist="$runtime_dir/$label.plist"
token_file="$runtime_dir/.runtime/token"
base_url="http://127.0.0.1:$port"

case "$(uname -s)" in
  Darwin) ;;
  *) echo "FAIL: this smoke test requires macOS" >&2; exit 1 ;;
esac

if [ ! -f "$plist" ]; then
  echo "FAIL: missing launchd plist: $plist" >&2
  exit 1
fi
if xattr -p com.apple.quarantine "$plist" >/dev/null 2>&1; then
  echo "FAIL: launchd plist is quarantined; macOS 27 will not load it: $plist" >&2
  exit 1
fi
launchctl print "gui/$uid/$label" >/dev/null

if [ ! -s "$token_file" ]; then
  echo "FAIL: missing daemon token: $token_file" >&2
  exit 1
fi
curl -fsS --max-time 2 "$base_url/" >/dev/null
token=$(cat "$token_file")
status=$(curl -fsS --max-time 5 \
  -H "Authorization: Bearer $token" \
  "$base_url/api/status")
for field in memory services unknown; do
  case "$status" in
    *"\"$field\""*) ;;
    *) echo "FAIL: status response is missing $field" >&2; exit 1 ;;
  esac
done

echo "PASS: macOS $(sw_vers -productVersion) $(sw_vers -buildVersion); launchd, HTTP, token and status are healthy"
