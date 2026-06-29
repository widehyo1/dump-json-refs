#!/bin/bash
set -euo pipefail

# Regression test for:
#   dump-json-refs cargo-installed baseline
#   dump-json-refs candidate implementation
#
# Required:
#   sqlite3
#   diff
#   cargo-installed dump-json-refs baseline
#   candidate dump-json-refs binary

show_help() {
  cat <<'EOF_HELP'
Compare dump-json-refs CLI behavior between the cargo baseline and candidate implementation.

Usage:
  regression_refs_cli_equivalence.sh [TEMP_JSON]

Environment:
  CARGO_DUMP_JSON_REFS_BIN cargo-installed baseline binary. Default: /home/widehyo/.cargo/bin/dump-json-refs
  DUMP_JSON_REFS_BIN       candidate binary name/path. Default: target/debug/dump-json-refs
  KEEP_TMP                 if set to 1, keep temporary test directory

Examples:
  ./regression_refs_cli_equivalence.sh
  ./regression_refs_cli_equivalence.sh temp.json

  CARGO_DUMP_JSON_REFS_BIN=/home/widehyo/.cargo/bin/dump-json-refs \
  DUMP_JSON_REFS_BIN=target/debug/dump-json-refs \
  ./regression_refs_cli_equivalence.sh temp.json
EOF_HELP
}

die() {
  echo "error: $*" >&2
  exit 1
}

need_cmd() {
  local cmd="$1"
  case "$cmd" in
    */*) [ -x "$cmd" ] || die "required executable not found: $cmd" ;;
    *) command -v "$cmd" >/dev/null 2>&1 || die "required command not found: $cmd" ;;
  esac
}

resolve_cmd_path() {
  local cmd="$1"
  case "$cmd" in
    */*)
      local dir base
      dir="$(cd "$(dirname "$cmd")" && pwd)"
      base="$(basename "$cmd")"
      printf '%s/%s\n' "$dir" "$base"
      ;;
    *)
      printf '%s\n' "$cmd"
      ;;
  esac
}

normalize_text() {
  # This test compares two implementations of the same CLI contract.
  # Only elapsed/execution time is intentionally unstable.
  sed \
    -e '/^execution_time_ms[[:space:]]/d' \
    -e '/^elapsed_time_ms[[:space:]]/d' \
    -e 's/[[:space:]]\+$//' \
    -e '/^$/N;/^\n$/D'
}

escape_sed_literal() {
  printf '%s' "$1" | sed 's/[#&\]/\\&/g'
}

normalize_allowed_outdir_prefix() {
  local old_outdir="$1"
  local new_outdir="$2"

  if [ "$old_outdir" = "$new_outdir" ]; then
    cat
    return 0
  fi

  local old_prefix
  local new_prefix
  old_prefix="$(escape_sed_literal "$old_outdir/")"
  new_prefix="$(escape_sed_literal "$new_outdir/")"
  sed "s#${old_prefix}#${new_prefix}#g"
}

quote_case_name() {
  printf '%s' "$1" | tr -cs 'A-Za-z0-9._-' '_'
}

resolve_sqlite_dir() {
  local workdir="$1"
  local outdir="$2"

  if [ -f "$workdir/$outdir/schemas.sqlite" ]; then
    printf '%s\n' "$workdir/$outdir"
  elif [ -f "$workdir/refs/schemas.sqlite" ]; then
    printf '%s\n' "$workdir/refs"
  else
    return 1
  fi
}

report_path_for_args() {
  local default_path="$1"
  shift

  while [ "$#" -gt 0 ]; do
    case "$1" in
      -o|--output)
        [ "$#" -ge 2 ] || die "missing report output path after $1"
        printf '%s\n' "$2"
        return 0
        ;;
      --output=*)
        printf '%s\n' "${1#--output=}"
        return 0
        ;;
    esac
    shift
  done

  printf '%s\n' "$default_path"
}

outdir_for_args() {
  local default_outdir="$1"
  shift

  while [ "$#" -gt 0 ]; do
    case "$1" in
      --outdir)
        [ "$#" -ge 2 ] || die "missing output directory after --outdir"
        printf '%s\n' "$2"
        return 0
        ;;
      --outdir=*)
        printf '%s\n' "${1#--outdir=}"
        return 0
        ;;
    esac
    shift
  done

  printf '%s\n' "$default_outdir"
}

