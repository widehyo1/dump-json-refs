#!/bin/bash
set -euo pipefail

show_help() {
  cat <<'EOF'
Compare two dump-json-refs SQLite indexes.

Usage:
  compare-dump-json-refs-sqlite.sh <DIR1> <DIR2>
  compare-dump-json-refs-sqlite.sh <DB1.sqlite> <DB2.sqlite>

Examples:
  compare-dump-json-refs-sqlite.sh refs-a refs-b
  compare-dump-json-refs-sqlite.sh dir1/schemas.sqlite dir2/schemas.sqlite
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

resolve_db() {
  local input="$1"

  if [ -d "$input" ]; then
    echo "$input/schemas.sqlite"
  else
    echo "$input"
  fi
}

quote_ident() {
  # SQLite identifier quoting for generated SQL.
  # Double quotes inside identifiers are escaped by doubling them.
  printf '"%s"' "$(printf '%s' "$1" | sed 's/"/""/g')"
}

dump_schema() {
  local db="$1"

  sqlite3 "$db" <<'SQL'
.headers off
.mode list
SELECT type || '|' || name || '|' || tbl_name || '|' || COALESCE(sql, '')
FROM sqlite_schema
WHERE type IN ('table', 'index', 'trigger', 'view')
  AND name NOT LIKE 'sqlite_%'
ORDER BY type, name, tbl_name, sql;
SQL
}

dump_table_counts() {
  local db="$1"

  sqlite3 -readonly "$db" <<'SQL' |
.headers off
.mode list
SELECT name
FROM sqlite_schema
WHERE type = 'table'
  AND name NOT LIKE 'sqlite_%'
ORDER BY name;
SQL
  while IFS= read -r table; do
    [ -n "$table" ] || continue
    local qtable
    qtable="$(quote_ident "$table")"
    local count
    count="$(sqlite3 -readonly "$db" "SELECT COUNT(*) FROM $qtable;")"
    printf '%s|%s\n' "$table" "$count"
  done
}

dump_table_info() {
  local db="$1"

  sqlite3 -readonly "$db" <<'SQL' |
.headers off
.mode list
SELECT name
FROM sqlite_schema
WHERE type = 'table'
  AND name NOT LIKE 'sqlite_%'
ORDER BY name;
SQL
  while IFS= read -r table; do
    [ -n "$table" ] || continue
    local qtable
    qtable="$(quote_ident "$table")"

    sqlite3 -readonly "$db" <<SQL
.headers off
.mode list
SELECT '$table|column|' || cid || '|' || name || '|' || type || '|' || "notnull" || '|' || COALESCE(dflt_value, '') || '|' || pk
FROM pragma_table_info($qtable)
ORDER BY cid;

SELECT '$table|fk|' || id || '|' || seq || '|' || "table" || '|' || "from" || '|' || "to" || '|' || on_update || '|' || on_delete || '|' || "match"
FROM pragma_foreign_key_list($qtable)
ORDER BY id, seq;

SELECT '$table|index|' || name || '|' || "unique" || '|' || origin || '|' || partial
FROM pragma_index_list($qtable)
ORDER BY name;
SQL
  done
}

dump_canonical_data() {
  local db="$1"

  # .dump output is normalized enough for same SQLite logical data comparison
  # as long as insert order is stable. dump-json-refs should generate deterministic tables.
  sqlite3 -readonly "$db" ".dump" \
    | grep -v '^PRAGMA foreign_keys=OFF;' \
    | grep -v '^BEGIN TRANSACTION;' \
    | grep -v '^COMMIT;' \
    | grep -v '^sqlite_sequence'
}

compare_section() {
  local label="$1"
  local file1="$2"
  local file2="$3"

  echo "== $label =="

  if diff -u "$file1" "$file2"; then
    echo "OK: $label matched"
  else
    echo "FAIL: $label differs"
    return 1
  fi

  echo
}

main() {
  if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    show_help
    exit 0
  fi

  [ "$#" -eq 2 ] || {
    show_help >&2
    exit 2
  }

  need_cmd sqlite3
  need_cmd diff
  need_cmd grep
  need_cmd sed
  need_cmd mktemp

  local db1 db2
  db1="$(resolve_db "$1")"
  db2="$(resolve_db "$2")"

  [ -f "$db1" ] || die "SQLite file not found: $db1"
  [ -f "$db2" ] || die "SQLite file not found: $db2"

  sqlite3 "$db1" 'PRAGMA integrity_check;' | grep -qx 'ok' || die "integrity_check failed: $db1"
  sqlite3 "$db2" 'PRAGMA integrity_check;' | grep -qx 'ok' || die "integrity_check failed: $db2"

  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  echo "DB1: $db1"
  echo "DB2: $db2"
  echo

  dump_schema "$db1" > "$tmp/schema.1"
  dump_schema "$db2" > "$tmp/schema.2"

  dump_table_info "$db1" > "$tmp/table-info.1"
  dump_table_info "$db2" > "$tmp/table-info.2"

  dump_table_counts "$db1" > "$tmp/counts.1"
  dump_table_counts "$db2" > "$tmp/counts.2"

  dump_canonical_data "$db1" > "$tmp/data.1"
  dump_canonical_data "$db2" > "$tmp/data.2"

  local failed=0

  compare_section "sqlite_schema" "$tmp/schema.1" "$tmp/schema.2" || failed=1
  compare_section "table_info / foreign_keys / indexes" "$tmp/table-info.1" "$tmp/table-info.2" || failed=1
  compare_section "table row counts" "$tmp/counts.1" "$tmp/counts.2" || failed=1
  compare_section "canonical dump data" "$tmp/data.1" "$tmp/data.2" || failed=1

  if [ "$failed" -eq 0 ]; then
    echo "PASS: SQLite indexes are equivalent."
  else
    echo "FAIL: SQLite indexes differ."
    echo "Temporary diff inputs were removed. Re-run with bash -x if deeper tracing is needed."
    exit 1
  fi
}

main "$@"
