#!/bin/bash
# Run from any directory; no real quota requests or account data.
set -eu
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TEMP="$(mktemp -d)"
trap 'rm -rf "$TEMP"' EXIT
export COSTS_BIN="$TEMP/fake costs"
export CALL_LOG="$TEMP/calls"
cat > "$COSTS_BIN" <<'SH'
#!/bin/sh
printf '%s\n' "$*" >> "$CALL_LOG"
case "$1" in
  quota) printf 'quota table must not pollute the menu\n'; exit "${QUOTA_EXIT:-0}" ;;
  menubar) printf 'menu data\n' ;;
  *) exit 9 ;;
esac
SH
chmod 700 "$COSTS_BIN"

for code in 0 1; do
  : > "$CALL_LOG"
  output="$(QUOTA_EXIT="$code" bash "$ROOT/menubar/costs.5m.sh" 2> "$TEMP/stderr")"
  if [ "$(cat "$CALL_LOG")" != "$(printf 'quota --if-stale\nmenubar')" ]; then
    echo "FAIL: SwiftBar must refresh session quota before rendering the menu" >&2
    exit 1
  fi
  [ "$output" = "menu data" ] || { echo 'FAIL: quota output leaked into menu' >&2; exit 1; }
  echo "PASS SwiftBar refresh exit=$code preserves menu output"
done