sqlite_path_from_args() {
  local default_path="$1"
  shift

  while [ "$#" -gt 0 ]; do
    case "$1" in
      --from-sqlite)
        if [ "$#" -ge 2 ]; then
          case "$2" in
            -*)
              printf '%s\n' "$default_path"
              return 0
              ;;
            *)
              printf '%s\n' "$2"
              return 0
              ;;
          esac
        fi
        printf '%s\n' "$default_path"
        return 0
        ;;
      --from-sqlite=*)
        printf '%s\n' "${1#--from-sqlite=}"
        return 0
        ;;
    esac
    shift
  done

  printf '%s\n' "$default_path"
}

run_baseline_file() {
  local cwd="$1"
  local input="$2"
  shift 2

  (
    cd "$cwd"
    "$CARGO_DUMP_JSON_REFS_BIN" "$input" "$@"
  )
}

run_candidate_file() {
  local cwd="$1"
  local input="$2"
  shift 2

  (
    cd "$cwd"
    "$DUMP_JSON_REFS_BIN" "$input" "$@"
  )
}

run_baseline_stdin() {
  local cwd="$1"
  local input="$2"
  shift 2

  (
    cd "$cwd"
    cat "$input" | "$CARGO_DUMP_JSON_REFS_BIN" "$@"
  )
}

run_candidate_stdin() {
  local cwd="$1"
  local input="$2"
  shift 2

  (
    cd "$cwd"
    cat "$input" | "$DUMP_JSON_REFS_BIN" "$@"
  )
}

run_baseline_from_sqlite() {
  local cwd="$1"
  shift

  (
    cd "$cwd"
    "$CARGO_DUMP_JSON_REFS_BIN" "$@"
  )
}

run_candidate_from_sqlite() {
  local cwd="$1"
  shift

  (
    cd "$cwd"
    "$DUMP_JSON_REFS_BIN" "$@"
  )
}

compare_file_normalized() {
  local label="$1"
  local old_file="$2"
  local new_file="$3"
  local diff_dir="$4"
  local old_outdir="${5:-}"
  local new_outdir="${6:-$old_outdir}"

  [ -f "$old_file" ] || {
    echo "FAIL: $label missing in cargo baseline: $old_file"
    return 1
  }

  [ -f "$new_file" ] || {
    echo "FAIL: $label missing in candidate implementation: $new_file"
    return 1
  }

  normalize_text < "$old_file" \
    | normalize_allowed_outdir_prefix "$old_outdir" "$new_outdir" \
    > "$diff_dir/$label.old.normalized"
  normalize_text < "$new_file" > "$diff_dir/$label.new.normalized"

  if diff -u "$diff_dir/$label.old.normalized" "$diff_dir/$label.new.normalized"; then
    echo "OK: $label matched"
  else
    echo "FAIL: $label differs"
    return 1
  fi
}

compare_optional_normalized_file() {
  local label="$1"
  local old_file="$2"
  local new_file="$3"
  local diff_dir="$4"
  local old_outdir="${5:-}"
  local new_outdir="${6:-$old_outdir}"

  if [ ! -f "$old_file" ] && [ ! -f "$new_file" ]; then
    echo "OK: $label not generated by either CLI"
    return 0
  fi

  compare_file_normalized "$label" "$old_file" "$new_file" "$diff_dir" "$old_outdir" "$new_outdir"
}

compare_optional_exact_file() {
  local label="$1"
  local old_file="$2"
  local new_file="$3"
  local old_outdir="${4:-}"
  local new_outdir="${5:-$old_outdir}"
  local tmp

  if [ ! -f "$old_file" ] && [ ! -f "$new_file" ]; then
    echo "OK: $label not generated by either CLI"
    return 0
  fi

  [ -f "$old_file" ] || {
    echo "FAIL: $label missing in cargo baseline: $old_file"
    return 1
  }

  [ -f "$new_file" ] || {
    echo "FAIL: $label missing in candidate implementation: $new_file"
    return 1
  }

  tmp="$(mktemp -d)"
  normalize_allowed_outdir_prefix "$old_outdir" "$new_outdir" < "$old_file" > "$tmp/old"
  cat "$new_file" > "$tmp/new"

  if diff -u "$tmp/old" "$tmp/new"; then
    echo "OK: $label matched"
    rm -rf "$tmp"
  else
    echo "FAIL: $label differs"
    rm -rf "$tmp"
    return 1
  fi
}

