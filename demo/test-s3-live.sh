#!/usr/bin/env bash
# Live S3 test for md-sync against the user's RustFS service (fs.gns.sh).
# Uses ONLY the md-sync-test/ and md-sync-backup/ prefixes in bucket museai;
# nothing else in the bucket is touched. Cleans both prefixes at start/end.
# Credentials come from ~/workspace/rustfs-s3-config.json via s3tool.py env
# (never echoed: this script never runs with `set -x` and never prints env).
# Usage: demo/test-s3-live.sh [path-to-md-sync-binary]
set -euo pipefail
BIN="${1:-$(dirname "$0")/../target/debug/md-sync}"
DIR="$(dirname "$0")"
ROOT="$(mktemp -d /tmp/md-sync-s3-test.XXXXXX)"
LOCAL="$ROOT/local"
S3="s3://museai/md-sync-test/"
S3BK="s3://museai/md-sync-backup/"
eval "$("$DIR/s3tool.py" env)"

say() { echo "== $*"; }
check() { # check <desc> <cmd...>
  local desc="$1"; shift
  if "$@"; then echo "PASS: $desc"; else echo "FAIL: $desc"; exit 1; fi
}
cleanup() {
  "$DIR/s3tool.py" clean md-sync-test/ >/dev/null
  "$DIR/s3tool.py" clean md-sync-backup/ >/dev/null
}
trap cleanup EXIT

say "0. clean slate"
cleanup
mkdir -p "$LOCAL/notes"

say "1. seed local files, first two-way sync local -> S3"
printf '# Todo\n\n- buy milk\n' > "$LOCAL/notes/todo.md"
printf '# Ideas\n\n- mechanical shiba\n' > "$LOCAL/ideas.md"
printf 'not markdown\n' > "$LOCAL/notes/skip.txt"
"$BIN" sync "$LOCAL" "$S3" --verbose
check "todo.md on S3" bash -c "test \"\$($DIR/s3tool.py exists md-sync-test/notes/todo.md)\" = yes"
check "ideas.md on S3" bash -c "test \"\$($DIR/s3tool.py exists md-sync-test/ideas.md)\" = yes"
check "skip.txt NOT on S3" bash -c "test \"\$($DIR/s3tool.py exists md-sync-test/notes/skip.txt)\" = no"
check "manifest on S3" bash -c "test \"\$($DIR/s3tool.py exists md-sync-test/.md-sync.json)\" = yes"
check "S3 content matches" bash -c "$DIR/s3tool.py get md-sync-test/notes/todo.md | grep -q 'buy milk'"

say "2. second sync is a no-op (no mtime flapping fs <-> S3)"
OUT=$("$BIN" sync "$LOCAL" "$S3")
echo "$OUT"
echo "$OUT" | grep -q "applied 0 ops"

say "3. local edit flows to S3"
sleep 1
printf '# Todo\n\n- buy milk\n- buy coffee\n' > "$LOCAL/notes/todo.md"
"$BIN" sync "$LOCAL" "$S3" >/dev/null
check "S3 has coffee edit" bash -c "$DIR/s3tool.py get md-sync-test/notes/todo.md | grep -q coffee"

say "4. S3-side edit flows back to local (exercises S3 read/stat)"
printf '# Ideas\n\n- mechanical shiba, edited on S3\n' > "$ROOT/s3edit.md"
"$DIR/s3tool.py" put md-sync-test/ideas.md "$ROOT/s3edit.md"
sleep 1
"$BIN" sync "$LOCAL" "$S3" --verbose
check "local has S3 edit" grep -q "edited on S3" "$LOCAL/ideas.md"

say "5. local deletion propagates to S3"
rm "$LOCAL/ideas.md"
"$BIN" sync "$LOCAL" "$S3" >/dev/null
check "ideas.md gone from S3" bash -c "test \"\$($DIR/s3tool.py exists md-sync-test/ideas.md)\" = no"

say "6. plan dry-run changes nothing on S3"
printf '# Plan\n\nnew\n' > "$LOCAL/plan.md"
"$BIN" plan "$LOCAL" "$S3" --verbose | grep -q "plan.md"
check "plan.md NOT on S3 yet" bash -c "test \"\$($DIR/s3tool.py exists md-sync-test/plan.md)\" = no"

say "7. backup to a second prefix (copy-only)"
"$BIN" backup "$LOCAL" "$S3BK" --verbose
check "backup has todo.md" bash -c "test \"\$($DIR/s3tool.py exists md-sync-backup/notes/todo.md)\" = yes"
check "backup has plan.md" bash -c "test \"\$($DIR/s3tool.py exists md-sync-backup/plan.md)\" = yes"

say "8. list via CLI shows S3 files"
"$BIN" list "$S3" | grep -q "notes/todo.md"

say "ALL S3 LIVE CHECKS PASSED (work dir: $ROOT)"
