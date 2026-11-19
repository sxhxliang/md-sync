#!/usr/bin/env bash
# End-to-end demo + verification for md-sync (fs backend).
# Usage: demo/run-demo.sh [path-to-md-sync-binary]
set -euo pipefail
BIN="${1:-$(dirname "$0")/../target/debug/md-sync}"
ROOT="$(mktemp -d /tmp/md-sync-demo.XXXXXX)"
A="$ROOT/a"; B="$ROOT/b"; BK="$ROOT/backup"
mkdir -p "$A/notes" "$B/notes" "$BK"
say() { echo "== $*"; }
check() { # check <desc> <cmd...>
  local desc="$1"; shift
  if "$@"; then echo "PASS: $desc"; else echo "FAIL: $desc"; exit 1; fi
}
diff_files() { diff -r --exclude=.md-sync.json --exclude='*.txt' "$1" "$2" >/dev/null; }

say "1. seed two folders"
printf '# Todo\n\n- buy milk\n' > "$A/notes/todo.md"
printf '# Ideas\n\n- mechanical shiba\n' > "$A/ideas.md"
printf '# Draft\n\nonly on B\n' > "$B/notes/draft.md"
printf 'not markdown\n' > "$A/notes/skip.txt"

say "2. first two-way sync (A-only and B-only files cross over, .txt ignored)"
"$BIN" sync "$A" "$B" --verbose
check "todo.md arrived on B" test -f "$B/notes/todo.md"
check "ideas.md arrived on B" test -f "$B/ideas.md"
check "draft.md arrived on A" test -f "$A/notes/draft.md"
check "skip.txt NOT synced" test ! -f "$B/notes/skip.txt"
check "folders identical (ignoring manifest)" diff_files "$A" "$B"

say "3. second sync is a no-op"
OUT=$("$BIN" sync "$A" "$B")
echo "$OUT"
echo "$OUT" | grep -q "applied 0 ops"

say "4. one-sided edits + a deletion propagate"
sleep 1
printf '# Todo\n\n- buy milk\n- buy coffee\n' > "$A/notes/todo.md"
printf '# Draft\n\nonly on B, edited\n' > "$B/notes/draft.md"
rm "$B/ideas.md"
"$BIN" sync "$A" "$B" --verbose
check "todo edit reached B" grep -q coffee "$B/notes/todo.md"
check "draft edit reached A" grep -q edited "$A/notes/draft.md"
check "ideas.md deletion propagated to A" test ! -f "$A/ideas.md"

say "5. conflict: both sides edit the same file"
sleep 1
printf '# Todo\n\nA version\n' > "$A/notes/todo.md"
sleep 1
printf '# Todo\n\nB version, newer\n' > "$B/notes/todo.md"
"$BIN" sync "$A" "$B" --conflict newer --verbose
check "newer (B) won on A" grep -q "B version" "$A/notes/todo.md"

say "6. conflict --conflict both keeps the loser as a .conflict file"
sleep 1; printf '# Todo\n\nA again\n' > "$A/notes/todo.md"
sleep 1; printf '# Todo\n\nB again\n' > "$B/notes/todo.md"
"$BIN" sync "$A" "$B" --conflict both --verbose
check "a .conflict-*.md file exists on both sides" \
  bash -c "ls $A/notes/*.conflict-*.md $B/notes/*.conflict-*.md >/dev/null"
check "loser content preserved in conflict copy" \
  bash -c "grep -rq 'A again' $A/notes/*.conflict-*.md $B/notes/*.conflict-*.md"
check "winner (B) is canonical" grep -q "B again" "$A/notes/todo.md"
rm -f "$A"/notes/*.conflict-*.md "$B"/notes/*.conflict-*.md
"$BIN" sync "$A" "$B" >/dev/null  # re-converge after removing conflict copies

say "7. plan is a dry run (nothing changes)"
printf '# Plan\n\nnew file\n' > "$A/plan.md"
"$BIN" plan "$A" "$B" --verbose | grep -q "plan.md"
check "plan.md NOT copied by plan" test ! -f "$B/plan.md"
"$BIN" sync "$A" "$B" >/dev/null
check "sync copies it" test -f "$B/plan.md"

say "8. push mirrors A -> B (deletes extras on B)"
printf 'extra\n' > "$B/extra.md"
"$BIN" push "$A" "$B" --verbose
check "extra.md deleted on B" test ! -f "$B/extra.md"

say "9. backup never deletes and never overwrites a newer dest file"
printf '# Todo\n\nbackup source, older\n' > "$A/notes/todo.md"
touch -d "2020-01-01" "$A/notes/todo.md"
"$BIN" backup "$A" "$BK"
check "backup has todo.md" test -f "$BK/notes/todo.md"
printf '# Todo\n\nsource changed again\n' > "$A/notes/todo.md"
sleep 1; printf '# Todo\n\nedited in backup, newer\n' > "$BK/notes/todo.md"
printf 'stale\n' > "$BK/stale.md"
"$BIN" backup "$A" "$BK" --verbose
check "newer backup file protected" grep -q "edited in backup" "$BK/notes/todo.md"
check "stale file kept in backup" test -f "$BK/stale.md"

say "10. list + json report"
"$BIN" list "$A" | grep -q "todo.md"
"$BIN" sync "$A" "$B" --dry-run --json | grep -q '"mode": "TwoWay"'

say "11. bidirectional: stateless two-way (no deletion, one-sided change is a conflict)"
C="$ROOT/c"; D="$ROOT/d"; mkdir -p "$C" "$D"
printf '# Shared\n\nv1\n' > "$C/shared.md"
printf '# Only C\n' > "$C/only-c.md"
printf '# Only D\n' > "$D/only-d.md"
"$BIN" sync "$C" "$D" --mode bidirectional --verbose
check "only-c reached D" test -f "$D/only-c.md"
check "only-d reached C" test -f "$C/only-d.md"
# Delete on D only: bidirectional must copy it back, never delete from C.
rm "$D/shared.md"
"$BIN" sync "$C" "$D" --mode bidirectional
check "shared.md copied back to D, not deleted from C" \
  bash -c "test -f $D/shared.md && test -f $C/shared.md"
# One-sided edit after a baseline: two-way would copy silently,
# bidirectional reports a conflict (and the newer side still wins).
sleep 1; printf '# Shared\n\nv2 on C\n' > "$C/shared.md"
OUT=$("$BIN" sync "$C" "$D" --mode bidirectional --conflict newer --verbose)
echo "$OUT"
echo "$OUT" | grep -q "conflict shared.md"
check "newer (C) won on D" grep -q "v2 on C" "$D/shared.md"

say "12. mirror: destination is made to match the source"
printf 'extra\n' > "$D/extra.md"
"$BIN" sync "$C" "$D" --mode mirror --verbose
check "extra.md deleted on D by mirror" test ! -f "$D/extra.md"
check "mirror kept shared.md in sync" grep -q "v2 on C" "$D/shared.md"

say "ALL DEMO CHECKS PASSED (work dir: $ROOT)"