compare_sqlite_if_present() {
  local old_cwd="$1"
  local new_cwd="$2"
  local old_outdir="$3"
  local new_outdir="$4"

  local old_sqlite_dir
  local new_sqlite_dir

  if ! old_sqlite_dir="$(resolve_sqlite_dir "$old_cwd" "$old_outdir")"; then
    echo "SKIP: sqlite not generated in cargo baseline workdir"
    return 0
  fi

  if ! new_sqlite_dir="$(resolve_sqlite_dir "$new_cwd" "$new_outdir")"; then
    echo "FAIL: sqlite generated by cargo baseline but not by candidate implementation"
    return 1
  fi

  compare_sqlite_compatible "$old_sqlite_dir/schemas.sqlite" "$new_sqlite_dir/schemas.sqlite" "$old_outdir" "$new_outdir"
}

dump_legacy_sqlite_data() {
  local db="$1"

  sqlite3 -readonly "$db" <<'SQL'
.headers off
.mode tabs
SELECT 'schema_paths', schema_path, object_path
FROM schema_paths
ORDER BY schema_path, object_path;
SELECT 'array_index_refs', array_path, array_index_path, schema_path
FROM array_index_refs
ORDER BY array_path, array_index_path, schema_path;
SELECT 'schema_definitions', schema_path, schema_kind, schema_json
FROM schema_definitions
ORDER BY schema_path;
SELECT 'schema_object_counts', schema_path, object_count
FROM schema_object_counts
ORDER BY schema_path;
SELECT 'schema_field_counts', schema_path, field_name, field_type, field_count
FROM schema_field_counts
ORDER BY schema_path, field_name, field_type;
SELECT 'schema_relations',
       from_schema_path, to_schema_path, relation_kind, fk_owner, fk_candidate,
       field_name, field_type, cardinality, required, mixed, nested_array_depth,
       COALESCE(via_schema_path, ''), COALESCE(via_array_path, ''),
       parent_schema_path, child_schema_path, parent_object_count,
       child_object_count, field_count
FROM schema_relations
ORDER BY from_schema_path, to_schema_path, relation_kind, field_name, field_type,
         COALESCE(via_schema_path, ''), COALESCE(via_array_path, '');
SQL
}

compare_sqlite_compatible() {
  local old_db="$1"
  local new_db="$2"
  local old_outdir="${3:-}"
  local new_outdir="${4:-$old_outdir}"
  local tmp

  sqlite3 "$old_db" 'PRAGMA integrity_check;' | grep -qx 'ok' || die "integrity_check failed: $old_db"
  sqlite3 "$new_db" 'PRAGMA integrity_check;' | grep -qx 'ok' || die "integrity_check failed: $new_db"

  tmp="$(mktemp -d)"
  dump_legacy_sqlite_data "$old_db" \
    | normalize_allowed_outdir_prefix "$old_outdir" "$new_outdir" \
    > "$tmp/old.tsv"
  dump_legacy_sqlite_data "$new_db" > "$tmp/new.tsv"

  if diff -u "$tmp/old.tsv" "$tmp/new.tsv"; then
    echo "OK: SQLite legacy data matched"
    rm -rf "$tmp"
  else
    echo "FAIL: SQLite legacy data differs"
    rm -rf "$tmp"
    return 1
  fi
}

compare_schema_json_files() {
  local label="$1"
  local old_dir="$2"
  local new_dir="$3"
  local old_outdir="${4:-}"
  local new_outdir="${5:-$old_outdir}"
  local tmp
  local failed=0

  if [ ! -d "$old_dir" ] && [ ! -d "$new_dir" ]; then
    echo "OK: $label not generated by either CLI"
    return 0
  fi

  if [ ! -d "$old_dir" ]; then
    echo "FAIL: $label cargo baseline output directory not found: $old_dir"
    return 1
  fi

  if [ ! -d "$new_dir" ]; then
    echo "FAIL: $label candidate output directory not found: $new_dir"
    return 1
  fi

  tmp="$(mktemp -d)"
  (
    cd "$old_dir"
    find . -type f -name '*.json' -printf '%P\n' | sort
  ) > "$tmp/old-files"
  (
    cd "$new_dir"
    find . -type f -name '*.json' -printf '%P\n' | sort
  ) > "$tmp/new-files"

  if ! diff -u "$tmp/old-files" "$tmp/new-files"; then
    echo "FAIL: $label schema JSON file list differs"
    failed=1
  fi

  while IFS= read -r file; do
    [ -n "$file" ] || continue
    if [ ! -f "$new_dir/$file" ]; then
      continue
    fi
    normalize_allowed_outdir_prefix "$old_outdir" "$new_outdir" < "$old_dir/$file" > "$tmp/old-json"
    cat "$new_dir/$file" > "$tmp/new-json"
    if ! diff -u "$tmp/old-json" "$tmp/new-json"; then
      echo "FAIL: $label schema JSON differs: $file"
      failed=1
    fi
  done < "$tmp/old-files"

  rm -rf "$tmp"
  if [ "$failed" -eq 0 ]; then
    echo "OK: $label schema JSON files matched"
  fi
  return "$failed"
}

compare_known_outputs() {
  local case_dir="$1"
  local old_cwd="$2"
  local new_cwd="$3"
  local old_report="$4"
  local new_report="$5"
  local old_outdir="$6"
  local new_outdir="$7"

  local failed=0

  compare_file_normalized \
    "stdout" \
    "$old_cwd/stdout.txt" \
    "$new_cwd/stdout.txt" \
    "$case_dir" \
    "$old_outdir" \
    "$new_outdir" || failed=1

  compare_file_normalized \
    "stderr" \
    "$old_cwd/stderr.txt" \
    "$new_cwd/stderr.txt" \
    "$case_dir" \
    "$old_outdir" \
    "$new_outdir" || failed=1

  compare_optional_normalized_file \
    "report" \
    "$old_cwd/$old_report" \
    "$new_cwd/$new_report" \
    "$case_dir" \
    "$old_outdir" \
    "$new_outdir" || failed=1

  compare_schema_json_files \
    "schema-json" \
    "$old_cwd/$old_outdir" \
    "$new_cwd/$new_outdir" \
    "$old_outdir" \
    "$new_outdir" || failed=1

  for graph in graph.mmd graph.md graph.dot; do
    compare_optional_exact_file \
      "$graph" \
      "$old_cwd/$graph" \
      "$new_cwd/$graph" \
      "$old_outdir" \
      "$new_outdir" || failed=1
  done

  return "$failed"
}

run_case() {
  local name="$1"
  local mode="$2"
  shift 2

  local safe_name
  safe_name="$(quote_case_name "$name")"

  local case_dir="$TMPDIR/$safe_name"
  local old_cwd="$case_dir/cargo"
  local new_cwd="$case_dir/candidate"
  local old_outdir
  local new_outdir
  local old_report
  local new_report

  mkdir -p "$old_cwd" "$new_cwd"
  old_outdir="$(outdir_for_args refs "$@")"
  new_outdir="$(outdir_for_args refs "$@")"
  old_report="$(report_path_for_args "$old_outdir/report.txt" "$@")"
  new_report="$(report_path_for_args "$new_outdir/report.txt" "$@")"

  echo
  echo "============================================================"
  echo "CASE: $name"
  echo "MODE: $mode"
  echo "ARGS: $*"
  echo "============================================================"

  case "$mode" in
    file)
      run_baseline_file "$old_cwd" "$FIXTURE_JSON" "$@" > "$old_cwd/stdout.txt" 2> "$old_cwd/stderr.txt"
      run_candidate_file "$new_cwd" "$FIXTURE_JSON" "$@" > "$new_cwd/stdout.txt" 2> "$new_cwd/stderr.txt"
      compare_known_outputs "$case_dir" "$old_cwd" "$new_cwd" "$old_report" "$new_report" "$old_outdir" "$new_outdir"
      compare_sqlite_if_present "$old_cwd" "$new_cwd" "$old_outdir" "$new_outdir"
      ;;

    stdin)
      run_baseline_stdin "$old_cwd" "$FIXTURE_JSON" "$@" > "$old_cwd/stdout.txt" 2> "$old_cwd/stderr.txt"
      run_candidate_stdin "$new_cwd" "$FIXTURE_JSON" "$@" > "$new_cwd/stdout.txt" 2> "$new_cwd/stderr.txt"
      compare_known_outputs "$case_dir" "$old_cwd" "$new_cwd" "$old_report" "$new_report" "$old_outdir" "$new_outdir"
      compare_sqlite_if_present "$old_cwd" "$new_cwd" "$old_outdir" "$new_outdir"
      ;;

    from-sqlite)
      mkdir -p "$old_cwd/refs" "$new_cwd/refs"

      # Prepare equivalent sqlite inputs for each implementation.
      (
        cd "$old_cwd"
        "$CARGO_DUMP_JSON_REFS_BIN" "$FIXTURE_JSON" --outdir refs > /dev/null
      )
      (
        cd "$new_cwd"
        "$DUMP_JSON_REFS_BIN" "$FIXTURE_JSON" --outdir refs > /dev/null
      )

      compare_sqlite_compatible "$old_cwd/refs/schemas.sqlite" "$new_cwd/refs/schemas.sqlite"

      old_report="$(report_path_for_args "$(dirname "$(sqlite_path_from_args refs/schemas.sqlite "$@")")/report.txt" "$@")"
      new_report="$(report_path_for_args "$(dirname "$(sqlite_path_from_args refs/schemas.sqlite "$@")")/report.txt" "$@")"
      local old_sqlite_outdir
      local new_sqlite_outdir
      old_sqlite_outdir="$(dirname "$(sqlite_path_from_args refs/schemas.sqlite "$@")")"
      new_sqlite_outdir="$(dirname "$(sqlite_path_from_args refs/schemas.sqlite "$@")")"

      run_baseline_from_sqlite "$old_cwd" "$@" > "$old_cwd/stdout.txt" 2> "$old_cwd/stderr.txt"
      run_candidate_from_sqlite "$new_cwd" "$@" > "$new_cwd/stdout.txt" 2> "$new_cwd/stderr.txt"

      compare_file_normalized "stdout" "$old_cwd/stdout.txt" "$new_cwd/stdout.txt" "$case_dir" "$old_sqlite_outdir" "$new_sqlite_outdir"
      compare_file_normalized "stderr" "$old_cwd/stderr.txt" "$new_cwd/stderr.txt" "$case_dir" "$old_sqlite_outdir" "$new_sqlite_outdir"
      compare_optional_normalized_file "report" "$old_cwd/$old_report" "$new_cwd/$new_report" "$case_dir" "$old_sqlite_outdir" "$new_sqlite_outdir"
      ;;

    *)
      die "unknown mode: $mode"
      ;;
  esac
}

main() {
  if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    show_help
    exit 0
  fi

  CARGO_DUMP_JSON_REFS_BIN="${CARGO_DUMP_JSON_REFS_BIN:-/home/widehyo/.cargo/bin/dump-json-refs}"
  DUMP_JSON_REFS_BIN="${DUMP_JSON_REFS_BIN:-target/debug/dump-json-refs}"

  CARGO_DUMP_JSON_REFS_BIN="$(resolve_cmd_path "$CARGO_DUMP_JSON_REFS_BIN")"
  DUMP_JSON_REFS_BIN="$(resolve_cmd_path "$DUMP_JSON_REFS_BIN")"

  need_cmd sqlite3
  need_cmd diff
  need_cmd awk
  need_cmd sed
  need_cmd grep
  need_cmd mktemp
  need_cmd cat
  need_cmd "$CARGO_DUMP_JSON_REFS_BIN"
  need_cmd "$DUMP_JSON_REFS_BIN"

  TMPDIR="$(mktemp -d)"
  export TMPDIR

  if [ "${KEEP_TMP:-0}" = "1" ]; then
    echo "Temporary directory: $TMPDIR"
  else
    trap 'rm -rf "$TMPDIR"' EXIT
  fi

  if [ "$#" -ge 1 ]; then
    FIXTURE_JSON="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
    [ -f "$FIXTURE_JSON" ] || die "fixture not found: $FIXTURE_JSON"
  else
    FIXTURE_JSON="$TMPDIR/temp.json"

    # Single-line JSON fixture.
    # This is valid as normal JSON and also acceptable as one-line JSONL.
    cat > "$FIXTURE_JSON" <<'JSON'
{"users":[{"id":1,"name":"alice","active":true,"profile":{"age":30,"tags":["admin","ops"]},"orders":[{"id":"o1","amount":10.5},{"id":"o2","amount":20}]},{"id":2,"name":"bob","active":false,"profile":{"age":25,"tags":["user"]},"orders":[]}],"meta":{"source":"regression","version":1}}
JSON
  fi

  FIXTURE_JSONL="$TMPDIR/temp.jsonl"
  cat > "$FIXTURE_JSONL" <<'JSONL'
{"id":1,"name":"alice"}
{"id":2,"name":"bob"}
JSONL

  export FIXTURE_JSON
  export FIXTURE_JSONL

  echo "Fixture: $FIXTURE_JSON"
  echo "JSONL fixture: $FIXTURE_JSONL"
  echo "Cargo baseline CLI: $CARGO_DUMP_JSON_REFS_BIN"
  echo "Candidate CLI: $DUMP_JSON_REFS_BIN"

  # ---------------------------------------------------------------------------
  # file input
  # ---------------------------------------------------------------------------
  run_case "file_default" file
  run_case "file_outdir" file --outdir refs-temp
  run_case "file_compact_output" file --compact-output
  run_case "file_report_txt" file -o report.txt
  run_case "file_jsonl" file --jsonl
  run_case "file_graph_no_path" file --graph
  run_case "file_graph_mmd" file --graph graph.mmd
  run_case "file_graph_md_mermaid_md" file --graph graph.md --graph-format mermaid-md
  run_case "file_graph_dot" file --graph graph.dot --graph-format dot
  run_case "file_graph_rankdir_tb" file --graph graph.mmd --graph-rankdir TB
  run_case "file_graph_include_marked" file --graph graph.mmd --graph-include-marked
  run_case "file_compact_outdir_report_graph" file \
    --compact-output \
    --outdir refs-temp \
    -o report.json \
    --graph graph.mmd

  local base_fixture
  base_fixture="$FIXTURE_JSON"
  FIXTURE_JSON="$FIXTURE_JSONL"
  run_case "file_jsonl_extension_default" file
  FIXTURE_JSON="$base_fixture"

  # ---------------------------------------------------------------------------
  # stdin input
  # ---------------------------------------------------------------------------
  run_case "stdin_default" stdin
  run_case "stdin_outdir" stdin --outdir refs-temp
  run_case "stdin_compact_output" stdin --compact-output
  run_case "stdin_report_txt" stdin -o report.txt
  run_case "stdin_jsonl" stdin --jsonl
  run_case "stdin_graph_no_path" stdin --graph
  run_case "stdin_graph_mmd" stdin --graph graph.mmd
  run_case "stdin_graph_md_mermaid_md" stdin --graph graph.md --graph-format mermaid-md
  run_case "stdin_graph_dot" stdin --graph graph.dot --graph-format dot
  run_case "stdin_graph_rankdir_tb" stdin --graph graph.mmd --graph-rankdir TB
  run_case "stdin_graph_include_marked" stdin --graph graph.mmd --graph-include-marked
  run_case "stdin_compact_outdir_report_graph" stdin \
    --compact-output \
    --outdir refs-temp \
    -o report.json \
    --graph graph.mmd

  # ---------------------------------------------------------------------------
  # from sqlite
  # ---------------------------------------------------------------------------
  run_case "sqlite_default_path" from-sqlite --from-sqlite
  run_case "sqlite_explicit_path" from-sqlite --from-sqlite refs/schemas.sqlite
  run_case "sqlite_report_txt" from-sqlite --from-sqlite refs/schemas.sqlite -o report.txt
  run_case "sqlite_graph_mmd" from-sqlite --from-sqlite refs/schemas.sqlite --graph graph.mmd
  run_case "sqlite_graph_dot" from-sqlite --from-sqlite refs/schemas.sqlite --graph graph.dot --graph-format dot
  run_case "sqlite_graph_rankdir_tb" from-sqlite --from-sqlite refs/schemas.sqlite --graph graph.mmd --graph-rankdir TB
  run_case "sqlite_graph_include_marked" from-sqlite --from-sqlite refs/schemas.sqlite --graph graph.mmd --graph-include-marked

  echo
  echo "PASS: dump-json-refs regression compatibility verified."
}

main "$@"
