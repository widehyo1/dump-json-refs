use anyhow::{anyhow, bail, Context, Result};
use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};
use lasso::{Rodeo, Spur};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const DEFAULT_GRAPH_OUTPUT: &str = "__default_graph_path__";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum GraphFormat {
    Mermaid,
    MermaidMd,
    Dot,
}

impl GraphFormat {
    fn default_filename(self) -> &'static str {
        match self {
            GraphFormat::Mermaid => "schema.mmd",
            GraphFormat::MermaidMd => "schema.md",
            GraphFormat::Dot => "schema.dot",
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "dump-json-refs")]
#[command(about = "Generate JSON schema refs and a SQLite path index from JSON/JSONL input")]
struct Args {
    /// Force JSONL mode. Without this flag, *.jsonl input files are also treated as JSONL.
    #[arg(long)]
    jsonl: bool,

    /// Output compact one-line JSON files under the refs directory.
    #[arg(short = 'c', long = "compact-output")]
    compact_output: bool,

    /// Write the full report to this file. Stdout prints only the summary.
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    report_output: Option<PathBuf>,

    /// Read report data from an existing SQLite index instead of generating refs.
    /// When the flag is used without a value, refs/schemas.sqlite is used.
    #[arg(
        long = "from-sqlite",
        value_name = "FILE",
        num_args = 0..=1,
        default_missing_value = "refs/schemas.sqlite"
    )]
    from_sqlite: Option<PathBuf>,

    /// Generate a graph projection from SQLite schema relations.
    ///
    /// When FILE is omitted, the default path depends on --graph-format:
    /// mermaid -> `<outdir>/schema.mmd`, mermaid-md -> `<outdir>/schema.md`,
    /// dot -> `<outdir>/schema.dot`. In --from-sqlite mode, `<outdir>` is
    /// replaced by the SQLite file's parent directory. Existing refs are not
    /// removed in --from-sqlite mode.
    #[arg(
        long = "graph",
        value_name = "FILE",
        num_args = 0..=1,
        default_missing_value = "__default_graph_path__"
    )]
    graph_output: Option<PathBuf>,

    /// Graph output format. Also generates the graph when --graph is omitted.
    ///
    /// Mermaid is the default for quick GitHub/Markdown feedback.
    #[arg(
        long = "graph-format",
        default_value_t = GraphFormat::Mermaid,
        value_enum,
        value_name = "mermaid|mermaid-md|dot",
        hide_possible_values = true
    )]
    graph_format: GraphFormat,

    /// Include structural marker relations such as nested array items.
    ///
    /// Also generates the graph when --graph is omitted.
    #[arg(long = "graph-include-marked")]
    graph_include_marked: bool,

    /// Graph layout direction used by Mermaid and DOT output.
    ///
    /// Also generates the graph when --graph is omitted.
    #[arg(
        long = "graph-rankdir",
        default_value = "LR",
        value_name = "LR|TB|RL|BT",
        value_parser = ["LR", "TB", "RL", "BT"],
        hide_possible_values = true
    )]
    graph_rankdir: String,

    #[arg(skip)]
    graph_requested: bool,

    /// Output directory. It is removed and recreated.
    #[arg(long, default_value = "refs")]
    outdir: PathBuf,

    /// Input JSON/JSONL file. Reads stdin when omitted.
    input_file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct Entry {
    path: Vec<String>,
    index: Option<usize>,
    collection: bool,
    value: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Role {
    Object,
    ArrayItem,
    RootCollection,
}

#[derive(Debug, Clone)]
struct Occurrence {
    segments: Vec<String>,
    value: Map<String, Value>,
    role: Role,
    array_indexes: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct Record {
    schema_path: String,
    object_paths: Vec<String>,
    schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    array_parent: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    array_index_paths: Vec<Vec<usize>>,
}

#[derive(Debug)]
struct SchemaBuild {
    records: Vec<Record>,
    occurrences: Vec<Occurrence>,
}

fn main() -> Result<()> {
    let matches = Args::command().get_matches();
    let graph_requested = graph_arg_was_provided(&matches);
    let mut args = Args::from_arg_matches(&matches)?;
    args.graph_requested = graph_requested;
    run(args)
}

fn graph_arg_was_provided(matches: &clap::ArgMatches) -> bool {
    [
        "graph_output",
        "graph_format",
        "graph_include_marked",
        "graph_rankdir",
    ]
    .iter()
    .any(|id| {
        matches
            .value_source(id)
            .is_some_and(|source| source == ValueSource::CommandLine)
    })
}

fn perf_trace_enabled() -> bool {
    std::env::var_os("DUMP_JSON_REFS_TRACE").is_some()
}

fn perf_trace(stage: &str, started: Instant) {
    if perf_trace_enabled() {
        eprintln!(
            "[perf]	stage={stage}	elapsed_ms={}",
            started.elapsed().as_millis()
        );
    }
}

fn perf_trace_detail(stage: &str, started: Instant, detail: impl std::fmt::Display) {
    if perf_trace_enabled() {
        eprintln!(
            "[perf]	stage={stage}	elapsed_ms={}	{detail}",
            started.elapsed().as_millis()
        );
    }
}

fn sqlite_table_count(tx: &Transaction<'_>, table_name: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table_name}");
    tx.query_row(&sql, [], |row| row.get(0))
        .with_context(|| format!("failed to count {table_name}"))
}

fn perf_trace_sql_enabled() -> bool {
    std::env::var_os("DUMP_JSON_REFS_TRACE_SQL").is_some()
}

fn perf_trace_query_plan(tx: &Transaction<'_>, stage: &str, sql: &str) -> Result<()> {
    if !perf_trace_sql_enabled() {
        return Ok(());
    }

    let started = Instant::now();
    let explain_sql = format!("EXPLAIN QUERY PLAN {sql}");
    let mut stmt = tx
        .prepare(&explain_sql)
        .with_context(|| format!("failed to explain query plan for {stage}"))?;
    let mut rows = stmt.query([])?;
    let mut row_count = 0usize;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let parent: i64 = row.get(1)?;
        let detail: String = row.get(3)?;
        eprintln!("[perf-sql]	stage={stage}	id={id}	parent={parent}	detail={detail}");
        row_count += 1;
    }
    perf_trace_detail(
        "sql.explain_query_plan",
        started,
        format_args!("query_stage={stage}	rows={row_count}"),
    );
    Ok(())
}

fn run(args: Args) -> Result<()> {
    if let Some(sqlite_path) = args.from_sqlite.as_deref() {
        if args.input_file.is_some() {
            bail!("input file cannot be used with --from-sqlite");
        }

        emit_report_from_sqlite(
            sqlite_path,
            &ReportSummary::FromSqlite {
                sqlite_path: sqlite_path.display().to_string(),
            },
            args.report_output.as_deref(),
        )?;

        if let Some(graph_output) = resolve_graph_output_path(
            args.graph_requested,
            args.graph_output.as_deref(),
            &args.outdir,
            Some(sqlite_path),
            args.graph_format,
        ) {
            write_graph_from_sqlite(
                sqlite_path,
                &graph_output,
                args.graph_include_marked,
                &args.graph_rankdir,
                args.graph_format,
            )?;
        }
        return Ok(());
    }

    let started = Instant::now();
    reject_unsafe_outdir(&args.outdir)?;

    let input_source = args
        .input_file
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "stdin".to_string());

    if let Some(path) = args.input_file.as_ref() {
        let (base, source_is_jsonl) = input_file_base_and_jsonl(path)?;
        if args.jsonl || source_is_jsonl {
            stream_jsonl_file_run(args, input_source, base, started)?;
            return Ok(());
        }
    } else if args.jsonl {
        stream_jsonl_stdin_run(args, input_source, started)?;
        return Ok(());
    }

    let (input, file_mode, base, source_is_jsonl) = read_input(&args)?;
    let jsonl_mode = args.jsonl || source_is_jsonl;
    let entries = normalize_input(&input, jsonl_mode, file_mode, &base)?;
    let build = build_schema_output(&args.outdir, entries)?;

    if args.outdir.exists() {
        fs::remove_dir_all(&args.outdir)
            .with_context(|| format!("failed to remove {}", args.outdir.display()))?;
    }
    fs::create_dir_all(&args.outdir)
        .with_context(|| format!("failed to create {}", args.outdir.display()))?;

    write_files_and_index(
        &args.outdir,
        &build.records,
        &build.occurrences,
        args.compact_output,
    )?;

    let database = args.outdir.join("schemas.sqlite");
    emit_report_from_sqlite(
        &database,
        &ReportSummary::Generated {
            execution_time_ms: started.elapsed().as_millis(),
            input_source,
        },
        args.report_output.as_deref(),
    )?;

    if let Some(graph_output) = resolve_graph_output_path(
        args.graph_requested,
        args.graph_output.as_deref(),
        &args.outdir,
        None,
        args.graph_format,
    ) {
        write_graph_from_sqlite(
            &database,
            &graph_output,
            args.graph_include_marked,
            &args.graph_rankdir,
            args.graph_format,
        )?;
    }

    Ok(())
}

fn input_file_base_and_jsonl(path: &Path) -> Result<(String, bool)> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("invalid input path: {}", path.display()))?
        .to_string_lossy();
    let source_is_jsonl = file_name.ends_with(".jsonl");
    let base = file_name
        .strip_suffix(".jsonl")
        .or_else(|| file_name.strip_suffix(".json"))
        .unwrap_or(&file_name)
        .to_string();
    Ok((base, source_is_jsonl))
}

fn stream_jsonl_file_run(
    args: Args,
    input_source: String,
    base: String,
    started: Instant,
) -> Result<()> {
    let input_path = args
        .input_file
        .as_ref()
        .ok_or_else(|| anyhow!("missing JSONL input file"))?;
    let file = fs::File::open(input_path)
        .with_context(|| format!("cannot read input file: {}", input_path.display()))?;

    prepare_outdir(&args.outdir)?;
    stream_jsonl_reader_to_outdir(
        BufReader::new(file),
        &args.outdir,
        true,
        &base,
        args.compact_output,
    )?;
    finish_generated_run(args, input_source, started)
}

fn stream_jsonl_stdin_run(args: Args, input_source: String, started: Instant) -> Result<()> {
    let stdin = io::stdin();
    prepare_outdir(&args.outdir)?;
    stream_jsonl_reader_to_outdir(
        stdin.lock(),
        &args.outdir,
        false,
        "root",
        args.compact_output,
    )?;
    finish_generated_run(args, input_source, started)
}

fn finish_generated_run(args: Args, input_source: String, started: Instant) -> Result<()> {
    let database = args.outdir.join("schemas.sqlite");
    emit_report_from_sqlite(
        &database,
        &ReportSummary::Generated {
            execution_time_ms: started.elapsed().as_millis(),
            input_source,
        },
        args.report_output.as_deref(),
    )?;

    if let Some(graph_output) = resolve_graph_output_path(
        args.graph_requested,
        args.graph_output.as_deref(),
        &args.outdir,
        None,
        args.graph_format,
    ) {
        write_graph_from_sqlite(
            &database,
            &graph_output,
            args.graph_include_marked,
            &args.graph_rankdir,
            args.graph_format,
        )?;
    }

    Ok(())
}

fn prepare_outdir(outdir: &Path) -> Result<()> {
    if outdir.exists() {
        fs::remove_dir_all(outdir)
            .with_context(|| format!("failed to remove {}", outdir.display()))?;
    }
    fs::create_dir_all(outdir).with_context(|| format!("failed to create {}", outdir.display()))
}

fn reject_unsafe_outdir(outdir: &Path) -> Result<()> {
    let s = outdir.as_os_str().to_string_lossy();
    if s.is_empty() || s == "/" || s == "." || s == ".." {
        bail!("refusing unsafe output directory: {}", outdir.display());
    }
    Ok(())
}

fn read_input(args: &Args) -> Result<(String, bool, String, bool)> {
    if let Some(path) = &args.input_file {
        let input = fs::read_to_string(path)
            .with_context(|| format!("cannot read input file: {}", path.display()))?;
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow!("invalid input path: {}", path.display()))?
            .to_string_lossy();
        let source_is_jsonl = file_name.ends_with(".jsonl");
        let base = file_name
            .strip_suffix(".jsonl")
            .or_else(|| file_name.strip_suffix(".json"))
            .unwrap_or(&file_name)
            .to_string();
        Ok((input, true, base, source_is_jsonl))
    } else {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .context("failed to read stdin")?;
        Ok((input, false, "root".to_string(), false))
    }
}

fn normalize_input(
    input: &str,
    jsonl_mode: bool,
    file_mode: bool,
    base: &str,
) -> Result<Vec<Entry>> {
    if input.trim().is_empty() {
        bail!("input is empty");
    }

    if jsonl_mode {
        let values = parse_jsonl_stream(input)?;
        return normalize_jsonl_values(values, file_mode, base);
    }

    let values = match parse_json_stream(input) {
        Ok(values) => values,
        Err(json_error) => {
            return match parse_jsonl_stream(input) {
                Ok(values) => normalize_jsonl_values(values, file_mode, base),
                Err(jsonl_error) => Err(json_error).with_context(|| {
                    format!("invalid JSON input; JSONL fallback also failed: {jsonl_error}")
                }),
            };
        }
    };

    normalize_json_values(values, file_mode, base)
}

fn normalize_jsonl_values(values: Vec<Value>, file_mode: bool, base: &str) -> Result<Vec<Entry>> {
    let root = if file_mode {
        format!("{base}_ref")
    } else {
        "root_item".to_string()
    };

    if values.is_empty() {
        bail!("input is empty");
    }

    values
        .into_iter()
        .enumerate()
        .map(|(i, value)| match value {
            Value::Object(map) => Ok(Entry {
                path: vec![root.clone()],
                index: Some(i),
                collection: true,
                value: map,
            }),
            _ => bail!("JSONL values must be objects"),
        })
        .collect()
}

fn normalize_json_values(values: Vec<Value>, file_mode: bool, base: &str) -> Result<Vec<Entry>> {
    if values.is_empty() {
        bail!("input is empty");
    }

    let root_object = if file_mode {
        base.to_string()
    } else {
        "root".to_string()
    };
    let root_collection = if file_mode {
        format!("{base}_ref")
    } else {
        "root_item".to_string()
    };

    if values.len() == 1 {
        match values.into_iter().next().unwrap() {
            Value::Object(map) => Ok(vec![Entry {
                path: vec![root_object],
                index: None,
                collection: false,
                value: map,
            }]),
            Value::Array(items) => items
                .into_iter()
                .enumerate()
                .map(|(i, value)| match value {
                    Value::Object(map) => Ok(Entry {
                        path: vec![root_collection.clone()],
                        index: Some(i),
                        collection: true,
                        value: map,
                    }),
                    _ => bail!("top-level array values must be objects"),
                })
                .collect(),
            _ => bail!("top-level JSON value must be an object or array"),
        }
    } else {
        values
            .into_iter()
            .enumerate()
            .map(|(i, value)| match value {
                Value::Object(map) => Ok(Entry {
                    path: vec![root_collection.clone()],
                    index: Some(i),
                    collection: true,
                    value: map,
                }),
                _ => bail!("top-level JSON values must be objects"),
            })
            .collect()
    }
}

fn parse_json_stream(input: &str) -> Result<Vec<Value>> {
    let de = serde_json::Deserializer::from_str(input);
    de.into_iter::<Value>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("invalid JSON input")
}

fn parse_jsonl_stream(input: &str) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    for (index, line) in input.lines().enumerate() {
        let line_number = index + 1;
        let record = line.trim_start_matches('\0');
        if record.trim().is_empty() {
            if line.trim().is_empty() {
                continue;
            }
            bail!("invalid JSONL input at line {line_number}: record contains only NUL padding");
        }
        let parsed = serde_json::Deserializer::from_str(record)
            .into_iter::<Value>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("invalid JSONL input at line {line_number}"))?;
        values.extend(parsed);
    }
    Ok(values)
}

fn stream_jsonl_reader_to_outdir<R: BufRead>(
    reader: R,
    outdir: &Path,
    file_mode: bool,
    base: &str,
    compact_output: bool,
) -> Result<()> {
    let stage_started = Instant::now();
    let database = outdir.join("schemas.sqlite");
    let mut conn = Connection::open(&database)
        .with_context(|| format!("failed to open {}", database.display()))?;
    perf_trace("stream.open_sqlite", stage_started);

    let stage_started = Instant::now();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = FILE;",
    )?;
    perf_trace("stream.sqlite_pragmas", stage_started);

    let stage_started = Instant::now();
    create_stream_tables(&conn)?;
    perf_trace("stream.create_temp_tables", stage_started);

    let stage_started = Instant::now();
    stream_jsonl_into_temp_tables(&mut conn, reader, outdir, file_mode, base)?;
    perf_trace("stream.accumulate_and_flush_temp", stage_started);

    let stage_started = Instant::now();
    let result = finalize_streamed_jsonl(&mut conn, outdir, compact_output);
    perf_trace("stream.finalize", stage_started);
    result
}

fn create_stream_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TEMP TABLE stream_object_totals (
             path_base TEXT PRIMARY KEY,
             object_count INTEGER NOT NULL
         );
         CREATE TEMP TABLE stream_object_field_facts (
             path_base TEXT NOT NULL,
             field_name TEXT NOT NULL,
             present_count INTEGER NOT NULL,
             array_count INTEGER NOT NULL,
             boolean_count INTEGER NOT NULL,
             null_count INTEGER NOT NULL,
             number_count INTEGER NOT NULL,
             object_count INTEGER NOT NULL,
             string_count INTEGER NOT NULL,
             PRIMARY KEY (path_base, field_name)
         );
         CREATE TEMP TABLE stream_array_member_facts (
             path_base TEXT NOT NULL,
             depth INTEGER NOT NULL,
             member_count INTEGER NOT NULL,
             array_count INTEGER NOT NULL,
             boolean_count INTEGER NOT NULL,
             null_count INTEGER NOT NULL,
             number_count INTEGER NOT NULL,
             object_count INTEGER NOT NULL,
             string_count INTEGER NOT NULL,
             PRIMARY KEY (path_base, depth)
         );
         CREATE TEMP TABLE stream_item_schema_counts (
             role TEXT NOT NULL,
             path_base TEXT NOT NULL,
             canonical_json TEXT NOT NULL,
             schema_json TEXT NOT NULL,
             object_count INTEGER NOT NULL,
             PRIMARY KEY (role, path_base, canonical_json)
         );
         CREATE TEMP TABLE stream_item_schema_index_refs (
             role TEXT NOT NULL,
             path_base TEXT NOT NULL,
             canonical_json TEXT NOT NULL,
             array_index_path TEXT NOT NULL,
             index_path_key TEXT NOT NULL,
             PRIMARY KEY (role, path_base, array_index_path)
         );
         CREATE TEMP TABLE stream_item_schema_field_counts (
             role TEXT NOT NULL,
             path_base TEXT NOT NULL,
             canonical_json TEXT NOT NULL,
             field_name TEXT NOT NULL,
             field_count INTEGER NOT NULL,
             PRIMARY KEY (role, path_base, canonical_json, field_name)
         );
         CREATE TEMP TABLE stream_object_schema_candidates (
             canonical_json TEXT NOT NULL,
             schema_json TEXT NOT NULL,
             object_path TEXT NOT NULL PRIMARY KEY
         );
         CREATE TEMP TABLE stream_schema_field_types (
             schema_path TEXT NOT NULL,
             field_name TEXT NOT NULL,
             field_type TEXT NOT NULL,
             PRIMARY KEY (schema_path, field_name)
         );
         CREATE TEMP TABLE stream_item_schema_paths (
             role TEXT NOT NULL,
             path_base TEXT NOT NULL,
             canonical_json TEXT NOT NULL,
             schema_path TEXT NOT NULL,
             PRIMARY KEY (role, path_base, canonical_json)
         );
         CREATE INDEX stream_item_schema_counts_path ON stream_item_schema_counts(role, path_base);
         CREATE INDEX stream_item_schema_index_refs_schema ON stream_item_schema_index_refs(role, path_base, canonical_json);
         CREATE INDEX stream_item_schema_paths_schema ON stream_item_schema_paths(role, path_base, canonical_json);",
    )?;
    Ok(())
}

fn stream_jsonl_into_temp_tables<R: BufRead>(
    conn: &mut Connection,
    mut reader: R,
    outdir: &Path,
    file_mode: bool,
    base: &str,
) -> Result<()> {
    let root = if file_mode {
        format!("{base}_ref")
    } else {
        "root_item".to_string()
    };
    let root_base = reference_path(outdir, &[root]);

    let mut line = String::new();
    let mut line_number = 0usize;
    let mut value_index = 0usize;
    let mut input_bytes = 0usize;
    let mut saw_value = false;
    let mut acc = StreamAcc::default();
    let accumulate_started = Instant::now();

    'records: loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .context("failed to read JSONL input")?;
        if bytes_read == 0 {
            break;
        }
        input_bytes += bytes_read;
        line_number += 1;

        let record = line.trim_start_matches('\0');
        if record.trim().is_empty() {
            if line.trim().is_empty() {
                continue;
            }
            bail!("invalid JSONL input at line {line_number}: record contains only NUL padding");
        }

        let de = serde_json::Deserializer::from_str(record);
        for value in de.into_iter::<Value>() {
            let value = match value {
                Ok(value) => value,
                Err(error) if is_incomplete_trailing_jsonl_record(&line, &error) => {
                    eprintln!(
                        "warning: skipped incomplete trailing JSONL record at line {line_number}"
                    );
                    break 'records;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("invalid JSONL input at line {line_number}"));
                }
            };
            let Value::Object(map) = value else {
                bail!("JSONL values must be objects");
            };

            let array_indexes = vec![value_index];
            value_index += 1;
            saw_value = true;
            record_streamed_object_occurrence_to_acc(
                &mut acc,
                outdir,
                &root_base,
                &map,
                Role::RootCollection,
                &array_indexes,
            )?;
        }
    }

    let acc_stats = acc.stats();
    perf_trace_detail(
        "stream.read_parse_walk",
        accumulate_started,
        format_args!(
            "lines={}	values={}	input_bytes={}	{}",
            line_number, value_index, input_bytes, acc_stats
        ),
    );

    let flush_started = Instant::now();
    let tx = conn.transaction()?;
    acc.flush_to_sqlite(&tx)?;
    if perf_trace_enabled() {
        perf_trace_detail(
            "stream.flush_acc_to_temp_tables",
            flush_started,
            format_args!(
                "stream_object_totals={}	stream_object_field_facts={}	stream_array_member_facts={}	stream_item_schema_counts={}	stream_item_schema_index_refs={}	stream_item_schema_field_counts={}",
                sqlite_table_count(&tx, "stream_object_totals")?,
                sqlite_table_count(&tx, "stream_object_field_facts")?,
                sqlite_table_count(&tx, "stream_array_member_facts")?,
                sqlite_table_count(&tx, "stream_item_schema_counts")?,
                sqlite_table_count(&tx, "stream_item_schema_index_refs")?,
                sqlite_table_count(&tx, "stream_item_schema_field_counts")?,
            ),
        );
    }
    let commit_started = Instant::now();
    tx.commit()?;
    perf_trace("stream.flush_acc_commit", commit_started);

    if !saw_value {
        bail!("input is empty");
    }

    Ok(())
}

fn is_incomplete_trailing_jsonl_record(line: &str, error: &serde_json::Error) -> bool {
    !line.ends_with('\n') && error.is_eof()
}

#[cfg(test)]
fn record_streamed_object_occurrence(
    tx: &Transaction<'_>,
    outdir: &Path,
    path_base: &str,
    object: &Map<String, Value>,
    role: Role,
    array_indexes: &[usize],
) -> Result<()> {
    match role {
        Role::Object => {
            tx.execute(
                "INSERT INTO stream_object_totals(path_base, object_count) VALUES (?1, 1) \
                 ON CONFLICT(path_base) DO UPDATE SET object_count = object_count + 1",
                params![path_base],
            )?;
        }
        Role::ArrayItem | Role::RootCollection => {
            let schema = schema_for_single_object(outdir, path_base, object)?;
            let canonical = canonical_json(&schema)?;
            let schema_json = serde_json::to_string(&schema)?;
            let role_name = role_storage_name(role);
            let array_index_path = serde_json::to_string(array_indexes)?;
            let compact = CompactIndexPath::from_slice(array_indexes);
            let mut index_path_key = String::new();
            compact.write_sort_key(&mut index_path_key);

            tx.execute(
                "INSERT INTO stream_item_schema_counts(role, path_base, canonical_json, schema_json, object_count) \
                 VALUES (?1, ?2, ?3, ?4, 1) \
                 ON CONFLICT(role, path_base, canonical_json) DO UPDATE SET object_count = object_count + 1",
                params![role_name, path_base, &canonical, &schema_json],
            )?;
            tx.execute(
                "INSERT INTO stream_item_schema_index_refs(role, path_base, canonical_json, array_index_path, index_path_key) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(role, path_base, array_index_path) DO UPDATE SET \
                 canonical_json = excluded.canonical_json, index_path_key = excluded.index_path_key",
                params![role_name, path_base, &canonical, &array_index_path, &index_path_key],
            )?;

            for field_name in object.keys() {
                tx.execute(
                    "INSERT INTO stream_item_schema_field_counts(role, path_base, canonical_json, field_name, field_count) \
                     VALUES (?1, ?2, ?3, ?4, 1) \
                     ON CONFLICT(role, path_base, canonical_json, field_name) DO UPDATE SET field_count = field_count + 1",
                    params![role_name, path_base, &canonical, field_name],
                )?;
            }
        }
    }

    for (key, child) in object {
        let child_base = child_path_base(path_base, key);
        if role == Role::Object {
            record_object_field_fact(tx, path_base, key, child)?;
            if let Value::Array(items) = child {
                record_array_member_facts(tx, &child_base, 0, items)?;
            }
        }

        match child {
            Value::Object(map) => {
                record_streamed_object_occurrence(
                    tx,
                    outdir,
                    &child_base,
                    map,
                    Role::Object,
                    array_indexes,
                )?;
            }
            Value::Array(items) => {
                record_streamed_array_object_occurrences(
                    tx,
                    outdir,
                    &child_base,
                    items,
                    array_indexes,
                )?;
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
fn record_streamed_array_object_occurrences(
    tx: &Transaction<'_>,
    outdir: &Path,
    path_base: &str,
    items: &[Value],
    array_indexes: &[usize],
) -> Result<()> {
    for (index, item) in items.iter().enumerate() {
        let mut item_indexes = array_indexes.to_vec();
        item_indexes.push(index);
        match item {
            Value::Object(map) => record_streamed_object_occurrence(
                tx,
                outdir,
                path_base,
                map,
                Role::ArrayItem,
                &item_indexes,
            )?,
            Value::Array(nested) => record_streamed_array_object_occurrences(
                tx,
                outdir,
                path_base,
                nested,
                &item_indexes,
            )?,
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
fn record_object_field_fact(
    tx: &Transaction<'_>,
    path_base: &str,
    field_name: &str,
    value: &Value,
) -> Result<()> {
    let counts = ValueTypeCounts::single(value);
    tx.execute(
        "INSERT INTO stream_object_field_facts(\
         path_base, field_name, present_count, array_count, boolean_count, null_count, number_count, object_count, string_count) \
         VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT(path_base, field_name) DO UPDATE SET \
         present_count = present_count + 1, \
         array_count = array_count + excluded.array_count, \
         boolean_count = boolean_count + excluded.boolean_count, \
         null_count = null_count + excluded.null_count, \
         number_count = number_count + excluded.number_count, \
         object_count = object_count + excluded.object_count, \
         string_count = string_count + excluded.string_count",
        params![
            path_base,
            field_name,
            counts.array,
            counts.boolean,
            counts.null,
            counts.number,
            counts.object,
            counts.string,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
fn record_array_member_facts(
    tx: &Transaction<'_>,
    path_base: &str,
    depth: usize,
    members: &[Value],
) -> Result<()> {
    for member in members {
        let counts = ValueTypeCounts::single(member);
        tx.execute(
            "INSERT INTO stream_array_member_facts(\
             path_base, depth, member_count, array_count, boolean_count, null_count, number_count, object_count, string_count) \
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(path_base, depth) DO UPDATE SET \
             member_count = member_count + 1, \
             array_count = array_count + excluded.array_count, \
             boolean_count = boolean_count + excluded.boolean_count, \
             null_count = null_count + excluded.null_count, \
             number_count = number_count + excluded.number_count, \
             object_count = object_count + excluded.object_count, \
             string_count = string_count + excluded.string_count",
            params![
                path_base,
                depth as i64,
                counts.array,
                counts.boolean,
                counts.null,
                counts.number,
                counts.object,
                counts.string,
            ],
        )?;

        if let Value::Array(nested) = member {
            record_array_member_facts(tx, path_base, depth + 1, nested)?;
        }
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy)]
struct ValueTypeCounts {
    array: i64,
    boolean: i64,
    null: i64,
    number: i64,
    object: i64,
    string: i64,
}

impl ValueTypeCounts {
    fn single(value: &Value) -> Self {
        match value {
            Value::Array(_) => Self {
                array: 1,
                boolean: 0,
                null: 0,
                number: 0,
                object: 0,
                string: 0,
            },
            Value::Bool(_) => Self {
                array: 0,
                boolean: 1,
                null: 0,
                number: 0,
                object: 0,
                string: 0,
            },
            Value::Null => Self {
                array: 0,
                boolean: 0,
                null: 1,
                number: 0,
                object: 0,
                string: 0,
            },
            Value::Number(_) => Self {
                array: 0,
                boolean: 0,
                null: 0,
                number: 1,
                object: 0,
                string: 0,
            },
            Value::Object(_) => Self {
                array: 0,
                boolean: 0,
                null: 0,
                number: 0,
                object: 1,
                string: 0,
            },
            Value::String(_) => Self {
                array: 0,
                boolean: 0,
                null: 0,
                number: 0,
                object: 0,
                string: 1,
            },
        }
    }

    fn add(&mut self, other: Self) {
        self.array += other.array;
        self.boolean += other.boolean;
        self.null += other.null;
        self.number += other.number;
        self.object += other.object;
        self.string += other.string;
    }

    fn type_names(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.array > 0 {
            out.push("array");
        }
        if self.boolean > 0 {
            out.push("boolean");
        }
        if self.null > 0 {
            out.push("null");
        }
        if self.number > 0 {
            out.push("number");
        }
        if self.object > 0 {
            out.push("object");
        }
        if self.string > 0 {
            out.push("string");
        }
        out
    }
}

type PathId = Spur;
type FieldId = Spur;
type SegmentId = Spur;

#[derive(Default)]
struct StreamInterns {
    paths: Rodeo,
    fields: Rodeo,
    segments: Rodeo,
    field_segments: HashMap<FieldId, SegmentId>,
    child_paths: HashMap<(PathId, SegmentId), PathId>,
}

impl StreamInterns {
    fn intern_path(&mut self, path: &str) -> PathId {
        self.paths.get_or_intern(path)
    }

    fn intern_field(&mut self, field_name: &str) -> FieldId {
        self.fields.get_or_intern(field_name)
    }

    fn resolve_path(&self, path_id: PathId) -> &str {
        self.paths.resolve(&path_id)
    }

    fn resolve_field(&self, field_id: FieldId) -> &str {
        self.fields.resolve(&field_id)
    }

    fn resolve_segment(&self, segment_id: SegmentId) -> &str {
        self.segments.resolve(&segment_id)
    }

    fn intern_escaped_segment_for_field(&mut self, field_id: FieldId) -> SegmentId {
        if let Some(&segment_id) = self.field_segments.get(&field_id) {
            return segment_id;
        }

        let escaped = path_segment(self.resolve_field(field_id));
        let segment_id = self.segments.get_or_intern(escaped);
        self.field_segments.insert(field_id, segment_id);
        segment_id
    }

    fn child_path(&mut self, parent: PathId, field_id: FieldId) -> PathId {
        let segment_id = self.intern_escaped_segment_for_field(field_id);
        if let Some(&child_path_id) = self.child_paths.get(&(parent, segment_id)) {
            return child_path_id;
        }

        let mut path = String::with_capacity(
            self.resolve_path(parent).len() + 1 + self.resolve_segment(segment_id).len(),
        );
        path.push_str(self.resolve_path(parent));
        path.push('/');
        path.push_str(self.resolve_segment(segment_id));
        let child_path_id = self.paths.get_or_intern(path);
        self.child_paths.insert((parent, segment_id), child_path_id);
        child_path_id
    }
}

#[derive(Default)]
struct StreamAcc {
    interns: StreamInterns,
    object_totals: HashMap<PathId, i64>,
    object_field_facts: HashMap<(PathId, FieldId), ObjectFieldAggregate>,
    array_member_facts: HashMap<(PathId, usize), ArrayMemberAggregate>,
    item_schema_counts: HashMap<(Role, PathId, String), ItemSchemaCountAggregate>,
    item_schema_index_refs: HashMap<ItemSchemaIndexRefKey, ItemSchemaIndexRefAggregate>,
    item_schema_field_counts: HashMap<(Role, PathId, String, FieldId), i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ItemSchemaIndexRefKey {
    role: Role,
    path_base: PathId,
    index_path: CompactIndexPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum CompactIndexPath {
    One(usize),
    Two(usize, usize),
    Three(usize, usize, usize),
    Many(Vec<usize>),
}

impl CompactIndexPath {
    fn from_slice(index_path: &[usize]) -> Self {
        match index_path {
            [a] => Self::One(*a),
            [a, b] => Self::Two(*a, *b),
            [a, b, c] => Self::Three(*a, *b, *c),
            _ => Self::Many(index_path.to_vec()),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::One(_) => 1,
            Self::Two(_, _) => 2,
            Self::Three(_, _, _) => 3,
            Self::Many(indexes) => indexes.len(),
        }
    }

    fn write_json_string(&self, out: &mut String) {
        out.clear();
        out.push('[');
        self.write_csv(out);
        out.push(']');
    }

    fn write_sort_key(&self, out: &mut String) {
        out.clear();
        self.write_padded_csv(out);
    }

    fn write_csv(&self, out: &mut String) {
        match self {
            Self::One(a) => out.push_str(&a.to_string()),
            Self::Two(a, b) => {
                out.push_str(&a.to_string());
                out.push(',');
                out.push_str(&b.to_string());
            }
            Self::Three(a, b, c) => {
                out.push_str(&a.to_string());
                out.push(',');
                out.push_str(&b.to_string());
                out.push(',');
                out.push_str(&c.to_string());
            }
            Self::Many(indexes) => {
                for (position, index) in indexes.iter().enumerate() {
                    if position > 0 {
                        out.push(',');
                    }
                    out.push_str(&index.to_string());
                }
            }
        }
    }

    fn write_padded_csv(&self, out: &mut String) {
        match self {
            Self::One(a) => push_padded_index(out, *a),
            Self::Two(a, b) => {
                push_padded_index(out, *a);
                out.push('/');
                push_padded_index(out, *b);
            }
            Self::Three(a, b, c) => {
                push_padded_index(out, *a);
                out.push('/');
                push_padded_index(out, *b);
                out.push('/');
                push_padded_index(out, *c);
            }
            Self::Many(indexes) => {
                for (position, index) in indexes.iter().enumerate() {
                    if position > 0 {
                        out.push('/');
                    }
                    push_padded_index(out, *index);
                }
            }
        }
    }
}

fn push_padded_index(out: &mut String, index: usize) {
    use std::fmt::Write;
    let _ = write!(out, "{index:020}");
}

#[derive(Debug)]
struct StreamAccStats {
    object_totals: usize,
    object_field_facts: usize,
    array_member_facts: usize,
    item_schema_counts: usize,
    item_schema_index_refs: usize,
    item_schema_field_counts: usize,
}

impl std::fmt::Display for StreamAccStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "acc_object_totals={}\tacc_object_field_facts={}\tacc_array_member_facts={}\tacc_item_schema_counts={}\tacc_item_schema_index_refs={}\tacc_item_schema_field_counts={}",
            self.object_totals,
            self.object_field_facts,
            self.array_member_facts,
            self.item_schema_counts,
            self.item_schema_index_refs,
            self.item_schema_field_counts
        )
    }
}

impl StreamAcc {
    fn intern_path(&mut self, path: &str) -> PathId {
        self.interns.intern_path(path)
    }

    fn child_path(&mut self, parent: PathId, field_name: &str) -> (PathId, FieldId) {
        let field_id = self.interns.intern_field(field_name);
        let child_path = self.interns.child_path(parent, field_id);
        (child_path, field_id)
    }

    fn stats(&self) -> StreamAccStats {
        StreamAccStats {
            object_totals: self.object_totals.len(),
            object_field_facts: self.object_field_facts.len(),
            array_member_facts: self.array_member_facts.len(),
            item_schema_counts: self.item_schema_counts.len(),
            item_schema_index_refs: self.item_schema_index_refs.len(),
            item_schema_field_counts: self.item_schema_field_counts.len(),
        }
    }

    fn add_object_total(&mut self, path_base: PathId) {
        *self.object_totals.entry(path_base).or_default() += 1;
    }

    fn add_object_field_fact(&mut self, path_base: PathId, field_id: FieldId, value: &Value) {
        let aggregate = self
            .object_field_facts
            .entry((path_base, field_id))
            .or_default();
        aggregate.present_count += 1;
        aggregate.counts.add(ValueTypeCounts::single(value));
    }

    fn add_array_member_fact(&mut self, path_base: PathId, depth: usize, value: &Value) {
        let aggregate = self
            .array_member_facts
            .entry((path_base, depth))
            .or_default();
        aggregate.member_count += 1;
        aggregate.counts.add(ValueTypeCounts::single(value));
    }

    fn add_item_schema(
        &mut self,
        role: Role,
        path_base: PathId,
        object: &Map<String, Value>,
        outdir: &Path,
        array_indexes: &[usize],
    ) -> Result<()> {
        let path_base_text = self.interns.resolve_path(path_base).to_string();
        let schema = schema_for_single_object(outdir, &path_base_text, object)?;
        let canonical_json = canonical_json(&schema)?;
        let schema_json = serde_json::to_string(&schema)?;
        let key = (role, path_base, canonical_json.clone());
        let count =
            self.item_schema_counts
                .entry(key)
                .or_insert_with(|| ItemSchemaCountAggregate {
                    schema_json: schema_json.clone(),
                    object_count: 0,
                });
        count.object_count += 1;

        let index_ref_key = ItemSchemaIndexRefKey {
            role,
            path_base,
            index_path: CompactIndexPath::from_slice(array_indexes),
        };
        self.item_schema_index_refs.insert(
            index_ref_key,
            ItemSchemaIndexRefAggregate {
                canonical_json: canonical_json.clone(),
            },
        );

        for field_name in object.keys() {
            let field_id = self.interns.intern_field(field_name);
            *self
                .item_schema_field_counts
                .entry((role, path_base, canonical_json.clone(), field_id))
                .or_default() += 1;
        }

        Ok(())
    }

    fn flush_to_sqlite(self, tx: &Transaction<'_>) -> Result<()> {
        let StreamAcc {
            interns,
            object_totals,
            object_field_facts,
            array_member_facts,
            item_schema_counts,
            item_schema_index_refs,
            item_schema_field_counts,
        } = self;

        let section_started = Instant::now();
        let mut object_totals = object_totals.into_iter().collect::<Vec<_>>();
        object_totals.sort_by(|a, b| interns.resolve_path(a.0).cmp(interns.resolve_path(b.0)));
        let object_totals_len = object_totals.len();
        for (path_base, object_count) in object_totals {
            tx.execute(
                "INSERT INTO stream_object_totals(path_base, object_count)
                 VALUES (?1, ?2)
                 ON CONFLICT(path_base) DO UPDATE SET
                     object_count = object_count + excluded.object_count",
                params![interns.resolve_path(path_base), object_count],
            )?;
        }
        perf_trace_detail(
            "stream.flush.object_totals",
            section_started,
            format_args!("rows={object_totals_len}"),
        );

        let section_started = Instant::now();
        let mut object_field_facts = object_field_facts.into_iter().collect::<Vec<_>>();
        object_field_facts.sort_by(|a, b| {
            interns
                .resolve_path((a.0).0)
                .cmp(interns.resolve_path((b.0).0))
                .then_with(|| {
                    interns
                        .resolve_field((a.0).1)
                        .cmp(interns.resolve_field((b.0).1))
                })
        });
        let object_field_facts_len = object_field_facts.len();
        for ((path_base, field_name), aggregate) in object_field_facts {
            tx.execute(
                "INSERT INTO stream_object_field_facts(
                     path_base, field_name, present_count, array_count, boolean_count,
                     null_count, number_count, object_count, string_count
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(path_base, field_name) DO UPDATE SET
                     present_count = present_count + excluded.present_count,
                     array_count = array_count + excluded.array_count,
                     boolean_count = boolean_count + excluded.boolean_count,
                     null_count = null_count + excluded.null_count,
                     number_count = number_count + excluded.number_count,
                     object_count = object_count + excluded.object_count,
                     string_count = string_count + excluded.string_count",
                params![
                    interns.resolve_path(path_base),
                    interns.resolve_field(field_name),
                    aggregate.present_count,
                    aggregate.counts.array,
                    aggregate.counts.boolean,
                    aggregate.counts.null,
                    aggregate.counts.number,
                    aggregate.counts.object,
                    aggregate.counts.string,
                ],
            )?;
        }
        perf_trace_detail(
            "stream.flush.object_field_facts",
            section_started,
            format_args!("rows={object_field_facts_len}"),
        );

        let section_started = Instant::now();
        let mut array_member_facts = array_member_facts.into_iter().collect::<Vec<_>>();
        array_member_facts.sort_by(|a, b| {
            interns
                .resolve_path((a.0).0)
                .cmp(interns.resolve_path((b.0).0))
                .then_with(|| (a.0).1.cmp(&(b.0).1))
        });
        let array_member_facts_len = array_member_facts.len();
        for ((path_base, depth), aggregate) in array_member_facts {
            tx.execute(
                "INSERT INTO stream_array_member_facts(
                     path_base, depth, member_count, array_count, boolean_count,
                     null_count, number_count, object_count, string_count
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(path_base, depth) DO UPDATE SET
                     member_count = member_count + excluded.member_count,
                     array_count = array_count + excluded.array_count,
                     boolean_count = boolean_count + excluded.boolean_count,
                     null_count = null_count + excluded.null_count,
                     number_count = number_count + excluded.number_count,
                     object_count = object_count + excluded.object_count,
                     string_count = string_count + excluded.string_count",
                params![
                    interns.resolve_path(path_base),
                    depth as i64,
                    aggregate.member_count,
                    aggregate.counts.array,
                    aggregate.counts.boolean,
                    aggregate.counts.null,
                    aggregate.counts.number,
                    aggregate.counts.object,
                    aggregate.counts.string,
                ],
            )?;
        }
        perf_trace_detail(
            "stream.flush.array_member_facts",
            section_started,
            format_args!("rows={array_member_facts_len}"),
        );

        let section_started = Instant::now();
        let mut item_schema_counts = item_schema_counts.into_iter().collect::<Vec<_>>();
        item_schema_counts.sort_by(|a, b| {
            role_storage_name((a.0).0)
                .cmp(role_storage_name((b.0).0))
                .then_with(|| {
                    interns
                        .resolve_path((a.0).1)
                        .cmp(interns.resolve_path((b.0).1))
                })
                .then_with(|| (a.0).2.cmp(&(b.0).2))
        });
        let item_schema_counts_len = item_schema_counts.len();
        for ((role, path_base, canonical_json), aggregate) in item_schema_counts {
            tx.execute(
                "INSERT INTO stream_item_schema_counts(
                     role, path_base, canonical_json, schema_json, object_count
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(role, path_base, canonical_json) DO UPDATE SET
                     object_count = object_count + excluded.object_count",
                params![
                    role_storage_name(role),
                    interns.resolve_path(path_base),
                    canonical_json,
                    aggregate.schema_json,
                    aggregate.object_count,
                ],
            )?;
        }
        perf_trace_detail(
            "stream.flush.item_schema_counts",
            section_started,
            format_args!("rows={item_schema_counts_len}"),
        );

        let section_started = Instant::now();
        let mut item_schema_index_refs = item_schema_index_refs.into_iter().collect::<Vec<_>>();
        let item_schema_index_refs_len = item_schema_index_refs.len();
        let sort_started = Instant::now();
        item_schema_index_refs.sort_by(|a, b| {
            role_storage_name(a.0.role)
                .cmp(role_storage_name(b.0.role))
                .then_with(|| {
                    interns
                        .resolve_path(a.0.path_base)
                        .cmp(interns.resolve_path(b.0.path_base))
                })
                .then_with(|| a.0.index_path.cmp(&b.0.index_path))
        });
        perf_trace_detail(
            "stream.flush.item_schema_index_refs.sort",
            sort_started,
            format_args!("rows={item_schema_index_refs_len}"),
        );

        let insert_started = Instant::now();
        let mut compact_len_one = 0usize;
        let mut compact_len_two = 0usize;
        let mut compact_len_three = 0usize;
        let mut compact_len_many = 0usize;
        let mut total_index_depth = 0usize;
        let mut max_index_depth = 0usize;
        let mut array_index_path = String::new();
        let mut index_path_key = String::new();
        let mut stmt = tx.prepare(
            "INSERT INTO stream_item_schema_index_refs(
                 role, path_base, canonical_json, array_index_path, index_path_key
             )
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(role, path_base, array_index_path) DO UPDATE SET
                 canonical_json = excluded.canonical_json,
                 index_path_key = excluded.index_path_key",
        )?;
        for (key, aggregate) in item_schema_index_refs {
            let index_depth = key.index_path.len();
            total_index_depth += index_depth;
            max_index_depth = max_index_depth.max(index_depth);
            match index_depth {
                1 => compact_len_one += 1,
                2 => compact_len_two += 1,
                3 => compact_len_three += 1,
                _ => compact_len_many += 1,
            }
            key.index_path.write_json_string(&mut array_index_path);
            key.index_path.write_sort_key(&mut index_path_key);
            stmt.execute(params![
                role_storage_name(key.role),
                interns.resolve_path(key.path_base),
                aggregate.canonical_json,
                &array_index_path,
                &index_path_key,
            ])?;
        }
        drop(stmt);
        perf_trace_detail(
            "stream.flush.item_schema_index_refs.insert",
            insert_started,
            format_args!(
                "rows={item_schema_index_refs_len}\tlen1={compact_len_one}\tlen2={compact_len_two}\tlen3={compact_len_three}\tlen_many={compact_len_many}\ttotal_index_depth={total_index_depth}\tmax_index_depth={max_index_depth}"
            ),
        );
        perf_trace_detail(
            "stream.flush.item_schema_index_refs",
            section_started,
            format_args!("rows={item_schema_index_refs_len}"),
        );

        let section_started = Instant::now();
        let mut item_schema_field_counts = item_schema_field_counts.into_iter().collect::<Vec<_>>();
        item_schema_field_counts.sort_by(|a, b| {
            role_storage_name((a.0).0)
                .cmp(role_storage_name((b.0).0))
                .then_with(|| {
                    interns
                        .resolve_path((a.0).1)
                        .cmp(interns.resolve_path((b.0).1))
                })
                .then_with(|| (a.0).2.cmp(&(b.0).2))
                .then_with(|| {
                    interns
                        .resolve_field((a.0).3)
                        .cmp(interns.resolve_field((b.0).3))
                })
        });
        let item_schema_field_counts_len = item_schema_field_counts.len();
        for ((role, path_base, canonical_json, field_name), field_count) in item_schema_field_counts
        {
            tx.execute(
                "INSERT INTO stream_item_schema_field_counts(
                     role, path_base, canonical_json, field_name, field_count
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(role, path_base, canonical_json, field_name) DO UPDATE SET
                     field_count = field_count + excluded.field_count",
                params![
                    role_storage_name(role),
                    interns.resolve_path(path_base),
                    canonical_json,
                    interns.resolve_field(field_name),
                    field_count,
                ],
            )?;
        }
        perf_trace_detail(
            "stream.flush.item_schema_field_counts",
            section_started,
            format_args!("rows={item_schema_field_counts_len}"),
        );

        Ok(())
    }
}

#[derive(Debug, Default)]
struct ObjectFieldAggregate {
    present_count: i64,
    counts: ValueTypeCounts,
}

#[derive(Debug, Default)]
struct ArrayMemberAggregate {
    member_count: i64,
    counts: ValueTypeCounts,
}

#[derive(Debug)]
struct ItemSchemaCountAggregate {
    schema_json: String,
    object_count: i64,
}

#[derive(Debug)]
struct ItemSchemaIndexRefAggregate {
    canonical_json: String,
}

fn record_streamed_object_occurrence_to_acc(
    acc: &mut StreamAcc,
    outdir: &Path,
    path_base: &str,
    object: &Map<String, Value>,
    role: Role,
    array_indexes: &[usize],
) -> Result<()> {
    let path_base = acc.intern_path(path_base);
    record_streamed_object_occurrence_to_acc_id(acc, outdir, path_base, object, role, array_indexes)
}

fn record_streamed_object_occurrence_to_acc_id(
    acc: &mut StreamAcc,
    outdir: &Path,
    path_base: PathId,
    object: &Map<String, Value>,
    role: Role,
    array_indexes: &[usize],
) -> Result<()> {
    match role {
        Role::Object => acc.add_object_total(path_base),
        Role::ArrayItem | Role::RootCollection => {
            acc.add_item_schema(role, path_base, object, outdir, array_indexes)?;
        }
    }

    for (key, child) in object {
        let (child_base, field_id) = acc.child_path(path_base, key);
        if role == Role::Object {
            acc.add_object_field_fact(path_base, field_id, child);
            if let Value::Array(items) = child {
                record_array_member_facts_to_acc_id(acc, child_base, 0, items);
            }
        }

        match child {
            Value::Object(map) => {
                record_streamed_object_occurrence_to_acc_id(
                    acc,
                    outdir,
                    child_base,
                    map,
                    Role::Object,
                    array_indexes,
                )?;
            }
            Value::Array(items) => {
                record_streamed_array_object_occurrences_to_acc_id(
                    acc,
                    outdir,
                    child_base,
                    items,
                    array_indexes,
                )?;
            }
            _ => {}
        }
    }

    Ok(())
}

fn record_streamed_array_object_occurrences_to_acc_id(
    acc: &mut StreamAcc,
    outdir: &Path,
    path_base: PathId,
    items: &[Value],
    array_indexes: &[usize],
) -> Result<()> {
    for (index, item) in items.iter().enumerate() {
        let mut item_indexes = array_indexes.to_vec();
        item_indexes.push(index);
        match item {
            Value::Object(map) => record_streamed_object_occurrence_to_acc_id(
                acc,
                outdir,
                path_base,
                map,
                Role::ArrayItem,
                &item_indexes,
            )?,
            Value::Array(nested) => record_streamed_array_object_occurrences_to_acc_id(
                acc,
                outdir,
                path_base,
                nested,
                &item_indexes,
            )?,
            _ => {}
        }
    }

    Ok(())
}

fn record_array_member_facts_to_acc_id(
    acc: &mut StreamAcc,
    path_base: PathId,
    depth: usize,
    members: &[Value],
) {
    for member in members {
        acc.add_array_member_fact(path_base, depth, member);
        if let Value::Array(nested) = member {
            record_array_member_facts_to_acc_id(acc, path_base, depth + 1, nested);
        }
    }
}

#[derive(Debug)]
struct StreamFieldFacts {
    field_name: String,
    present_count: i64,
    counts: ValueTypeCounts,
}

#[derive(Debug)]
struct StreamArrayFacts {
    member_count: i64,
    counts: ValueTypeCounts,
}

fn role_storage_name(role: Role) -> &'static str {
    match role {
        Role::Object => "object",
        Role::ArrayItem => "array_item",
        Role::RootCollection => "root_collection",
    }
}

fn child_path_base(path_base: &str, key: &str) -> String {
    format!("{}/{}", path_base, path_segment(key))
}

fn schema_for_single_object(
    outdir: &Path,
    path_base: &str,
    object: &Map<String, Value>,
) -> Result<Value> {
    let _ = outdir;
    let mut schema = Map::new();
    for key in object.keys().collect::<BTreeSet<_>>() {
        let child_base = child_path_base(path_base, key);
        let label = label_for_single_value(&child_base, &object[key])?;
        schema.insert(key.clone(), Value::String(label));
    }
    Ok(Value::Object(schema))
}

fn label_for_single_value(path_base: &str, value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok("null".to_string()),
        Value::Bool(_) => Ok("boolean".to_string()),
        Value::Number(_) => Ok("number".to_string()),
        Value::String(_) => Ok("string".to_string()),
        Value::Object(_) => Ok(format!("{path_base}.json")),
        Value::Array(items) => Ok(format!(
            "array({})",
            array_member_label_for_single_array(path_base, items)?
        )),
    }
}

fn array_member_label_for_single_array(path_base: &str, members: &[Value]) -> Result<String> {
    let members = members.iter().collect::<Vec<_>>();
    array_member_label_for_single_members(path_base, &members)
}

fn array_member_label_for_single_members(path_base: &str, members: &[&Value]) -> Result<String> {
    if members.is_empty() {
        return Ok("empty".to_string());
    }

    let types = unique_types(members.iter().copied());
    let mut labels = BTreeSet::new();
    for primitive in ["boolean", "number", "string", "null"] {
        if types.contains(&primitive) {
            labels.insert(primitive.to_string());
        }
    }
    if types.contains(&"object") {
        labels.insert(format!("{path_base}.json"));
    }
    if types.contains(&"array") {
        let nested = members
            .iter()
            .filter_map(|value| value.as_array())
            .flat_map(|items| items.iter())
            .collect::<Vec<_>>();
        labels.insert(format!(
            "array({})",
            array_member_label_for_single_members(path_base, &nested)?
        ));
    }
    Ok(union_member_labels(labels))
}

fn finalize_streamed_jsonl(
    conn: &mut Connection,
    outdir: &Path,
    compact_output: bool,
) -> Result<()> {
    let tx_started = Instant::now();
    let tx = conn.transaction()?;
    perf_trace("finalize.begin_transaction", tx_started);

    let stage_started = Instant::now();
    create_schema_index_tables(&tx)?;
    perf_trace("finalize.create_schema_index_tables", stage_started);

    let mut made_dirs = HashMap::<PathBuf, ()>::new();

    let stage_started = Instant::now();
    write_streamed_object_records(&tx, outdir, compact_output, &mut made_dirs)?;
    if perf_trace_enabled() {
        perf_trace_detail(
            "finalize.write_object_records",
            stage_started,
            format_args!(
                "schema_definitions={}	schema_paths={}",
                sqlite_table_count(&tx, "schema_definitions")?,
                sqlite_table_count(&tx, "schema_paths")?,
            ),
        );
    }

    let stage_started = Instant::now();
    write_streamed_item_records(&tx, Role::ArrayItem, outdir, compact_output, &mut made_dirs)?;
    if perf_trace_enabled() {
        perf_trace_detail(
            "finalize.write_array_item_records",
            stage_started,
            format_args!(
                "schema_definitions={}	array_index_refs={}",
                sqlite_table_count(&tx, "schema_definitions")?,
                sqlite_table_count(&tx, "array_index_refs")?,
            ),
        );
    }

    let stage_started = Instant::now();
    write_streamed_item_records(
        &tx,
        Role::RootCollection,
        outdir,
        compact_output,
        &mut made_dirs,
    )?;
    if perf_trace_enabled() {
        perf_trace_detail(
            "finalize.write_root_collection_records",
            stage_started,
            format_args!(
                "schema_definitions={}	array_index_refs={}",
                sqlite_table_count(&tx, "schema_definitions")?,
                sqlite_table_count(&tx, "array_index_refs")?,
            ),
        );
    }

    let stage_started = Instant::now();
    write_streamed_occurrence_statistics(&tx)?;
    if perf_trace_enabled() {
        perf_trace_detail(
            "finalize.write_occurrence_statistics",
            stage_started,
            format_args!(
                "schema_object_counts={}	schema_field_counts={}",
                sqlite_table_count(&tx, "schema_object_counts")?,
                sqlite_table_count(&tx, "schema_field_counts")?,
            ),
        );
    }

    let stage_started = Instant::now();
    write_schema_relations_from_sqlite(&tx)?;
    if perf_trace_enabled() {
        perf_trace_detail(
            "finalize.write_schema_relations",
            stage_started,
            format_args!(
                "schema_relations={}",
                sqlite_table_count(&tx, "schema_relations")?
            ),
        );
    }

    let stage_started = Instant::now();
    tx.commit()?;
    perf_trace("finalize.commit", stage_started);
    Ok(())
}

fn create_schema_index_tables(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE schema_paths (schema_path TEXT NOT NULL, object_path TEXT PRIMARY KEY);
         CREATE TABLE array_index_refs (array_path TEXT NOT NULL, array_index_path TEXT NOT NULL, schema_path TEXT NOT NULL, PRIMARY KEY (array_path, array_index_path));
         CREATE TABLE schema_definitions (schema_path TEXT PRIMARY KEY, schema_kind TEXT NOT NULL, schema_json TEXT NOT NULL);
         CREATE TABLE schema_object_counts (schema_path TEXT PRIMARY KEY, object_count INTEGER NOT NULL CHECK (object_count > 0));
         CREATE TABLE schema_field_counts (schema_path TEXT NOT NULL, field_name TEXT NOT NULL, field_type TEXT NOT NULL, field_count INTEGER NOT NULL CHECK (field_count > 0), PRIMARY KEY (schema_path, field_name));
         CREATE TABLE schema_relations (relation_id INTEGER PRIMARY KEY AUTOINCREMENT, from_schema_path TEXT NOT NULL, to_schema_path TEXT NOT NULL, relation_kind TEXT NOT NULL, fk_owner TEXT NOT NULL, fk_candidate INTEGER NOT NULL CHECK (fk_candidate IN (0, 1)), field_name TEXT NOT NULL, field_type TEXT NOT NULL, cardinality TEXT NOT NULL, required INTEGER NOT NULL CHECK (required IN (0, 1)), mixed INTEGER NOT NULL CHECK (mixed IN (0, 1)), nested_array_depth INTEGER NOT NULL DEFAULT 0 CHECK (nested_array_depth >= 0), via_schema_path TEXT, via_array_path TEXT, parent_schema_path TEXT NOT NULL, child_schema_path TEXT NOT NULL, parent_object_count INTEGER NOT NULL DEFAULT 0 CHECK (parent_object_count >= 0), child_object_count INTEGER NOT NULL DEFAULT 0 CHECK (child_object_count >= 0), field_count INTEGER NOT NULL DEFAULT 0 CHECK (field_count >= 0), UNIQUE (from_schema_path, to_schema_path, relation_kind, field_name, field_type, via_schema_path, via_array_path));
         CREATE INDEX idx_schema_relations_from ON schema_relations(from_schema_path);
         CREATE INDEX idx_schema_relations_to ON schema_relations(to_schema_path);
         CREATE INDEX idx_schema_relations_kind ON schema_relations(relation_kind, fk_candidate);
         CREATE INDEX idx_schema_relations_parent_child ON schema_relations(parent_schema_path, child_schema_path);",
    )?;
    Ok(())
}

fn write_streamed_object_records(
    tx: &Transaction<'_>,
    outdir: &Path,
    compact_output: bool,
    made_dirs: &mut HashMap<PathBuf, ()>,
) -> Result<()> {
    let path_bases = {
        let mut stmt =
            tx.prepare("SELECT path_base FROM stream_object_totals ORDER BY path_base")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    for path_base in path_bases {
        let schema = schema_for_streamed_object_path(tx, &path_base)?;
        let canonical = canonical_json(&schema)?;
        let schema_json = serde_json::to_string(&schema)?;
        let object_path = format!("{path_base}.json");
        tx.execute(
            "INSERT INTO stream_object_schema_candidates(canonical_json, schema_json, object_path) VALUES (?1, ?2, ?3)",
            params![canonical, schema_json, object_path],
        )?;
    }

    let canonical_groups = {
        let mut stmt = tx.prepare(
            "SELECT canonical_json, MIN(schema_json), MIN(object_path) \
             FROM stream_object_schema_candidates \
             GROUP BY canonical_json \
             ORDER BY canonical_json",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    for (canonical, schema_json, schema_path) in canonical_groups {
        let object_paths = {
            let mut stmt = tx.prepare(
                "SELECT object_path FROM stream_object_schema_candidates \
                 WHERE canonical_json = ?1 ORDER BY object_path",
            )?;
            let rows = stmt
                .query_map([&canonical], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        insert_streamed_schema_record(
            tx,
            &schema_path,
            &schema_json,
            &object_paths,
            compact_output,
            made_dirs,
        )?;
    }

    let _ = outdir;
    Ok(())
}

fn schema_for_streamed_object_path(tx: &Transaction<'_>, path_base: &str) -> Result<Value> {
    let total = tx.query_row(
        "SELECT object_count FROM stream_object_totals WHERE path_base = ?1",
        [path_base],
        |row| row.get::<_, i64>(0),
    )?;

    let facts = {
        let mut stmt = tx.prepare(
            "SELECT field_name, present_count, array_count, boolean_count, null_count, number_count, object_count, string_count \
             FROM stream_object_field_facts \
             WHERE path_base = ?1 \
             ORDER BY field_name",
        )?;
        let rows = stmt
            .query_map([path_base], |row| {
                Ok(StreamFieldFacts {
                    field_name: row.get(0)?,
                    present_count: row.get(1)?,
                    counts: ValueTypeCounts {
                        array: row.get(2)?,
                        boolean: row.get(3)?,
                        null: row.get(4)?,
                        number: row.get(5)?,
                        object: row.get(6)?,
                        string: row.get(7)?,
                    },
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    let mut schema = Map::new();
    for fact in facts {
        let child_base = child_path_base(path_base, &fact.field_name);
        let label = streamed_field_label(tx, &child_base, &fact, total)?;
        schema.insert(fact.field_name, Value::String(label));
    }
    Ok(Value::Object(schema))
}

fn streamed_field_label(
    tx: &Transaction<'_>,
    child_base: &str,
    fact: &StreamFieldFacts,
    total: i64,
) -> Result<String> {
    let types = fact.counts.type_names();
    if fact.counts.array > 0 {
        let mut labels = BTreeSet::new();
        labels.insert(streamed_array_label(tx, child_base)?);
        for value_type in types {
            if !matches!(value_type, "array" | "null") {
                labels.insert(value_type.to_string());
            }
        }
        return Ok(union_member_labels(labels));
    }

    if fact.counts.object > 0 {
        if types
            .iter()
            .all(|value_type| matches!(*value_type, "object" | "null"))
        {
            return Ok(format!("{child_base}.json"));
        }
        return Ok(types
            .iter()
            .filter(|value_type| **value_type != "null")
            .copied()
            .collect::<Vec<_>>()
            .join("|"));
    }

    Ok(primitive_label_from_counts(fact, total))
}

fn primitive_label_from_counts(fact: &StreamFieldFacts, total: i64) -> String {
    let types = fact.counts.type_names();
    let mut result = if types == vec!["null"] {
        "null".to_string()
    } else if types.len() == 1 {
        types[0].to_string()
    } else if types.len() == 2 && types[0] == "null" {
        format!("{}?", types[1])
    } else {
        types.join("|")
    };

    if fact.present_count < total && result != "null" && !result.ends_with('?') {
        result.push('?');
    }
    result
}

fn streamed_array_label(tx: &Transaction<'_>, path_base: &str) -> Result<String> {
    Ok(format!(
        "array({})",
        streamed_array_member_label(tx, path_base, 0)?
    ))
}

fn streamed_array_member_label(
    tx: &Transaction<'_>,
    path_base: &str,
    depth: usize,
) -> Result<String> {
    let facts = tx
        .query_row(
            "SELECT member_count, array_count, boolean_count, null_count, number_count, object_count, string_count \
             FROM stream_array_member_facts WHERE path_base = ?1 AND depth = ?2",
            params![path_base, depth as i64],
            |row| {
                Ok(StreamArrayFacts {
                    member_count: row.get(0)?,
                    counts: ValueTypeCounts {
                        array: row.get(1)?,
                        boolean: row.get(2)?,
                        null: row.get(3)?,
                        number: row.get(4)?,
                        object: row.get(5)?,
                        string: row.get(6)?,
                    },
                })
            },
        )
        .optional()?;

    let Some(facts) = facts else {
        return Ok("empty".to_string());
    };
    if facts.member_count == 0 {
        return Ok("empty".to_string());
    }

    let mut labels = BTreeSet::new();
    if facts.counts.boolean > 0 {
        labels.insert("boolean".to_string());
    }
    if facts.counts.number > 0 {
        labels.insert("number".to_string());
    }
    if facts.counts.string > 0 {
        labels.insert("string".to_string());
    }
    if facts.counts.null > 0 {
        labels.insert("null".to_string());
    }
    if facts.counts.object > 0 {
        labels.insert(format!("{path_base}.json"));
    }
    if facts.counts.array > 0 {
        labels.insert(format!(
            "array({})",
            streamed_array_member_label(tx, path_base, depth + 1)?
        ));
    }
    Ok(union_member_labels(labels))
}

fn write_streamed_item_records(
    tx: &Transaction<'_>,
    role: Role,
    outdir: &Path,
    compact_output: bool,
    made_dirs: &mut HashMap<PathBuf, ()>,
) -> Result<()> {
    let role_name = role_storage_name(role);
    let path_bases = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT path_base FROM stream_item_schema_counts \
             WHERE role = ?1 ORDER BY path_base",
        )?;
        let rows = stmt
            .query_map([role_name], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    for path_base in path_bases {
        let schema_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM stream_item_schema_counts WHERE role = ?1 AND path_base = ?2",
            params![role_name, &path_base],
            |row| row.get(0),
        )?;

        if schema_count == 1 {
            let (canonical, schema_json) = tx.query_row(
                "SELECT canonical_json, schema_json FROM stream_item_schema_counts \
                 WHERE role = ?1 AND path_base = ?2",
                params![role_name, &path_base],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?;
            let schema_path = format!("{path_base}.json");
            let object_paths = vec![schema_path.clone()];
            insert_streamed_schema_record(
                tx,
                &schema_path,
                &schema_json,
                &object_paths,
                compact_output,
                made_dirs,
            )?;
            insert_streamed_array_index_refs_for_item_schema(
                tx,
                &schema_path,
                &schema_path,
                role_name,
                &path_base,
                &canonical,
            )?;
            insert_item_schema_path(tx, role_name, &path_base, &canonical, &schema_path)?;
            continue;
        }

        if role == Role::ArrayItem {
            let container_path = format!("{path_base}.json");
            let container_schema = json!({"$refs_mut": format!("{path_base}/")});
            let container_json = serde_json::to_string(&container_schema)?;
            insert_streamed_schema_record(
                tx,
                &container_path,
                &container_json,
                std::slice::from_ref(&container_path),
                compact_output,
                made_dirs,
            )?;
        }

        let variants = {
            let mut stmt = tx.prepare(
                "SELECT c.canonical_json, c.schema_json, MIN(r.index_path_key) \
                 FROM stream_item_schema_counts AS c \
                 JOIN stream_item_schema_index_refs AS r \
                   ON r.role = c.role AND r.path_base = c.path_base AND r.canonical_json = c.canonical_json \
                 WHERE c.role = ?1 AND c.path_base = ?2 \
                 GROUP BY c.canonical_json, c.schema_json \
                 ORDER BY MIN(r.index_path_key)",
            )?;
            let rows = stmt
                .query_map(params![role_name, &path_base], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        for (canonical, schema_json, _first_key) in variants {
            let first_index_path: String = tx.query_row(
                "SELECT array_index_path FROM stream_item_schema_index_refs \
                 WHERE role = ?1 AND path_base = ?2 AND canonical_json = ?3 \
                 ORDER BY index_path_key LIMIT 1",
                params![role_name, &path_base, &canonical],
                |row| row.get(0),
            )?;
            let index_path = serde_json::from_str::<Vec<usize>>(&first_index_path)
                .with_context(|| format!("invalid stored array index path: {first_index_path}"))?;
            let file_stem = index_path_file_stem(&index_path);
            let schema_path = format!("{path_base}/{file_stem}.json");
            let object_paths = vec![schema_path.clone()];
            let array_parent = format!("{path_base}.json");
            insert_streamed_schema_record(
                tx,
                &schema_path,
                &schema_json,
                &object_paths,
                compact_output,
                made_dirs,
            )?;
            insert_streamed_array_index_refs_for_item_schema(
                tx,
                &array_parent,
                &schema_path,
                role_name,
                &path_base,
                &canonical,
            )?;
            insert_item_schema_path(tx, role_name, &path_base, &canonical, &schema_path)?;
        }
    }

    let _ = outdir;
    Ok(())
}

fn insert_streamed_array_index_refs_for_item_schema(
    tx: &Transaction<'_>,
    array_path: &str,
    schema_path: &str,
    role_name: &str,
    path_base: &str,
    canonical: &str,
) -> Result<()> {
    let stage_started = Instant::now();
    let inserted = tx.execute(
        "INSERT OR REPLACE INTO array_index_refs(array_path, array_index_path, schema_path)
         SELECT ?1, array_index_path, ?2
         FROM stream_item_schema_index_refs
         WHERE role = ?3 AND path_base = ?4 AND canonical_json = ?5",
        params![array_path, schema_path, role_name, path_base, canonical],
    )?;
    perf_trace_detail(
        "finalize.insert_array_index_refs_for_schema",
        stage_started,
        format_args!(
            "rows={}	role={}	path_base={}	schema_path={}",
            inserted, role_name, path_base, schema_path
        ),
    );
    Ok(())
}

fn insert_item_schema_path(
    tx: &Transaction<'_>,
    role_name: &str,
    path_base: &str,
    canonical: &str,
    schema_path: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO stream_item_schema_paths(role, path_base, canonical_json, schema_path) VALUES (?1, ?2, ?3, ?4)",
        params![role_name, path_base, canonical, schema_path],
    )?;
    Ok(())
}

fn insert_streamed_schema_record(
    tx: &Transaction<'_>,
    schema_path: &str,
    schema_json: &str,
    object_paths: &[String],
    compact_output: bool,
    made_dirs: &mut HashMap<PathBuf, ()>,
) -> Result<()> {
    let schema_value = serde_json::from_str::<Value>(schema_json)
        .with_context(|| format!("invalid schema JSON for {schema_path}"))?;
    let schema_kind = schema_kind(&schema_value);
    let canonical = PathBuf::from(schema_path);
    ensure_parent(&canonical, made_dirs)?;
    let file_json = if compact_output {
        schema_json.to_string()
    } else {
        serde_json::to_string_pretty(&schema_value)?
    };
    fs::write(&canonical, format!("{file_json}\n"))
        .with_context(|| format!("failed to write {}", canonical.display()))?;

    tx.execute(
        "INSERT INTO schema_definitions(schema_path, schema_kind, schema_json) VALUES (?1, ?2, ?3) \
         ON CONFLICT(schema_path) DO UPDATE SET schema_kind=excluded.schema_kind, schema_json=excluded.schema_json",
        params![schema_path, schema_kind, schema_json],
    )?;
    insert_schema_field_types(tx, schema_path, &schema_value)?;

    for object_path in object_paths {
        tx.execute(
            "INSERT INTO schema_paths(schema_path, object_path) VALUES (?1, ?2) \
             ON CONFLICT(object_path) DO UPDATE SET schema_path=excluded.schema_path",
            params![schema_path, object_path],
        )?;

        if object_path != schema_path {
            let object_path = PathBuf::from(object_path);
            ensure_parent(&object_path, made_dirs)?;
            replace_with_symlink(&canonical, &object_path)?;
        }
    }

    Ok(())
}

fn insert_schema_field_types(
    tx: &Transaction<'_>,
    schema_path: &str,
    schema: &Value,
) -> Result<()> {
    let Some(map) = schema.as_object() else {
        return Ok(());
    };
    for (field_name, field_type) in map {
        let Some(field_type) = field_type.as_str() else {
            continue;
        };
        tx.execute(
            "INSERT INTO stream_schema_field_types(schema_path, field_name, field_type) VALUES (?1, ?2, ?3) \
             ON CONFLICT(schema_path, field_name) DO UPDATE SET field_type = excluded.field_type",
            params![schema_path, field_name, field_type],
        )?;
    }
    Ok(())
}

fn write_streamed_occurrence_statistics(tx: &Transaction<'_>) -> Result<()> {
    tx.execute(
        "INSERT INTO schema_object_counts(schema_path, object_count) \
         SELECT sp.schema_path, SUM(t.object_count) \
         FROM stream_object_totals AS t \
         JOIN schema_paths AS sp ON sp.object_path = t.path_base || '.json' \
         JOIN schema_definitions AS d ON d.schema_path = sp.schema_path \
         WHERE d.schema_kind != 'heterogeneous' \
         GROUP BY sp.schema_path \
         ON CONFLICT(schema_path) DO UPDATE SET object_count = schema_object_counts.object_count + excluded.object_count",
        [],
    )?;

    tx.execute(
        "INSERT INTO schema_field_counts(schema_path, field_name, field_type, field_count) \
         SELECT sp.schema_path, f.field_name, ft.field_type, SUM(f.present_count) \
         FROM stream_object_field_facts AS f \
         JOIN schema_paths AS sp ON sp.object_path = f.path_base || '.json' \
         JOIN schema_definitions AS d ON d.schema_path = sp.schema_path \
         JOIN stream_schema_field_types AS ft ON ft.schema_path = sp.schema_path AND ft.field_name = f.field_name \
         WHERE d.schema_kind != 'heterogeneous' \
         GROUP BY sp.schema_path, f.field_name, ft.field_type \
         ON CONFLICT(schema_path, field_name) DO UPDATE SET \
         field_type = excluded.field_type, \
         field_count = schema_field_counts.field_count + excluded.field_count",
        [],
    )?;

    tx.execute(
        "INSERT INTO schema_object_counts(schema_path, object_count) \
         SELECT m.schema_path, SUM(c.object_count) \
         FROM stream_item_schema_counts AS c \
         JOIN stream_item_schema_paths AS m \
           ON m.role = c.role AND m.path_base = c.path_base AND m.canonical_json = c.canonical_json \
         JOIN schema_definitions AS d ON d.schema_path = m.schema_path \
         WHERE d.schema_kind != 'heterogeneous' \
         GROUP BY m.schema_path \
         ON CONFLICT(schema_path) DO UPDATE SET object_count = schema_object_counts.object_count + excluded.object_count",
        [],
    )?;

    tx.execute(
        "INSERT INTO schema_field_counts(schema_path, field_name, field_type, field_count) \
         SELECT m.schema_path, f.field_name, ft.field_type, SUM(f.field_count) \
         FROM stream_item_schema_field_counts AS f \
         JOIN stream_item_schema_paths AS m \
           ON m.role = f.role AND m.path_base = f.path_base AND m.canonical_json = f.canonical_json \
         JOIN schema_definitions AS d ON d.schema_path = m.schema_path \
         JOIN stream_schema_field_types AS ft ON ft.schema_path = m.schema_path AND ft.field_name = f.field_name \
         WHERE d.schema_kind != 'heterogeneous' \
         GROUP BY m.schema_path, f.field_name, ft.field_type \
         ON CONFLICT(schema_path, field_name) DO UPDATE SET \
         field_type = excluded.field_type, \
         field_count = schema_field_counts.field_count + excluded.field_count",
        [],
    )?;

    Ok(())
}

fn write_schema_relations_from_sqlite(tx: &Transaction<'_>) -> Result<()> {
    if perf_trace_enabled() {
        let started = Instant::now();
        perf_trace_detail(
            "finalize.write_schema_relations.input_counts",
            started,
            format_args!(
                "schema_definitions={}	array_index_refs={}	schema_object_counts={}	schema_field_counts={}",
                sqlite_table_count(tx, "schema_definitions")?,
                sqlite_table_count(tx, "array_index_refs")?,
                sqlite_table_count(tx, "schema_object_counts")?,
                sqlite_table_count(tx, "schema_field_counts")?,
            ),
        );
    }

    let array_parent_sql = r#"
SELECT schema_path, MIN(array_path) AS array_parent
FROM array_index_refs
GROUP BY schema_path
"#;
    perf_trace_query_plan(
        tx,
        "finalize.write_schema_relations.load_array_parents",
        array_parent_sql,
    )?;

    let parent_started = Instant::now();
    let mut array_parents = HashMap::<String, String>::new();
    {
        let mut stmt = tx
            .prepare(array_parent_sql)
            .context("failed to prepare schema relation array parent query")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let schema_path: String = row.get(0)?;
            let array_parent: Option<String> = row.get(1)?;
            if let Some(array_parent) = array_parent {
                array_parents.insert(schema_path, array_parent);
            }
        }
    }
    perf_trace_detail(
        "finalize.write_schema_relations.load_array_parents",
        parent_started,
        format_args!("parents={}", array_parents.len()),
    );

    let schema_definitions_sql = r#"
SELECT schema_path, schema_json
FROM schema_definitions
ORDER BY schema_path
"#;
    perf_trace_query_plan(
        tx,
        "finalize.write_schema_relations.load_schema_definitions",
        schema_definitions_sql,
    )?;

    let records_started = Instant::now();
    let mut records = Vec::new();
    let mut records_with_array_parent = 0usize;
    let mut parse_schema_json_ms = 0u128;
    {
        let mut stmt = tx
            .prepare(schema_definitions_sql)
            .context("failed to prepare schema relation schema definition query")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let schema_path: String = row.get(0)?;
            let schema_json: String = row.get(1)?;
            let parse_started = Instant::now();
            let schema = serde_json::from_str(&schema_json).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?;
            parse_schema_json_ms += parse_started.elapsed().as_millis();

            let array_parent = array_parents.get(&schema_path).cloned();
            if array_parent.is_some() {
                records_with_array_parent += 1;
            }

            records.push(Record {
                schema_path,
                object_paths: Vec::new(),
                schema,
                array_parent,
                array_index_paths: Vec::new(),
            });
        }
    }
    perf_trace_detail(
        "finalize.write_schema_relations.load_schema_definitions",
        records_started,
        format_args!(
            "records={}	records_with_array_parent={}	parse_schema_json_ms={}",
            records.len(),
            records_with_array_parent,
            parse_schema_json_ms,
        ),
    );

    let build_started = Instant::now();
    write_schema_relations(tx, &records)?;
    perf_trace_detail(
        "finalize.write_schema_relations.build",
        build_started,
        format_args!(
            "records={}	schema_relations={}",
            records.len(),
            sqlite_table_count(tx, "schema_relations")?,
        ),
    );
    Ok(())
}

fn build_schema_output(outdir: &Path, entries: Vec<Entry>) -> Result<SchemaBuild> {
    let mut occurrences = Vec::new();
    for entry in entries {
        let role = if entry.collection {
            Role::RootCollection
        } else {
            Role::Object
        };
        collect_object_occurrences(
            entry.path,
            entry.value,
            role,
            entry.index.into_iter().collect(),
            &mut occurrences,
        );
    }

    let mut records = Vec::new();
    records.extend(object_records(
        outdir,
        occurrences
            .iter()
            .filter(|o| o.role == Role::Object)
            .cloned()
            .collect(),
    )?);
    records.extend(array_records(
        outdir,
        occurrences
            .iter()
            .filter(|o| o.role == Role::ArrayItem)
            .cloned()
            .collect(),
    )?);
    records.extend(root_collection_records(
        outdir,
        occurrences
            .iter()
            .filter(|o| o.role == Role::RootCollection)
            .cloned()
            .collect(),
    )?);
    Ok(SchemaBuild {
        records,
        occurrences,
    })
}

#[cfg(test)]
fn distinct_schemas(outdir: &Path, entries: Vec<Entry>) -> Result<Vec<Record>> {
    Ok(build_schema_output(outdir, entries)?.records)
}

fn collect_object_occurrences(
    segments: Vec<String>,
    value: Map<String, Value>,
    role: Role,
    array_indexes: Vec<usize>,
    out: &mut Vec<Occurrence>,
) {
    out.push(Occurrence {
        segments: segments.clone(),
        value: value.clone(),
        role,
        array_indexes: array_indexes.clone(),
    });

    for (key, child) in value {
        let mut child_segments = segments.clone();
        child_segments.push(key);
        match child {
            Value::Object(map) => {
                collect_object_occurrences(
                    child_segments,
                    map,
                    Role::Object,
                    array_indexes.clone(),
                    out,
                );
            }
            Value::Array(items) => {
                collect_array_object_occurrences(child_segments, items, array_indexes.clone(), out);
            }
            _ => {}
        }
    }
}

fn collect_array_object_occurrences(
    segments: Vec<String>,
    items: Vec<Value>,
    array_indexes: Vec<usize>,
    out: &mut Vec<Occurrence>,
) {
    for (index, item) in items.into_iter().enumerate() {
        let mut item_indexes = array_indexes.clone();
        item_indexes.push(index);
        match item {
            Value::Object(map) => collect_object_occurrences(
                segments.clone(),
                map,
                Role::ArrayItem,
                item_indexes,
                out,
            ),
            Value::Array(nested) => {
                collect_array_object_occurrences(segments.clone(), nested, item_indexes, out);
            }
            _ => {}
        }
    }
}

fn object_records(outdir: &Path, occurrences: Vec<Occurrence>) -> Result<Vec<Record>> {
    let mut by_segments: BTreeMap<Vec<String>, Vec<Map<String, Value>>> = BTreeMap::new();
    for occ in occurrences {
        by_segments.entry(occ.segments).or_default().push(occ.value);
    }

    let mut by_canonical: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
    for (segments, objects) in by_segments {
        let schema_path = format!("{}.json", reference_path(outdir, &segments));
        let schema = schema_for_values(outdir, &segments, &objects)?;
        let canonical = canonical_json(&schema)?;
        by_canonical
            .entry(canonical)
            .or_default()
            .push((schema_path, schema));
    }

    let mut records = Vec::new();
    for (_canonical, mut same) in by_canonical {
        same.sort_by(|a, b| a.0.cmp(&b.0));
        let object_paths = same.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
        records.push(Record {
            schema_path: object_paths[0].clone(),
            object_paths,
            schema: same[0].1.clone(),
            array_parent: None,
            array_index_paths: Vec::new(),
        });
    }
    Ok(records)
}

fn array_records(outdir: &Path, occurrences: Vec<Occurrence>) -> Result<Vec<Record>> {
    let mut by_segments: BTreeMap<Vec<String>, Vec<Occurrence>> = BTreeMap::new();
    for occ in occurrences {
        by_segments
            .entry(occ.segments.clone())
            .or_default()
            .push(occ);
    }

    let mut records = Vec::new();
    for (segments, items) in by_segments {
        let array_base = reference_path(outdir, &segments);
        let mut groups: BTreeMap<String, Vec<(Vec<usize>, Value)>> = BTreeMap::new();
        for item in items {
            if item.array_indexes.is_empty() {
                bail!("array item missing index path");
            }
            let schema = schema_for_values(outdir, &segments, &[item.value])?;
            groups
                .entry(canonical_json(&schema)?)
                .or_default()
                .push((item.array_indexes, schema));
        }

        if groups.len() == 1 {
            let (_, same) = groups.into_iter().next().unwrap();
            let mut array_index_paths = same
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            array_index_paths.sort_unstable();
            array_index_paths.dedup();
            records.push(Record {
                schema_path: format!("{array_base}.json"),
                object_paths: vec![format!("{array_base}.json")],
                schema: same[0].1.clone(),
                array_parent: Some(format!("{array_base}.json")),
                array_index_paths,
            });
        } else {
            records.push(Record {
                schema_path: format!("{array_base}.json"),
                object_paths: vec![format!("{array_base}.json")],
                schema: json!({"$refs_mut": format!("{array_base}/")}),
                array_parent: None,
                array_index_paths: Vec::new(),
            });

            let mut distinct = groups
                .into_values()
                .map(|same| {
                    let first_index_path = same.iter().map(|(path, _)| path.clone()).min().unwrap();
                    (first_index_path, same)
                })
                .collect::<Vec<_>>();
            distinct.sort_by_key(|(first_index_path, _)| first_index_path.clone());

            for (first_index_path, same) in distinct {
                let file_stem = index_path_file_stem(&first_index_path);
                let mut array_index_paths = same
                    .iter()
                    .map(|(path, _)| path.clone())
                    .collect::<Vec<_>>();
                array_index_paths.sort_unstable();
                array_index_paths.dedup();
                records.push(Record {
                    schema_path: format!("{array_base}/{file_stem}.json"),
                    object_paths: vec![format!("{array_base}/{file_stem}.json")],
                    schema: same[0].1.clone(),
                    array_parent: Some(format!("{array_base}.json")),
                    array_index_paths,
                });
            }
        }
    }
    Ok(records)
}

fn index_path_file_stem(index_path: &[usize]) -> String {
    index_path
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join("_")
}

fn root_collection_records(outdir: &Path, occurrences: Vec<Occurrence>) -> Result<Vec<Record>> {
    let mut by_segments: BTreeMap<Vec<String>, Vec<Occurrence>> = BTreeMap::new();
    for occ in occurrences {
        by_segments
            .entry(occ.segments.clone())
            .or_default()
            .push(occ);
    }

    let mut records = Vec::new();
    for (segments, items) in by_segments {
        let collection_base = reference_path(outdir, &segments);
        let mut groups: BTreeMap<String, Vec<(Vec<usize>, Value)>> = BTreeMap::new();
        for item in items {
            if item.array_indexes.is_empty() {
                bail!("root collection item missing index path");
            }
            let schema = schema_for_values(outdir, &segments, &[item.value])?;
            groups
                .entry(canonical_json(&schema)?)
                .or_default()
                .push((item.array_indexes, schema));
        }

        if groups.len() == 1 {
            let (_, same) = groups.into_iter().next().unwrap();
            let mut array_index_paths = same
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            array_index_paths.sort_unstable();
            array_index_paths.dedup();
            records.push(Record {
                schema_path: format!("{collection_base}.json"),
                object_paths: vec![format!("{collection_base}.json")],
                schema: same[0].1.clone(),
                array_parent: Some(format!("{collection_base}.json")),
                array_index_paths,
            });
        } else {
            let mut distinct = groups
                .into_values()
                .map(|same| {
                    let first_index_path = same.iter().map(|(path, _)| path.clone()).min().unwrap();
                    (first_index_path, same)
                })
                .collect::<Vec<_>>();
            distinct.sort_by_key(|(first_index_path, _)| first_index_path.clone());

            for (first_index_path, same) in distinct {
                let file_stem = index_path_file_stem(&first_index_path);
                let mut array_index_paths = same
                    .iter()
                    .map(|(path, _)| path.clone())
                    .collect::<Vec<_>>();
                array_index_paths.sort_unstable();
                array_index_paths.dedup();
                records.push(Record {
                    schema_path: format!("{collection_base}/{file_stem}.json"),
                    object_paths: vec![format!("{collection_base}/{file_stem}.json")],
                    schema: same[0].1.clone(),
                    array_parent: Some(format!("{collection_base}.json")),
                    array_index_paths,
                });
            }
        }
    }
    Ok(records)
}

fn schema_for_values(
    outdir: &Path,
    segments: &[String],
    objects: &[Map<String, Value>],
) -> Result<Value> {
    let total = objects.len();
    let mut keys = BTreeSet::new();
    for object in objects {
        keys.extend(object.keys().cloned());
    }

    let mut schema = Map::new();
    for key in keys {
        let values = objects
            .iter()
            .filter_map(|object| object.get(&key))
            .collect::<Vec<_>>();
        let types = unique_types(values.iter().copied());
        let mut child_segments = segments.to_vec();
        child_segments.push(key.clone());

        let label = if types.contains(&"array") {
            let mut labels = BTreeSet::new();
            labels.insert(array_label(outdir, &child_segments, &values)?);
            for value_type in types {
                if !matches!(value_type, "array" | "null") {
                    labels.insert(value_type.to_string());
                }
            }
            union_member_labels(labels)
        } else if types.contains(&"object") {
            if types.iter().all(|t| matches!(*t, "object" | "null")) {
                format!("{}.json", reference_path(outdir, &child_segments))
            } else {
                types
                    .iter()
                    .filter(|t| **t != "null")
                    .copied()
                    .collect::<Vec<_>>()
                    .join("|")
            }
        } else {
            primitive_label(&values, total)
        };
        schema.insert(key, Value::String(label));
    }
    Ok(Value::Object(schema))
}

fn unique_types<'a>(values: impl IntoIterator<Item = &'a Value>) -> Vec<&'static str> {
    let mut out = BTreeSet::new();
    for value in values {
        out.insert(value_type(value));
    }
    out.into_iter().collect()
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn primitive_label(values: &[&Value], total: usize) -> String {
    let present = values.len();
    let types = unique_types(values.iter().copied());
    let mut result = if types == vec!["null"] {
        "null".to_string()
    } else if types.len() == 1 {
        types[0].to_string()
    } else if types.len() == 2 && types[0] == "null" {
        format!("{}?", types[1])
    } else {
        types.join("|")
    };

    if present < total && result != "null" && !result.ends_with('?') {
        result.push('?');
    }
    result
}

fn array_label(outdir: &Path, segments: &[String], arrays: &[&Value]) -> Result<String> {
    let members = arrays
        .iter()
        .filter_map(|value| value.as_array())
        .flat_map(|items| items.iter())
        .collect::<Vec<_>>();
    Ok(format!(
        "array({})",
        array_member_label(outdir, segments, &members)?
    ))
}

fn array_member_label(outdir: &Path, segments: &[String], members: &[&Value]) -> Result<String> {
    let types = unique_types(members.iter().copied());
    if members.is_empty() {
        return Ok("empty".to_string());
    }

    let mut labels = BTreeSet::new();
    for primitive in ["boolean", "number", "string", "null"] {
        if types.contains(&primitive) {
            labels.insert(primitive.to_string());
        }
    }
    if types.contains(&"object") {
        labels.insert(format!("{}.json", reference_path(outdir, segments)));
    }
    if types.contains(&"array") {
        let nested = members
            .iter()
            .filter_map(|value| value.as_array())
            .flat_map(|items| items.iter())
            .collect::<Vec<_>>();
        labels.insert(format!(
            "array({})",
            array_member_label(outdir, segments, &nested)?
        ));
    }
    Ok(union_member_labels(labels))
}

fn union_member_labels(mut labels: BTreeSet<String>) -> String {
    if labels.len() == 2 && labels.contains("null") {
        labels.remove("null");
        return format!("{}?", labels.into_iter().next().unwrap());
    }
    labels.into_iter().collect::<Vec<_>>().join("|")
}

fn path_segment(segment: &str) -> String {
    segment.replace('%', "%25").replace('/', "%2F")
}

fn reference_path(outdir: &Path, segments: &[String]) -> String {
    let joined = segments
        .iter()
        .map(|s| path_segment(s))
        .collect::<Vec<_>>()
        .join("/");
    format!("{}/{}", outdir.to_string_lossy(), joined)
}

fn canonical_json(value: &Value) -> Result<String> {
    match value {
        Value::Object(map) => {
            let sorted = map
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<_, _>>();
            serde_json::to_string(&sorted).context("failed to serialize canonical schema")
        }
        _ => serde_json::to_string(value).context("failed to serialize canonical schema"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct SchemaCount {
    schema_path: String,
    schema_kind: String,
    object_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct SchemaObjectPath {
    schema_path: String,
    object_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct SchemaArrayIndexRef {
    schema_path: String,
    array_path: String,
    array_index_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct FieldCount {
    schema_path: String,
    field_name: String,
    field_type: String,
    field_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct ReportData {
    schemas: Vec<SchemaCount>,
    object_paths: Vec<SchemaObjectPath>,
    array_index_refs: Vec<SchemaArrayIndexRef>,
    fields: Vec<FieldCount>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReportSummary {
    Generated {
        execution_time_ms: u128,
        input_source: String,
    },
    FromSqlite {
        sqlite_path: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportDetail {
    CompactStdout,
    FullFile,
}

#[cfg(test)]
fn load_report_from_sqlite(database: &Path) -> Result<ReportData> {
    if !database.exists() {
        bail!(
            "SQLite report source does not exist: {}",
            database.display()
        );
    }

    let conn = Connection::open(database).with_context(|| {
        format!(
            "failed to open SQLite report source: {}",
            database.display()
        )
    })?;
    read_report_data(&conn)
}

#[cfg(test)]
fn read_report_data(conn: &Connection) -> Result<ReportData> {
    ensure_field_type_column_exists(conn)?;

    let schema_order_sql = "SELECT paths.schema_path, defs.schema_kind, COALESCE(counts.object_count, 0) AS object_count \
         FROM (SELECT DISTINCT schema_path FROM schema_paths) AS paths \
         JOIN schema_definitions AS defs ON defs.schema_path = paths.schema_path \
         LEFT JOIN schema_object_counts AS counts ON counts.schema_path = paths.schema_path";

    let mut schema_stmt = conn.prepare(&format!(
        "{schema_order_sql} \
         ORDER BY object_count DESC, paths.schema_path"
    ))?;

    let schemas = schema_stmt
        .query_map([], |row| {
            Ok(SchemaCount {
                schema_path: row.get(0)?,
                schema_kind: row.get(1)?,
                object_count: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut object_path_stmt = conn.prepare(&format!(
        "SELECT p.schema_path, p.object_path \
         FROM schema_paths AS p \
         JOIN ({schema_order_sql}) AS schema_order ON schema_order.schema_path = p.schema_path \
         ORDER BY schema_order.object_count DESC, p.schema_path, p.object_path"
    ))?;

    let object_paths = object_path_stmt
        .query_map([], |row| {
            Ok(SchemaObjectPath {
                schema_path: row.get(0)?,
                object_path: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut array_index_stmt = conn.prepare(&format!(
        "SELECT r.schema_path, r.array_path, r.array_index_path \
         FROM array_index_refs AS r \
         JOIN ({schema_order_sql}) AS schema_order ON schema_order.schema_path = r.schema_path \
         ORDER BY schema_order.object_count DESC, r.schema_path, r.array_path, r.array_index_path"
    ))?;

    let array_index_refs = array_index_stmt
        .query_map([], |row| {
            Ok(SchemaArrayIndexRef {
                schema_path: row.get(0)?,
                array_path: row.get(1)?,
                array_index_path: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut field_stmt = conn.prepare(&format!(
        "SELECT f.schema_path, f.field_name, f.field_type, f.field_count \
         FROM schema_field_counts AS f \
         JOIN ({schema_order_sql}) AS schema_order ON schema_order.schema_path = f.schema_path \
         ORDER BY schema_order.object_count DESC, f.schema_path, f.field_count DESC, f.field_name"
    ))?;

    let fields = field_stmt
        .query_map([], |row| {
            Ok(FieldCount {
                schema_path: row.get(0)?,
                field_name: row.get(1)?,
                field_type: row.get(2)?,
                field_count: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(ReportData {
        schemas,
        object_paths,
        array_index_refs,
        fields,
    })
}

fn ensure_field_type_column_exists(conn: &Connection) -> Result<()> {
    ensure_table_has_column(
        conn,
        "schema_field_counts",
        "field_type",
        "SQLite report source does not contain schema_field_counts.field_type; regenerate refs with the current dump-json-refs version",
    )?;
    ensure_table_has_column(
        conn,
        "schema_definitions",
        "schema_kind",
        "SQLite report source does not contain schema_definitions.schema_kind; regenerate refs with the current dump-json-refs version",
    )?;
    Ok(())
}

fn ensure_table_has_column(
    conn: &Connection,
    table_name: &str,
    column_name: &str,
    error_message: &str,
) -> Result<()> {
    let pragma = format!("PRAGMA table_info({table_name})");
    let mut stmt = conn.prepare(&pragma)?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if columns.iter().any(|column| column == column_name) {
        Ok(())
    } else {
        bail!("{error_message}")
    }
}

fn emit_report_from_sqlite(
    database: &Path,
    summary: &ReportSummary,
    output_path: Option<&Path>,
) -> Result<()> {
    let report_started = Instant::now();
    if let Some(output_path) = output_path {
        if let Some(parent) = output_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file = fs::File::create(output_path)
            .with_context(|| format!("failed to write {}", output_path.display()))?;
        let mut writer = BufWriter::new(file);
        render_report_from_sqlite_to_writer(
            database,
            summary,
            ReportDetail::FullFile,
            None,
            &mut writer,
        )?;
        writer.flush()?;

        let stdout = io::stdout();
        let mut stdout = BufWriter::new(stdout.lock());
        render_summary_from_sqlite_to_writer(database, summary, Some(output_path), &mut stdout)?;
        stdout.flush()?;
    } else {
        let stdout = io::stdout();
        let mut stdout = BufWriter::new(stdout.lock());
        render_report_from_sqlite_to_writer(
            database,
            summary,
            ReportDetail::CompactStdout,
            None,
            &mut stdout,
        )?;
        stdout.flush()?;
    }

    perf_trace("report.emit", report_started);
    Ok(())
}

fn render_report_from_sqlite_to_writer<W: Write>(
    database: &Path,
    summary: &ReportSummary,
    detail: ReportDetail,
    output_path: Option<&Path>,
    writer: &mut W,
) -> Result<()> {
    if !database.exists() {
        bail!(
            "SQLite report source does not exist: {}",
            database.display()
        );
    }

    let stage_started = Instant::now();
    let conn = Connection::open(database).with_context(|| {
        format!(
            "failed to open SQLite report source: {}",
            database.display()
        )
    })?;
    ensure_field_type_column_exists(&conn)?;
    perf_trace("report.open_sqlite", stage_started);

    writeln!(writer, "# dump-json-refs report")?;
    writeln!(writer)?;

    let stage_started = Instant::now();
    write_schemas_section_from_sqlite(&conn, writer)?;
    perf_trace("report.write_schemas", stage_started);

    match detail {
        ReportDetail::CompactStdout => {
            let stage_started = Instant::now();
            write_fields_section_from_sqlite(&conn, writer)?;
            perf_trace("report.write_fields", stage_started);

            let stage_started = Instant::now();
            write_schema_aliases_section_from_sqlite(&conn, writer)?;
            perf_trace("report.write_schema_aliases", stage_started);
        }
        ReportDetail::FullFile => {
            let stage_started = Instant::now();
            write_schema_object_paths_section_from_sqlite(&conn, writer)?;
            perf_trace("report.write_schema_object_paths", stage_started);

            let stage_started = Instant::now();
            write_schema_array_index_refs_section_from_sqlite(&conn, writer)?;
            perf_trace("report.write_schema_array_index_refs", stage_started);

            let stage_started = Instant::now();
            write_fields_section_from_sqlite(&conn, writer)?;
            perf_trace("report.write_fields", stage_started);
        }
    }

    let stage_started = Instant::now();
    render_summary_from_conn_to_writer(&conn, summary, output_path, writer)?;
    perf_trace("report.write_summary", stage_started);
    Ok(())
}

fn schema_order_sql() -> &'static str {
    "SELECT paths.schema_path, defs.schema_kind, COALESCE(counts.object_count, 0) AS object_count \
     FROM (SELECT DISTINCT schema_path FROM schema_paths) AS paths \
     JOIN schema_definitions AS defs ON defs.schema_path = paths.schema_path \
     LEFT JOIN schema_object_counts AS counts ON counts.schema_path = paths.schema_path"
}

fn write_schemas_section_from_sqlite<W: Write>(conn: &Connection, writer: &mut W) -> Result<()> {
    writeln!(writer, "[schemas]")?;
    writeln!(writer, "schema_path\tschema_kind\tobject_count")?;
    let mut stmt = conn.prepare(&format!(
        "{} ORDER BY object_count DESC, paths.schema_path",
        schema_order_sql()
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let schema_path: String = row.get(0)?;
        let schema_kind: String = row.get(1)?;
        let object_count: i64 = row.get(2)?;
        writeln!(writer, "{schema_path}\t{schema_kind}\t{object_count}")?;
    }
    writeln!(writer)?;
    Ok(())
}

fn write_schema_object_paths_section_from_sqlite<W: Write>(
    conn: &Connection,
    writer: &mut W,
) -> Result<()> {
    writeln!(writer, "[schema_object_paths]")?;
    writeln!(writer, "schema_path\tobject_path")?;
    let mut stmt = conn.prepare(&format!(
        "SELECT p.schema_path, p.object_path \
         FROM schema_paths AS p \
         JOIN ({}) AS schema_order ON schema_order.schema_path = p.schema_path \
         ORDER BY schema_order.object_count DESC, p.schema_path, p.object_path",
        schema_order_sql()
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let schema_path: String = row.get(0)?;
        let object_path: String = row.get(1)?;
        writeln!(writer, "{schema_path}\t{object_path}")?;
    }
    writeln!(writer)?;
    Ok(())
}

fn write_schema_aliases_section_from_sqlite<W: Write>(
    conn: &Connection,
    writer: &mut W,
) -> Result<()> {
    let mut stmt = conn.prepare(&format!(
        "SELECT p.schema_path, p.object_path \
         FROM schema_paths AS p \
         JOIN ({}) AS schema_order ON schema_order.schema_path = p.schema_path \
         WHERE p.schema_path != p.object_path \
         ORDER BY schema_order.object_count DESC, p.schema_path, p.object_path",
        schema_order_sql()
    ))?;
    let mut rows = stmt.query([])?;
    let mut wrote_header = false;
    while let Some(row) = rows.next()? {
        if !wrote_header {
            writeln!(writer, "[schema_aliases]")?;
            writeln!(writer, "schema_path\tobject_path")?;
            wrote_header = true;
        }
        let schema_path: String = row.get(0)?;
        let object_path: String = row.get(1)?;
        writeln!(writer, "{schema_path}\t{object_path}")?;
    }
    if wrote_header {
        writeln!(writer)?;
    }
    Ok(())
}

fn write_schema_array_index_refs_section_from_sqlite<W: Write>(
    conn: &Connection,
    writer: &mut W,
) -> Result<()> {
    writeln!(writer, "[schema_array_index_refs]")?;
    writeln!(writer, "schema_path	array_path	array_index_path")?;
    let query_started = Instant::now();
    let mut stmt = conn.prepare(&format!(
        "SELECT r.schema_path, r.array_path, r.array_index_path
         FROM array_index_refs AS r
         JOIN ({}) AS schema_order ON schema_order.schema_path = r.schema_path
         ORDER BY schema_order.object_count DESC, r.schema_path, r.array_path, r.array_index_path",
        schema_order_sql()
    ))?;
    let mut rows = stmt.query([])?;
    let mut row_count = 0usize;
    while let Some(row) = rows.next()? {
        let schema_path: String = row.get(0)?;
        let array_path: String = row.get(1)?;
        let array_index_path: String = row.get(2)?;
        writeln!(writer, "{schema_path}	{array_path}	{array_index_path}")?;
        row_count += 1;
    }
    writeln!(writer)?;
    perf_trace_detail(
        "report.array_index_refs.query_and_write_rows",
        query_started,
        format_args!("rows={row_count}"),
    );
    Ok(())
}

fn write_fields_section_from_sqlite<W: Write>(conn: &Connection, writer: &mut W) -> Result<()> {
    writeln!(writer, "[fields]")?;
    writeln!(writer, "schema_path\tfield_name\tfield_type\tfield_count")?;
    let mut stmt = conn.prepare(&format!(
        "SELECT f.schema_path, f.field_name, f.field_type, f.field_count \
         FROM schema_field_counts AS f \
         JOIN ({}) AS schema_order ON schema_order.schema_path = f.schema_path \
         ORDER BY schema_order.object_count DESC, f.schema_path, f.field_count DESC, f.field_name",
        schema_order_sql()
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let schema_path: String = row.get(0)?;
        let field_name: String = row.get(1)?;
        let field_type: String = row.get(2)?;
        let field_count: i64 = row.get(3)?;
        writeln!(
            writer,
            "{schema_path}\t{field_name}\t{field_type}\t{field_count}"
        )?;
    }
    writeln!(writer)?;
    Ok(())
}

fn render_summary_from_sqlite_to_writer<W: Write>(
    database: &Path,
    summary: &ReportSummary,
    output_path: Option<&Path>,
    writer: &mut W,
) -> Result<()> {
    if !database.exists() {
        bail!(
            "SQLite report source does not exist: {}",
            database.display()
        );
    }
    let conn = Connection::open(database).with_context(|| {
        format!(
            "failed to open SQLite report source: {}",
            database.display()
        )
    })?;
    ensure_field_type_column_exists(&conn)?;
    render_summary_from_conn_to_writer(&conn, summary, output_path, writer)
}

fn render_summary_from_conn_to_writer<W: Write>(
    conn: &Connection,
    summary: &ReportSummary,
    output_path: Option<&Path>,
    writer: &mut W,
) -> Result<()> {
    let schema_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM (SELECT DISTINCT p.schema_path FROM schema_paths AS p JOIN schema_definitions AS d ON d.schema_path = p.schema_path)",
        [],
        |row| row.get(0),
    )?;
    let field_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM schema_field_counts", [], |row| {
            row.get(0)
        })?;
    let object_path_mapping_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM schema_paths", [], |row| row.get(0))?;
    let schema_alias_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM schema_paths WHERE schema_path != object_path",
        [],
        |row| row.get(0),
    )?;
    let array_index_ref_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM array_index_refs", [], |row| {
            row.get(0)
        })?;

    writeln!(writer, "[summary]")?;
    writeln!(writer, "schema_count\t{schema_count}")?;
    writeln!(writer, "field_count\t{field_count}")?;
    writeln!(
        writer,
        "object_path_mapping_count\t{object_path_mapping_count}"
    )?;
    writeln!(writer, "schema_alias_count\t{schema_alias_count}")?;
    writeln!(writer, "array_index_ref_count\t{array_index_ref_count}")?;

    match summary {
        ReportSummary::Generated {
            execution_time_ms,
            input_source,
        } => {
            writeln!(writer, "execution_time_ms\t{execution_time_ms}")?;
            writeln!(writer, "input\t{input_source}")?;
        }
        ReportSummary::FromSqlite { sqlite_path } => {
            writeln!(writer, "sqlite_path\t{sqlite_path}")?;
        }
    }

    if let Some(output_path) = output_path {
        writeln!(writer, "report\t{}", output_path.display())?;
    }

    Ok(())
}

fn write_text_file(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
fn render_report(report: &ReportData, summary: &ReportSummary, detail: ReportDetail) -> String {
    let mut out = String::new();

    out.push_str("# dump-json-refs report\n\n");
    push_schemas_section(&mut out, report);

    match detail {
        ReportDetail::CompactStdout => {
            push_fields_section(&mut out, report);
            push_schema_aliases_section_if_non_empty(&mut out, report);
        }
        ReportDetail::FullFile => {
            push_schema_object_paths_section(&mut out, report);
            push_schema_array_index_refs_section(&mut out, report);
            push_fields_section(&mut out, report);
        }
    }

    out.push_str(&render_summary(report, summary, None));
    out
}

#[cfg(test)]
fn push_schemas_section(out: &mut String, report: &ReportData) {
    out.push_str("[schemas]\n");
    out.push_str("schema_path\tschema_kind\tobject_count\n");
    for schema in &report.schemas {
        out.push_str(&format!(
            "{}\t{}\t{}\n",
            schema.schema_path, schema.schema_kind, schema.object_count
        ));
    }
    out.push('\n');
}

#[cfg(test)]
fn push_schema_object_paths_section(out: &mut String, report: &ReportData) {
    out.push_str("[schema_object_paths]\n");
    out.push_str("schema_path\tobject_path\n");
    for mapping in &report.object_paths {
        out.push_str(&format!(
            "{}\t{}\n",
            mapping.schema_path, mapping.object_path
        ));
    }
    out.push('\n');
}

#[cfg(test)]
fn push_schema_aliases_section_if_non_empty(out: &mut String, report: &ReportData) {
    let aliases = report
        .object_paths
        .iter()
        .filter(|mapping| mapping.schema_path != mapping.object_path)
        .collect::<Vec<_>>();

    if aliases.is_empty() {
        return;
    }

    out.push_str("[schema_aliases]\n");
    out.push_str("schema_path\tobject_path\n");
    for mapping in aliases {
        out.push_str(&format!(
            "{}\t{}\n",
            mapping.schema_path, mapping.object_path
        ));
    }
    out.push('\n');
}

#[cfg(test)]
fn push_schema_array_index_refs_section(out: &mut String, report: &ReportData) {
    out.push_str("[schema_array_index_refs]\n");
    out.push_str("schema_path\tarray_path\tarray_index_path\n");
    for mapping in &report.array_index_refs {
        out.push_str(&format!(
            "{}\t{}\t{}\n",
            mapping.schema_path, mapping.array_path, mapping.array_index_path
        ));
    }
    out.push('\n');
}

#[cfg(test)]
fn push_fields_section(out: &mut String, report: &ReportData) {
    out.push_str("[fields]\n");
    out.push_str("schema_path\tfield_name\tfield_type\tfield_count\n");
    for field in &report.fields {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            field.schema_path, field.field_name, field.field_type, field.field_count
        ));
    }
    out.push('\n');
}

#[cfg(test)]
fn render_summary(
    report: &ReportData,
    summary: &ReportSummary,
    output_path: Option<&Path>,
) -> String {
    let mut out = String::new();

    out.push_str("[summary]\n");
    out.push_str(&format!("schema_count\t{}\n", report.schemas.len()));
    out.push_str(&format!("field_count\t{}\n", report.fields.len()));
    out.push_str(&format!(
        "object_path_mapping_count\t{}\n",
        report.object_paths.len()
    ));
    out.push_str(&format!(
        "schema_alias_count\t{}\n",
        report
            .object_paths
            .iter()
            .filter(|mapping| mapping.schema_path != mapping.object_path)
            .count()
    ));
    out.push_str(&format!(
        "array_index_ref_count\t{}\n",
        report.array_index_refs.len()
    ));

    match summary {
        ReportSummary::Generated {
            execution_time_ms,
            input_source,
        } => {
            out.push_str(&format!("execution_time_ms\t{execution_time_ms}\n"));
            out.push_str(&format!("input\t{input_source}\n"));
        }
        ReportSummary::FromSqlite { sqlite_path } => {
            out.push_str(&format!("sqlite_path\t{sqlite_path}\n"));
        }
    }

    if let Some(output_path) = output_path {
        out.push_str(&format!("report\t{}\n", output_path.display()));
    }

    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DotNode {
    schema_path: String,
    schema_kind: String,
    object_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DotRelation {
    from_schema_path: String,
    to_schema_path: String,
    relation_kind: String,
    fk_owner: String,
    fk_candidate: bool,
    field_name: String,
    cardinality: String,
    required: bool,
    mixed: bool,
    nested_array_depth: usize,
    via_schema_path: Option<String>,
    via_array_path: Option<String>,
    parent_object_count: i64,
    child_object_count: i64,
    field_count: i64,
}

fn resolve_graph_output_path(
    graph_requested: bool,
    graph_output: Option<&Path>,
    outdir: &Path,
    from_sqlite: Option<&Path>,
    graph_format: GraphFormat,
) -> Option<PathBuf> {
    if !graph_requested {
        return None;
    }

    if let Some(graph_output) = graph_output {
        if graph_output != Path::new(DEFAULT_GRAPH_OUTPUT) {
            return Some(graph_output.to_path_buf());
        }
    }

    if let Some(sqlite_path) = from_sqlite {
        return Some(
            sqlite_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(graph_format.default_filename()),
        );
    }

    Some(outdir.join(graph_format.default_filename()))
}

fn write_graph_from_sqlite(
    database: &Path,
    output_path: &Path,
    include_marked: bool,
    rankdir: &str,
    graph_format: GraphFormat,
) -> Result<()> {
    if !database.exists() {
        bail!("SQLite graph source does not exist: {}", database.display());
    }

    let conn = Connection::open(database)
        .with_context(|| format!("failed to open SQLite graph source: {}", database.display()))?;
    ensure_graph_tables_exist(&conn)?;
    let (nodes, relations) = load_graph_data(&conn, include_marked)?;
    let rendered = match graph_format {
        GraphFormat::Mermaid => render_mermaid_graph(&nodes, &relations, rankdir, false),
        GraphFormat::MermaidMd => render_mermaid_graph(&nodes, &relations, rankdir, true),
        GraphFormat::Dot => render_dot_graph(&nodes, &relations, rankdir),
    };
    write_text_file(output_path, &rendered)
}

fn ensure_graph_tables_exist(conn: &Connection) -> Result<()> {
    ensure_table_has_column(
        conn,
        "schema_relations",
        "relation_kind",
        "SQLite graph source does not contain schema_relations; regenerate refs with the current dump-json-refs version",
    )?;
    ensure_table_has_column(
        conn,
        "schema_relations",
        "fk_candidate",
        "SQLite graph source does not contain schema_relations.fk_candidate; regenerate refs with the current dump-json-refs version",
    )?;
    ensure_table_has_column(
        conn,
        "schema_relations",
        "nested_array_depth",
        "SQLite graph source does not contain schema_relations.nested_array_depth; regenerate refs with the current dump-json-refs version",
    )?;
    Ok(())
}

fn load_graph_data(
    conn: &Connection,
    include_marked: bool,
) -> Result<(Vec<DotNode>, Vec<DotRelation>)> {
    let relation_filter = if include_marked {
        ""
    } else {
        "WHERE fk_candidate = 1"
    };
    let mut relation_stmt = conn.prepare(&format!(
        "SELECT from_schema_path, to_schema_path, relation_kind, fk_owner, fk_candidate, \
                field_name, cardinality, required, mixed, nested_array_depth, \
                via_schema_path, via_array_path, parent_object_count, child_object_count, field_count \
         FROM schema_relations \
         {relation_filter} \
         ORDER BY fk_candidate DESC, relation_kind, from_schema_path, to_schema_path, field_name"
    ))?;

    let relations = relation_stmt
        .query_map([], |row| {
            Ok(DotRelation {
                from_schema_path: row.get(0)?,
                to_schema_path: row.get(1)?,
                relation_kind: row.get(2)?,
                fk_owner: row.get(3)?,
                fk_candidate: row.get::<_, i64>(4)? == 1,
                field_name: row.get(5)?,
                cardinality: row.get(6)?,
                required: row.get::<_, i64>(7)? == 1,
                mixed: row.get::<_, i64>(8)? == 1,
                nested_array_depth: row.get::<_, i64>(9)? as usize,
                via_schema_path: row.get(10)?,
                via_array_path: row.get(11)?,
                parent_object_count: row.get(12)?,
                child_object_count: row.get(13)?,
                field_count: row.get(14)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut used_paths = BTreeSet::<String>::new();
    for relation in &relations {
        used_paths.insert(relation.from_schema_path.clone());
        used_paths.insert(relation.to_schema_path.clone());
    }

    let mut node_stmt = conn.prepare(
        "SELECT d.schema_path, d.schema_kind, COALESCE(c.object_count, 0) AS object_count \
         FROM schema_definitions AS d \
         LEFT JOIN schema_object_counts AS c ON c.schema_path = d.schema_path \
         ORDER BY object_count DESC, d.schema_path",
    )?;
    let nodes = node_stmt
        .query_map([], |row| {
            Ok(DotNode {
                schema_path: row.get(0)?,
                schema_kind: row.get(1)?,
                object_count: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|node| used_paths.contains(&node.schema_path))
        .collect::<Vec<_>>();

    Ok((nodes, relations))
}

fn render_mermaid_graph(
    nodes: &[DotNode],
    relations: &[DotRelation],
    rankdir: &str,
    markdown_fence: bool,
) -> String {
    let mut out = String::new();
    if markdown_fence {
        out.push_str("```mermaid\n");
    }
    out.push_str(&format!("flowchart {}\n", mermaid_direction(rankdir)));

    let mut node_ids = HashMap::<String, String>::new();
    for (index, node) in nodes.iter().enumerate() {
        let id = format!("n{index}");
        node_ids.insert(node.schema_path.clone(), id.clone());
        let label = format!(
            "{}<br/>kind={}<br/>object_count={}",
            node.schema_path, node.schema_kind, node.object_count
        );
        out.push_str(&format!("  {id}[\"{}\"]\n", mermaid_label(&label)));
    }

    if !nodes.is_empty() {
        out.push('\n');
    }

    for relation in relations {
        let Some(from_id) = node_ids.get(&relation.from_schema_path) else {
            continue;
        };
        let Some(to_id) = node_ids.get(&relation.to_schema_path) else {
            continue;
        };

        let mut label = vec![
            relation_edge_label(relation),
            relation.relation_kind.clone(),
            relation.cardinality.clone(),
        ];
        if relation.required {
            label.push("required".to_string());
        } else {
            label.push("optional".to_string());
        }
        if relation.mixed {
            label.push("mixed".to_string());
        }
        if relation.nested_array_depth >= 2 {
            label.push(format!("depth={}", relation.nested_array_depth));
        }
        let label = mermaid_label(&label.join("<br/>"));

        if relation.fk_candidate {
            out.push_str(&format!("  {from_id} -->|\"{label}\"| {to_id}\n"));
        } else {
            out.push_str(&format!("  {from_id} -. \"{label}\" .-> {to_id}\n"));
        }
    }

    if markdown_fence {
        out.push_str("```\n");
    }
    out
}

fn mermaid_direction(rankdir: &str) -> &'static str {
    match rankdir {
        "LR" => "LR",
        "TB" => "TB",
        "RL" => "RL",
        "BT" => "BT",
        _ => "LR",
    }
}

fn mermaid_label(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('|', "&#124;")
}

fn render_dot_graph(nodes: &[DotNode], relations: &[DotRelation], rankdir: &str) -> String {
    let mut out = String::new();
    out.push_str("digraph schema_fk {\n");
    out.push_str(&format!("  rankdir={};\n", dot_id(rankdir)));
    out.push_str("  node [shape=box];\n\n");

    for node in nodes {
        let label = format!(
            "{}\nkind={}\nobject_count={}",
            node.schema_path, node.schema_kind, node.object_count
        );
        out.push_str(&format!(
            "  {} [label={}, schema_kind={}, object_count={}];\n",
            dot_string(&node.schema_path),
            dot_string(&label),
            dot_string(&node.schema_kind),
            dot_string(&node.object_count.to_string()),
        ));
    }

    if !nodes.is_empty() {
        out.push('\n');
    }

    for relation in relations {
        let label = relation_edge_label(relation);
        let mut attributes = vec![
            ("label".to_string(), label),
            ("relation_kind".to_string(), relation.relation_kind.clone()),
            ("fk_owner".to_string(), relation.fk_owner.clone()),
            (
                "fk_candidate".to_string(),
                relation.fk_candidate.to_string(),
            ),
            ("field_name".to_string(), relation.field_name.clone()),
            ("cardinality".to_string(), relation.cardinality.clone()),
            ("required".to_string(), relation.required.to_string()),
            ("mixed".to_string(), relation.mixed.to_string()),
            (
                "nested_array_depth".to_string(),
                relation.nested_array_depth.to_string(),
            ),
            (
                "parent_object_count".to_string(),
                relation.parent_object_count.to_string(),
            ),
            (
                "child_object_count".to_string(),
                relation.child_object_count.to_string(),
            ),
            ("field_count".to_string(), relation.field_count.to_string()),
        ];

        if let Some(via_schema_path) = &relation.via_schema_path {
            attributes.push(("via_schema_path".to_string(), via_schema_path.clone()));
        }
        if let Some(via_array_path) = &relation.via_array_path {
            attributes.push(("via_array_path".to_string(), via_array_path.clone()));
        }
        if !relation.fk_candidate {
            attributes.push(("style".to_string(), "dashed".to_string()));
        }

        out.push_str(&format!(
            "  {} -> {} [{}];\n",
            dot_string(&relation.from_schema_path),
            dot_string(&relation.to_schema_path),
            attributes
                .into_iter()
                .map(|(key, value)| format!("{}={}", key, dot_string(&value)))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }

    out.push_str("}\n");
    out
}

fn relation_edge_label(relation: &DotRelation) -> String {
    match relation.relation_kind.as_str() {
        "object_ref" => relation.field_name.clone(),
        "array_item" | "heterogeneous_array_item" => format!("{}[*]", relation.field_name),
        "nested_array_item" => {
            let suffix = (0..relation.nested_array_depth)
                .map(|_| "[*]")
                .collect::<Vec<_>>()
                .join("");
            format!("{}{}", relation.field_name, suffix)
        }
        _ => relation.field_name.clone(),
    }
}

fn dot_id(value: &str) -> String {
    match value {
        "LR" | "TB" | "RL" | "BT" => value.to_string(),
        _ => "LR".to_string(),
    }
}

fn dot_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

fn write_files_and_index(
    outdir: &Path,
    records: &[Record],
    occurrences: &[Occurrence],
    compact_output: bool,
) -> Result<()> {
    let mut made_dirs = HashMap::<PathBuf, ()>::new();

    for record in records {
        let canonical = PathBuf::from(&record.schema_path);
        ensure_parent(&canonical, &mut made_dirs)?;
        let schema_json = if compact_output {
            serde_json::to_string(&record.schema)?
        } else {
            serde_json::to_string_pretty(&record.schema)?
        };

        fs::write(&canonical, format!("{schema_json}\n"))
            .with_context(|| format!("failed to write {}", canonical.display()))?;

        for object_path in &record.object_paths {
            if object_path == &record.schema_path {
                continue;
            }
            let object_path = PathBuf::from(object_path);
            ensure_parent(&object_path, &mut made_dirs)?;
            replace_with_symlink(&canonical, &object_path)?;
        }
    }

    let database = outdir.join("schemas.sqlite");
    let mut conn = Connection::open(&database)
        .with_context(|| format!("failed to open {}", database.display()))?;
    let tx = conn.transaction()?;
    tx.execute_batch(
        "CREATE TABLE schema_paths (schema_path TEXT NOT NULL, object_path TEXT PRIMARY KEY);
         CREATE TABLE array_index_refs (array_path TEXT NOT NULL, array_index_path TEXT NOT NULL, schema_path TEXT NOT NULL, PRIMARY KEY (array_path, array_index_path));
         CREATE TABLE schema_definitions (schema_path TEXT PRIMARY KEY, schema_kind TEXT NOT NULL, schema_json TEXT NOT NULL);
         CREATE TABLE schema_object_counts (schema_path TEXT PRIMARY KEY, object_count INTEGER NOT NULL CHECK (object_count > 0));
         CREATE TABLE schema_field_counts (schema_path TEXT NOT NULL, field_name TEXT NOT NULL, field_type TEXT NOT NULL, field_count INTEGER NOT NULL CHECK (field_count > 0), PRIMARY KEY (schema_path, field_name));
         CREATE TABLE schema_relations (relation_id INTEGER PRIMARY KEY AUTOINCREMENT, from_schema_path TEXT NOT NULL, to_schema_path TEXT NOT NULL, relation_kind TEXT NOT NULL, fk_owner TEXT NOT NULL, fk_candidate INTEGER NOT NULL CHECK (fk_candidate IN (0, 1)), field_name TEXT NOT NULL, field_type TEXT NOT NULL, cardinality TEXT NOT NULL, required INTEGER NOT NULL CHECK (required IN (0, 1)), mixed INTEGER NOT NULL CHECK (mixed IN (0, 1)), nested_array_depth INTEGER NOT NULL DEFAULT 0 CHECK (nested_array_depth >= 0), via_schema_path TEXT, via_array_path TEXT, parent_schema_path TEXT NOT NULL, child_schema_path TEXT NOT NULL, parent_object_count INTEGER NOT NULL DEFAULT 0 CHECK (parent_object_count >= 0), child_object_count INTEGER NOT NULL DEFAULT 0 CHECK (child_object_count >= 0), field_count INTEGER NOT NULL DEFAULT 0 CHECK (field_count >= 0), UNIQUE (from_schema_path, to_schema_path, relation_kind, field_name, field_type, via_schema_path, via_array_path));
         CREATE INDEX idx_schema_relations_from ON schema_relations(from_schema_path);
         CREATE INDEX idx_schema_relations_to ON schema_relations(to_schema_path);
         CREATE INDEX idx_schema_relations_kind ON schema_relations(relation_kind, fk_candidate);
         CREATE INDEX idx_schema_relations_parent_child ON schema_relations(parent_schema_path, child_schema_path);",
    )?;

    for record in records {
        let schema_kind = schema_kind(&record.schema);
        let schema_json = serde_json::to_string(&record.schema)?;
        tx.execute(
            "INSERT INTO schema_definitions(schema_path, schema_kind, schema_json) VALUES (?1, ?2, ?3) \
             ON CONFLICT(schema_path) DO UPDATE SET schema_kind=excluded.schema_kind, schema_json=excluded.schema_json",
            params![&record.schema_path, schema_kind, schema_json],
        )?;

        for object_path in &record.object_paths {
            tx.execute(
                "INSERT INTO schema_paths(schema_path, object_path) VALUES (?1, ?2) \
                 ON CONFLICT(object_path) DO UPDATE SET schema_path=excluded.schema_path",
                params![record.schema_path, object_path],
            )?;
        }

        if let Some(parent) = &record.array_parent {
            for index_path in &record.array_index_paths {
                let index_path = serde_json::to_string(index_path)?;
                tx.execute(
                    "INSERT INTO array_index_refs(array_path, array_index_path, schema_path) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(array_path, array_index_path) DO UPDATE SET schema_path=excluded.schema_path",
                    params![parent, index_path, &record.schema_path],
                )?;
            }
        }
    }
    write_occurrence_statistics(&tx, outdir, records, occurrences)?;
    write_schema_relations(&tx, records)?;
    tx.commit()?;
    Ok(())
}

fn schema_kind(schema: &Value) -> &'static str {
    match schema.as_object() {
        Some(map) if is_refs_mut_schema(map) => "heterogeneous",
        _ => "object",
    }
}

fn is_refs_mut_schema(schema: &Map<String, Value>) -> bool {
    schema.len() == 1 && schema.get("$refs_mut").and_then(Value::as_str).is_some()
}

fn write_occurrence_statistics(
    tx: &Transaction<'_>,
    outdir: &Path,
    records: &[Record],
    occurrences: &[Occurrence],
) -> Result<()> {
    let schemas_by_path = records
        .iter()
        .filter_map(|record| {
            record
                .schema
                .as_object()
                .map(|schema| (record.schema_path.as_str(), schema))
        })
        .collect::<HashMap<_, _>>();

    for occurrence in occurrences {
        let schema_path = canonical_schema_path_for_occurrence(tx, outdir, occurrence)?;
        let schema = schemas_by_path
            .get(schema_path.as_str())
            .with_context(|| format!("missing in-memory schema for {schema_path}"))?;

        if is_refs_mut_schema(schema) {
            continue;
        }

        tx.execute(
            "INSERT INTO schema_object_counts(schema_path, object_count) VALUES (?1, 1) \
             ON CONFLICT(schema_path) DO UPDATE SET object_count = schema_object_counts.object_count + 1",
            params![&schema_path],
        )?;

        for field_name in occurrence.value.keys() {
            let field_type = schema
                .get(field_name)
                .and_then(Value::as_str)
                .with_context(|| {
                    format!("missing field type for {schema_path}.{field_name} in canonical schema")
                })?;

            tx.execute(
                "INSERT INTO schema_field_counts(schema_path, field_name, field_type, field_count) VALUES (?1, ?2, ?3, 1) \
                 ON CONFLICT(schema_path, field_name) DO UPDATE SET \
                 field_type = excluded.field_type, \
                 field_count = schema_field_counts.field_count + 1",
                params![&schema_path, field_name, field_type],
            )?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelationTarget {
    target_schema_path: String,
    array_depth: usize,
    mixed: bool,
    optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SchemaRelation {
    from_schema_path: String,
    to_schema_path: String,
    relation_kind: String,
    fk_owner: String,
    fk_candidate: bool,
    field_name: String,
    field_type: String,
    cardinality: String,
    required: bool,
    mixed: bool,
    nested_array_depth: usize,
    via_schema_path: Option<String>,
    via_array_path: Option<String>,
    parent_schema_path: String,
    child_schema_path: String,
    parent_object_count: i64,
    child_object_count: i64,
    field_count: i64,
}

fn write_schema_relations(tx: &Transaction<'_>, records: &[Record]) -> Result<()> {
    let schema_kinds = records
        .iter()
        .map(|record| {
            (
                record.schema_path.clone(),
                schema_kind(&record.schema).to_string(),
            )
        })
        .collect::<HashMap<_, _>>();

    let mut variants_by_array_parent = HashMap::<String, Vec<String>>::new();
    for record in records {
        if let Some(array_parent) = &record.array_parent {
            if record.schema_path != *array_parent {
                variants_by_array_parent
                    .entry(array_parent.clone())
                    .or_default()
                    .push(record.schema_path.clone());
            }
        }
    }
    for variants in variants_by_array_parent.values_mut() {
        variants.sort();
        variants.dedup();
    }

    for record in records {
        let Some(schema) = record.schema.as_object() else {
            continue;
        };
        if is_refs_mut_schema(schema) {
            continue;
        }

        let parent_schema_path = &record.schema_path;
        let parent_object_count = object_count_for_schema(tx, parent_schema_path)?;

        for (field_name, field_type_value) in schema {
            let Some(field_type) = field_type_value.as_str() else {
                continue;
            };

            let field_count = field_count_for_schema(tx, parent_schema_path, field_name)?;
            if field_count == 0 {
                continue;
            }
            let required = parent_object_count > 0 && field_count == parent_object_count;

            for target in relation_targets_from_field_type(field_type) {
                let target_kind = schema_kinds
                    .get(&target.target_schema_path)
                    .map(String::as_str)
                    .unwrap_or("object");

                if target.array_depth == 0 {
                    if target_kind == "heterogeneous" {
                        continue;
                    }
                    let child_schema_path = target.target_schema_path.clone();
                    let relation = SchemaRelation {
                        from_schema_path: parent_schema_path.clone(),
                        to_schema_path: child_schema_path.clone(),
                        relation_kind: "object_ref".to_string(),
                        fk_owner: "parent".to_string(),
                        fk_candidate: true,
                        field_name: field_name.clone(),
                        field_type: field_type.to_string(),
                        cardinality: if required {
                            "one_to_one_candidate".to_string()
                        } else {
                            "zero_or_one_to_one_candidate".to_string()
                        },
                        required,
                        mixed: target.mixed,
                        nested_array_depth: 0,
                        via_schema_path: None,
                        via_array_path: None,
                        parent_schema_path: parent_schema_path.clone(),
                        child_schema_path: child_schema_path.clone(),
                        parent_object_count,
                        child_object_count: object_count_for_schema(tx, &child_schema_path)?,
                        field_count,
                    };
                    insert_schema_relation(tx, &relation)?;
                    continue;
                }

                let via_array_path = Some(target.target_schema_path.clone());
                if target.array_depth >= 2 {
                    let children = if target_kind == "heterogeneous" {
                        variants_by_array_parent
                            .get(&target.target_schema_path)
                            .cloned()
                            .unwrap_or_default()
                    } else {
                        vec![target.target_schema_path.clone()]
                    };
                    for child_schema_path in children {
                        let relation = SchemaRelation {
                            from_schema_path: child_schema_path.clone(),
                            to_schema_path: parent_schema_path.clone(),
                            relation_kind: "nested_array_item".to_string(),
                            fk_owner: "child".to_string(),
                            fk_candidate: false,
                            field_name: field_name.clone(),
                            field_type: field_type.to_string(),
                            cardinality: "nested_many_to_one_marker".to_string(),
                            required,
                            mixed: target.mixed,
                            nested_array_depth: target.array_depth,
                            via_schema_path: if target_kind == "heterogeneous" {
                                Some(target.target_schema_path.clone())
                            } else {
                                Some(child_schema_path.clone())
                            },
                            via_array_path: via_array_path.clone(),
                            parent_schema_path: parent_schema_path.clone(),
                            child_schema_path: child_schema_path.clone(),
                            parent_object_count,
                            child_object_count: object_count_for_schema(tx, &child_schema_path)?,
                            field_count,
                        };
                        insert_schema_relation(tx, &relation)?;
                    }
                    continue;
                }

                if target_kind == "heterogeneous" {
                    for child_schema_path in variants_by_array_parent
                        .get(&target.target_schema_path)
                        .cloned()
                        .unwrap_or_default()
                    {
                        let relation = SchemaRelation {
                            from_schema_path: child_schema_path.clone(),
                            to_schema_path: parent_schema_path.clone(),
                            relation_kind: "heterogeneous_array_item".to_string(),
                            fk_owner: "child".to_string(),
                            fk_candidate: true,
                            field_name: field_name.clone(),
                            field_type: field_type.to_string(),
                            cardinality: "many_to_one".to_string(),
                            required,
                            mixed: target.mixed,
                            nested_array_depth: 1,
                            via_schema_path: Some(target.target_schema_path.clone()),
                            via_array_path: via_array_path.clone(),
                            parent_schema_path: parent_schema_path.clone(),
                            child_schema_path: child_schema_path.clone(),
                            parent_object_count,
                            child_object_count: object_count_for_schema(tx, &child_schema_path)?,
                            field_count,
                        };
                        insert_schema_relation(tx, &relation)?;
                    }
                } else {
                    let child_schema_path = target.target_schema_path.clone();
                    let relation = SchemaRelation {
                        from_schema_path: child_schema_path.clone(),
                        to_schema_path: parent_schema_path.clone(),
                        relation_kind: "array_item".to_string(),
                        fk_owner: "child".to_string(),
                        fk_candidate: true,
                        field_name: field_name.clone(),
                        field_type: field_type.to_string(),
                        cardinality: "many_to_one".to_string(),
                        required,
                        mixed: target.mixed,
                        nested_array_depth: 1,
                        via_schema_path: Some(child_schema_path.clone()),
                        via_array_path,
                        parent_schema_path: parent_schema_path.clone(),
                        child_schema_path: child_schema_path.clone(),
                        parent_object_count,
                        child_object_count: object_count_for_schema(tx, &child_schema_path)?,
                        field_count,
                    };
                    insert_schema_relation(tx, &relation)?;
                }
            }
        }
    }

    Ok(())
}

fn insert_schema_relation(tx: &Transaction<'_>, relation: &SchemaRelation) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO schema_relations(\
         from_schema_path, to_schema_path, relation_kind, fk_owner, fk_candidate, \
         field_name, field_type, cardinality, required, mixed, nested_array_depth, \
         via_schema_path, via_array_path, parent_schema_path, child_schema_path, \
         parent_object_count, child_object_count, field_count) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            &relation.from_schema_path,
            &relation.to_schema_path,
            &relation.relation_kind,
            &relation.fk_owner,
            relation.fk_candidate as i64,
            &relation.field_name,
            &relation.field_type,
            &relation.cardinality,
            relation.required as i64,
            relation.mixed as i64,
            relation.nested_array_depth as i64,
            relation.via_schema_path.as_deref(),
            relation.via_array_path.as_deref(),
            &relation.parent_schema_path,
            &relation.child_schema_path,
            relation.parent_object_count,
            relation.child_object_count,
            relation.field_count,
        ],
    )?;
    Ok(())
}

fn object_count_for_schema(tx: &Transaction<'_>, schema_path: &str) -> Result<i64> {
    tx.query_row(
        "SELECT COALESCE((SELECT object_count FROM schema_object_counts WHERE schema_path = ?1), 0)",
        params![schema_path],
        |row| row.get(0),
    )
    .with_context(|| format!("failed to read object count for {schema_path}"))
}

fn field_count_for_schema(
    tx: &Transaction<'_>,
    schema_path: &str,
    field_name: &str,
) -> Result<i64> {
    tx.query_row(
        "SELECT COALESCE((SELECT field_count FROM schema_field_counts WHERE schema_path = ?1 AND field_name = ?2), 0)",
        params![schema_path, field_name],
        |row| row.get(0),
    )
    .with_context(|| format!("failed to read field count for {schema_path}.{field_name}"))
}

fn relation_targets_from_field_type(field_type: &str) -> Vec<RelationTarget> {
    let optional = field_type.ends_with('?');
    let trimmed = field_type.trim_end_matches('?');
    let mut targets = relation_targets_from_label(trimmed, 0, optional);
    targets.sort_by(|a, b| {
        a.target_schema_path
            .cmp(&b.target_schema_path)
            .then(a.array_depth.cmp(&b.array_depth))
    });
    targets.dedup();
    targets
}

fn relation_targets_from_label(
    label: &str,
    array_depth: usize,
    inherited_optional: bool,
) -> Vec<RelationTarget> {
    let parts = split_top_level_union(label);
    let mixed_here = parts.len() > 1;
    let mut out = Vec::new();

    for part in parts {
        let part = part.trim();
        if let Some(inner) = array_inner(part) {
            for mut target in
                relation_targets_from_label(inner, array_depth + 1, inherited_optional)
            {
                target.mixed |= mixed_here;
                out.push(target);
            }
        } else if part.ends_with(".json") {
            out.push(RelationTarget {
                target_schema_path: part.to_string(),
                array_depth,
                mixed: mixed_here,
                optional: inherited_optional,
            });
        }
    }

    out
}

fn split_top_level_union(label: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;

    for (idx, ch) in label.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => {
                parts.push(&label[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }

    parts.push(&label[start..]);
    parts
}

fn array_inner(label: &str) -> Option<&str> {
    if !label.starts_with("array(") || !label.ends_with(')') {
        return None;
    }
    Some(&label[6..label.len() - 1])
}

fn canonical_schema_path_for_occurrence(
    tx: &Transaction<'_>,
    outdir: &Path,
    occurrence: &Occurrence,
) -> Result<String> {
    match occurrence.role {
        Role::Object => {
            let object_path = format!("{}.json", reference_path(outdir, &occurrence.segments));
            tx.query_row(
                "SELECT schema_path FROM schema_paths WHERE object_path = ?1",
                params![&object_path],
                |row| row.get(0),
            )
            .with_context(|| format!("missing schema_paths entry for {object_path}"))
        }
        Role::ArrayItem | Role::RootCollection => {
            let array_path = format!("{}.json", reference_path(outdir, &occurrence.segments));
            let array_index_path = serde_json::to_string(&occurrence.array_indexes)?;
            tx.query_row(
                "SELECT schema_path FROM array_index_refs \
                 WHERE array_path = ?1 AND array_index_path = ?2",
                params![&array_path, &array_index_path],
                |row| row.get(0),
            )
            .with_context(|| {
                format!("missing array_index_refs entry for {array_path} at {array_index_path}")
            })
        }
    }
}

fn ensure_parent(path: &Path, made_dirs: &mut HashMap<PathBuf, ()>) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if !made_dirs.contains_key(dir) {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        made_dirs.insert(dir.to_path_buf(), ());
    }
    Ok(())
}

fn replace_with_symlink(target: &Path, link: &Path) -> Result<()> {
    let _ = fs::remove_file(link);
    let link_dir = link.parent().unwrap_or_else(|| Path::new("."));
    let relative = pathdiff::diff_paths(target, link_dir).unwrap_or_else(|| target.to_path_buf());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&relative, link).with_context(|| {
            format!(
                "failed to symlink {} -> {}",
                link.display(),
                relative.display()
            )
        })?;
    }
    #[cfg(not(unix))]
    {
        fs::copy(target, link).with_context(|| {
            format!("failed to copy {} to {}", target.display(), link.display())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn temp_outdir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("dump-json-refs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn build_schema_output_retains_every_walked_object_occurrence() {
        let build = build_schema_output(
            Path::new("refs"),
            vec![Entry {
                path: vec!["root".to_string()],
                index: None,
                collection: false,
                value: object(json!({
                    "items": [
                        {"meta": {"id": 1, "note": null}},
                        {"meta": {"id": 2}}
                    ]
                })),
            }],
        )
        .unwrap();

        assert_eq!(build.occurrences.len(), 5);
        assert!(build.occurrences.iter().any(|occurrence| {
            occurrence.role == Role::Object
                && occurrence.segments == vec!["root", "items", "meta"]
                && occurrence.value.contains_key("note")
        }));
        assert!(build.occurrences.iter().any(|occurrence| {
            occurrence.role == Role::ArrayItem
                && occurrence.segments == vec!["root", "items"]
                && occurrence.array_indexes == vec![1]
        }));
    }

    #[test]
    fn nullable_containers_use_schema_references_and_dump_nested_schemas() {
        let outdir = Path::new("refs");
        let root = vec!["root".to_string()];
        let objects = vec![
            object(json!({"child": null, "items": null})),
            object(json!({
                "child": {"name": "example"},
                "items": [{"id": 1}]
            })),
        ];

        let schema = schema_for_values(outdir, &root, &objects).unwrap();
        assert_eq!(schema["child"], "refs/root/child.json");
        assert_eq!(schema["items"], "array(refs/root/items.json)");

        let records = distinct_schemas(
            outdir,
            vec![Entry {
                path: root,
                index: None,
                collection: false,
                value: objects[1].clone(),
            }],
        )
        .unwrap();
        let paths = records
            .iter()
            .map(|record| record.schema_path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"refs/root/child.json"));
        assert!(paths.contains(&"refs/root/items.json"));
    }

    #[test]
    fn primitive_array_fields_do_not_gain_optional_suffixes() {
        let objects = vec![
            object(json!({"tags": ["stable"]})),
            object(json!({})),
            object(json!({"tags": null})),
        ];

        let schema = schema_for_values(Path::new("refs"), &["root".to_string()], &objects).unwrap();
        assert_eq!(schema["tags"], "array(string)");
    }

    #[test]
    fn array_labels_distinguish_objects_nested_arrays_and_mixed_members() {
        let schema = schema_for_values(
            Path::new("refs"),
            &["root".to_string()],
            &[object(json!({
                "child": {"id": 1},
                "objects": [{"id": 1}],
                "mixed": [{"id": 1}, "fallback"],
                "matrix": [[{"id": 1}]],
                "capabilities": [["gpu"]],
            }))],
        )
        .unwrap();

        assert_eq!(schema["child"], "refs/root/child.json");
        assert_eq!(schema["objects"], "array(refs/root/objects.json)");
        assert_eq!(schema["mixed"], "array(refs/root/mixed.json|string)");
        assert_eq!(schema["matrix"], "array(array(refs/root/matrix.json))");
        assert_eq!(schema["capabilities"], "array(array(string))");
    }

    #[test]
    fn mixed_array_and_scalar_fields_preserve_the_array_member_label() {
        let schema = schema_for_values(
            Path::new("refs"),
            &["root".to_string()],
            &[
                object(json!({"content": [{"type": "text"}]})),
                object(json!({"content": "<command-message>example</command-message>"})),
            ],
        )
        .unwrap();

        assert_eq!(schema["content"], "array(refs/root/content.json)|string");
    }

    #[test]
    fn nested_object_arrays_preserve_full_index_paths() {
        let records = distinct_schemas(
            Path::new("refs"),
            vec![
                Entry {
                    path: vec!["root_item".to_string()],
                    index: Some(0),
                    collection: true,
                    value: object(json!({"nested": [{"id": 1}]})),
                },
                Entry {
                    path: vec!["root_item".to_string()],
                    index: Some(1),
                    collection: true,
                    value: object(json!({"nested": [{"id": 2}]})),
                },
            ],
        )
        .unwrap();

        let nested = records
            .iter()
            .find(|record| record.schema_path == "refs/root_item/nested.json")
            .unwrap();
        assert_eq!(
            nested.array_parent.as_deref(),
            Some("refs/root_item/nested.json")
        );
        assert_eq!(nested.array_index_paths, vec![vec![0, 0], vec![1, 0]]);
    }

    #[test]
    fn sqlite_stores_nested_array_index_paths_as_json() {
        let outdir =
            std::env::temp_dir().join(format!("dump-json-refs-index-paths-{}", std::process::id()));
        let _ = fs::remove_dir_all(&outdir);
        fs::create_dir_all(&outdir).unwrap();

        let build = build_schema_output(
            &outdir,
            vec![
                Entry {
                    path: vec!["root_item".to_string()],
                    index: Some(0),
                    collection: true,
                    value: object(json!({"nested": [{"id": 1}]})),
                },
                Entry {
                    path: vec!["root_item".to_string()],
                    index: Some(1),
                    collection: true,
                    value: object(json!({"nested": [{"id": 2}]})),
                },
            ],
        )
        .unwrap();
        write_files_and_index(&outdir, &build.records, &build.occurrences, false).unwrap();

        let conn = Connection::open(outdir.join("schemas.sqlite")).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT array_index_path FROM array_index_refs \
                 WHERE array_path = ?1 ORDER BY array_index_path",
            )
            .unwrap();
        let stored = stmt
            .query_map(
                [format!("{}/root_item/nested.json", outdir.display())],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(stored, vec!["[0,0]", "[1,0]"]);

        drop(stmt);
        drop(conn);
        fs::remove_dir_all(&outdir).unwrap();
    }

    #[test]
    fn sqlite_counts_object_fields_by_the_resolved_canonical_schema() {
        let outdir = temp_outdir("field-counts");
        let build = build_schema_output(
            &outdir,
            vec![Entry {
                path: vec!["root".to_string()],
                index: None,
                collection: false,
                value: object(json!({
                    "items": [
                        {"meta": {"id": 1, "note": null}},
                        {"meta": {"id": 2}}
                    ]
                })),
            }],
        )
        .unwrap();
        write_files_and_index(&outdir, &build.records, &build.occurrences, false).unwrap();

        let conn = Connection::open(outdir.join("schemas.sqlite")).unwrap();
        let meta_path = format!("{}/root/items/meta.json", outdir.display());
        let item_path = format!("{}/root/items.json", outdir.display());
        assert_eq!(
            conn.query_row(
                "SELECT object_count FROM schema_object_counts WHERE schema_path = ?1",
                [&meta_path],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row(
                "SELECT field_count FROM schema_field_counts \
             WHERE schema_path = ?1 AND field_name = 'id'",
                [&meta_path],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row(
                "SELECT field_count FROM schema_field_counts \
             WHERE schema_path = ?1 AND field_name = 'note'",
                [&meta_path],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT object_count FROM schema_object_counts WHERE schema_path = ?1",
                [&item_path],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );

        drop(conn);
        fs::remove_dir_all(outdir).unwrap();
    }

    #[test]
    fn sqlite_does_not_count_refs_mut_array_containers() {
        let outdir = temp_outdir("refs-mut-counts");
        let build = build_schema_output(
            &outdir,
            vec![Entry {
                path: vec!["root".to_string()],
                index: None,
                collection: false,
                value: object(json!({
                    "items": [{"id": 1}, {"label": "second"}]
                })),
            }],
        )
        .unwrap();
        write_files_and_index(&outdir, &build.records, &build.occurrences, false).unwrap();

        let conn = Connection::open(outdir.join("schemas.sqlite")).unwrap();
        let container_path = format!("{}/root/items.json", outdir.display());
        let count = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_object_counts WHERE schema_path = ?1",
                [&container_path],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(count, 0);

        drop(conn);
        fs::remove_dir_all(outdir).unwrap();
    }

    #[test]
    fn sqlite_counts_homogeneous_root_collection_items_via_array_index_refs() {
        let outdir = temp_outdir("root-collection-counts");
        let build = build_schema_output(
            &outdir,
            vec![
                Entry {
                    path: vec!["root_item".to_string()],
                    index: Some(0),
                    collection: true,
                    value: object(json!({"kind": "snapshot"})),
                },
                Entry {
                    path: vec!["root_item".to_string()],
                    index: Some(1),
                    collection: true,
                    value: object(json!({"kind": "event"})),
                },
            ],
        )
        .unwrap();
        write_files_and_index(&outdir, &build.records, &build.occurrences, false).unwrap();

        let conn = Connection::open(outdir.join("schemas.sqlite")).unwrap();
        let root_path = format!("{}/root_item.json", outdir.display());
        let counts = conn
            .prepare(
                "SELECT object_count, (SELECT field_count FROM schema_field_counts \
             WHERE schema_path = ?1 AND field_name = 'kind') \
             FROM schema_object_counts WHERE schema_path = ?1",
            )
            .unwrap()
            .query_row([&root_path], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap();
        assert_eq!(counts, (2, 2));

        drop(conn);
        fs::remove_dir_all(outdir).unwrap();
    }

    #[test]
    fn jsonl_strips_leading_nul_padding_per_record() {
        let values =
            parse_jsonl_stream("\0\0{\"kind\":\"snapshot\"}\n{\"kind\":\"event\"}\n").unwrap();
        assert_eq!(
            values,
            vec![json!({"kind": "snapshot"}), json!({"kind": "event"})]
        );
    }

    #[test]
    fn jsonl_rejects_a_control_byte_inside_a_record() {
        let error = parse_jsonl_stream("{\"kind\":\"bad\0value\"}\n").unwrap_err();
        assert!(error.to_string().contains("invalid JSONL input at line 1"));
    }

    #[test]
    fn jqfile_is_not_a_supported_cli_option_or_help_entry() {
        assert!(Args::try_parse_from(["dump-json-refs", "--jqfile", "schema.jq"]).is_err());

        let mut command = Args::command();
        let help = command.render_help().to_string();
        assert!(!help.contains("--jqfile"));
    }
    #[test]
    fn writes_pretty_json_files_by_default() {
        let outdir = temp_outdir("pretty-json-output");
        let build = build_schema_output(
            &outdir,
            vec![Entry {
                path: vec!["root".to_string()],
                index: None,
                collection: false,
                value: object(json!({
                    "id": 1,
                    "name": "example"
                })),
            }],
        )
        .unwrap();

        write_files_and_index(&outdir, &build.records, &build.occurrences, false).unwrap();

        let schema_path = outdir.join("root.json");
        let written = fs::read_to_string(&schema_path).unwrap();

        assert!(written.contains('\n'));
        assert!(written.contains("  \"id\""));
        assert_eq!(
            serde_json::from_str::<Value>(&written).unwrap(),
            json!({
                "id": "number",
                "name": "string"
            })
        );

        fs::remove_dir_all(outdir).unwrap();
    }

    #[test]
    fn writes_compact_json_files_with_compact_output_flag() {
        let outdir = temp_outdir("compact-json-output");
        let build = build_schema_output(
            &outdir,
            vec![Entry {
                path: vec!["root".to_string()],
                index: None,
                collection: false,
                value: object(json!({
                    "id": 1,
                    "name": "example"
                })),
            }],
        )
        .unwrap();

        write_files_and_index(&outdir, &build.records, &build.occurrences, true).unwrap();

        let schema_path = outdir.join("root.json");
        let written = fs::read_to_string(&schema_path).unwrap();

        assert_eq!(written, "{\"id\":\"number\",\"name\":\"string\"}\n");

        fs::remove_dir_all(outdir).unwrap();
    }

    #[test]
    fn stdin_style_input_falls_back_to_jsonl_when_later_record_has_nul_padding() {
        let entries = normalize_input(
            "{\"kind\":\"snapshot\"}\n\0\0{\"kind\":\"event\"}\n",
            false,
            false,
            "root",
        )
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, vec!["root_item"]);
        assert_eq!(entries[0].index, Some(0));
        assert_eq!(entries[1].index, Some(1));
    }

    #[test]
    fn sqlite_stores_field_types_from_canonical_schema() {
        let outdir = temp_outdir("field-types");
        let build = build_schema_output(
            &outdir,
            vec![Entry {
                path: vec!["root".to_string()],
                index: None,
                collection: false,
                value: object(json!({
                    "id": 1,
                    "optional": null,
                    "items": [{"kind": "a"}, {"kind": "b"}]
                })),
            }],
        )
        .unwrap();
        write_files_and_index(&outdir, &build.records, &build.occurrences, false).unwrap();

        let conn = Connection::open(outdir.join("schemas.sqlite")).unwrap();
        let root_path = format!("{}/root.json", outdir.display());
        let item_path = format!("{}/root/items.json", outdir.display());

        assert_eq!(
            conn.query_row(
                "SELECT field_type FROM schema_field_counts WHERE schema_path = ?1 AND field_name = 'items'",
                [&root_path],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            format!("array({item_path})")
        );
        assert_eq!(
            conn.query_row(
                "SELECT field_type FROM schema_field_counts WHERE schema_path = ?1 AND field_name = 'kind'",
                [&item_path],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "string".to_string()
        );

        drop(conn);
        fs::remove_dir_all(outdir).unwrap();
    }

    #[test]
    fn report_fields_are_sorted_by_count_desc_and_include_field_type() {
        let report = ReportData {
            schemas: vec![SchemaCount {
                schema_path: "refs/root.json".to_string(),
                schema_kind: "object".to_string(),
                object_count: 2,
            }],
            object_paths: vec![SchemaObjectPath {
                schema_path: "refs/root.json".to_string(),
                object_path: "refs/root.json".to_string(),
            }],
            array_index_refs: Vec::new(),
            fields: vec![
                FieldCount {
                    schema_path: "refs/root.json".to_string(),
                    field_name: "frequent".to_string(),
                    field_type: "string".to_string(),
                    field_count: 10,
                },
                FieldCount {
                    schema_path: "refs/root.json".to_string(),
                    field_name: "rare".to_string(),
                    field_type: "boolean?".to_string(),
                    field_count: 1,
                },
            ],
        };

        let rendered = render_report(
            &report,
            &ReportSummary::Generated {
                execution_time_ms: 12,
                input_source: "stdin".to_string(),
            },
            ReportDetail::CompactStdout,
        );

        assert!(rendered.contains("schema_path\tfield_name\tfield_type\tfield_count\n"));
        assert!(rendered.contains("refs/root.json\tfrequent\tstring\t10\n"));
        assert!(rendered.contains("refs/root.json\trare\tboolean?\t1\n"));
        assert!(!rendered.contains("[schema_array_index_refs]"));
        assert!(!rendered.contains("[schema_object_paths]"));
    }

    #[test]
    fn full_file_report_includes_object_paths_and_array_index_refs() {
        let report = ReportData {
            schemas: vec![SchemaCount {
                schema_path: "refs/root_item.json".to_string(),
                schema_kind: "object".to_string(),
                object_count: 2,
            }],
            object_paths: vec![
                SchemaObjectPath {
                    schema_path: "refs/root_item.json".to_string(),
                    object_path: "refs/root_item.json".to_string(),
                },
                SchemaObjectPath {
                    schema_path: "refs/root_item.json".to_string(),
                    object_path: "refs/alias.json".to_string(),
                },
            ],
            array_index_refs: vec![SchemaArrayIndexRef {
                schema_path: "refs/root_item.json".to_string(),
                array_path: "refs/root_item.json".to_string(),
                array_index_path: "[0]".to_string(),
            }],
            fields: Vec::new(),
        };

        let compact = render_report(
            &report,
            &ReportSummary::FromSqlite {
                sqlite_path: "refs/schemas.sqlite".to_string(),
            },
            ReportDetail::CompactStdout,
        );
        assert!(compact.contains("[schema_aliases]\n"));
        assert!(compact.contains("refs/root_item.json\trefs/alias.json\n"));
        assert!(!compact.contains("[schema_object_paths]"));
        assert!(!compact.contains("[schema_array_index_refs]"));

        let full = render_report(
            &report,
            &ReportSummary::FromSqlite {
                sqlite_path: "refs/schemas.sqlite".to_string(),
            },
            ReportDetail::FullFile,
        );
        assert!(full.contains("[schema_object_paths]\n"));
        assert!(full.contains("[schema_array_index_refs]\n"));
        assert!(full.contains("refs/root_item.json\trefs/root_item.json\t[0]\n"));
    }

    #[test]
    fn report_data_orders_schemas_by_object_count_and_groups_fields_by_schema_order() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_paths (schema_path TEXT NOT NULL, object_path TEXT PRIMARY KEY);
             CREATE TABLE array_index_refs (array_path TEXT NOT NULL, array_index_path TEXT NOT NULL, schema_path TEXT NOT NULL, PRIMARY KEY (array_path, array_index_path));
             CREATE TABLE schema_definitions (schema_path TEXT PRIMARY KEY, schema_kind TEXT NOT NULL, schema_json TEXT NOT NULL);
             CREATE TABLE schema_object_counts (schema_path TEXT PRIMARY KEY, object_count INTEGER NOT NULL CHECK (object_count > 0));
             CREATE TABLE schema_field_counts (schema_path TEXT NOT NULL, field_name TEXT NOT NULL, field_type TEXT NOT NULL, field_count INTEGER NOT NULL CHECK (field_count > 0), PRIMARY KEY (schema_path, field_name));"
        ).unwrap();

        conn.execute(
            "INSERT INTO schema_paths(schema_path, object_path) VALUES (?1, ?2)",
            ["refs/high.json", "refs/high.json"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_paths(schema_path, object_path) VALUES (?1, ?2)",
            ["refs/low.json", "refs/low.json"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_paths(schema_path, object_path) VALUES (?1, ?2)",
            ["refs/container.json", "refs/container.json"],
        )
        .unwrap();

        for (schema_path, schema_kind) in [
            ("refs/high.json", "object"),
            ("refs/low.json", "object"),
            ("refs/container.json", "heterogeneous"),
        ] {
            conn.execute(
                "INSERT INTO schema_definitions(schema_path, schema_kind, schema_json) VALUES (?1, ?2, '{}')",
                params![schema_path, schema_kind],
            )
            .unwrap();
        }

        conn.execute(
            "INSERT INTO schema_object_counts(schema_path, object_count) VALUES (?1, ?2)",
            params!["refs/high.json", 100],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_object_counts(schema_path, object_count) VALUES (?1, ?2)",
            params!["refs/low.json", 50],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO schema_field_counts(schema_path, field_name, field_type, field_count) VALUES (?1, ?2, ?3, ?4)",
            params!["refs/high.json", "rare", "string", 1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_field_counts(schema_path, field_name, field_type, field_count) VALUES (?1, ?2, ?3, ?4)",
            params!["refs/high.json", "common", "number", 90],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_field_counts(schema_path, field_name, field_type, field_count) VALUES (?1, ?2, ?3, ?4)",
            params!["refs/low.json", "very_common", "boolean", 999],
        )
        .unwrap();

        let report = read_report_data(&conn).unwrap();
        assert_eq!(
            report
                .schemas
                .iter()
                .map(|schema| schema.schema_path.as_str())
                .collect::<Vec<_>>(),
            vec!["refs/high.json", "refs/low.json", "refs/container.json"]
        );
        assert_eq!(
            report
                .fields
                .iter()
                .map(|field| (
                    field.schema_path.as_str(),
                    field.field_name.as_str(),
                    field.field_count
                ))
                .collect::<Vec<_>>(),
            vec![
                ("refs/high.json", "common", 90),
                ("refs/high.json", "rare", 1),
                ("refs/low.json", "very_common", 999),
            ]
        );
    }

    #[test]
    fn refs_mut_schemas_are_marked_heterogeneous_and_skipped_for_statistics() {
        let outdir = temp_outdir("refs-mut-kind");
        let build = build_schema_output(
            &outdir,
            vec![Entry {
                path: vec!["root".to_string()],
                index: None,
                collection: false,
                value: object(json!({
                    "items": [{"kind": "a"}, {"other": "b"}]
                })),
            }],
        )
        .unwrap();
        write_files_and_index(&outdir, &build.records, &build.occurrences, false).unwrap();

        let conn = Connection::open(outdir.join("schemas.sqlite")).unwrap();
        let container_path = format!("{}/root/items.json", outdir.display());

        assert_eq!(
            conn.query_row(
                "SELECT schema_kind FROM schema_definitions WHERE schema_path = ?1",
                [&container_path],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "heterogeneous"
        );

        let object_count_rows = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_object_counts WHERE schema_path = ?1",
                [&container_path],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(object_count_rows, 0);

        let field_count_rows = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_field_counts WHERE schema_path = ?1",
                [&container_path],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(field_count_rows, 0);

        let report = load_report_from_sqlite(&outdir.join("schemas.sqlite")).unwrap();
        let rendered = render_report(
            &report,
            &ReportSummary::FromSqlite {
                sqlite_path: outdir.join("schemas.sqlite").display().to_string(),
            },
            ReportDetail::CompactStdout,
        );
        assert!(rendered.contains(&format!("{container_path}\theterogeneous\t0\n")));

        drop(conn);
        fs::remove_dir_all(outdir).unwrap();
    }

    #[test]
    fn streaming_jsonl_generation_matches_existing_jsonl_pipeline() {
        let legacy_outdir = temp_outdir("legacy-jsonl-equivalence");
        let streaming_outdir = temp_outdir("streaming-jsonl-equivalence");
        let jsonl = concat!(
            "\0\0{\"kind\":\"snapshot\",\"payload\":{\"id\":1,\"tags\":[\"a\"],\"items\":[{\"sku\":\"a\",\"qty\":1}]}}\n",
            "{\"kind\":\"event\",\"payload\":{\"id\":2,\"tags\":[],\"items\":[{\"sku\":\"b\"},{\"sku\":\"c\",\"note\":null}]}}\n",
            "{\"kind\":\"event\",\"payload\":{\"id\":\"mixed\",\"items\":[{\"sku\":\"d\",\"qty\":4},[ {\"sku\":\"nested\"} ]]}}\n",
        );

        let entries = normalize_input(jsonl, true, true, "sample").unwrap();
        let legacy_build = build_schema_output(&legacy_outdir, entries).unwrap();
        write_files_and_index(
            &legacy_outdir,
            &legacy_build.records,
            &legacy_build.occurrences,
            false,
        )
        .unwrap();

        stream_jsonl_reader_to_outdir(jsonl.as_bytes(), &streaming_outdir, true, "sample", false)
            .unwrap();

        let legacy_conn = Connection::open(legacy_outdir.join("schemas.sqlite")).unwrap();
        let streaming_conn = Connection::open(streaming_outdir.join("schemas.sqlite")).unwrap();
        let legacy_rows = dump_index_rows_for_test(&legacy_conn).unwrap();
        let streaming_rows = dump_index_rows_for_test(&streaming_conn).unwrap();
        assert_eq!(
            legacy_rows.replace(&legacy_outdir.display().to_string(), "OUTDIR"),
            streaming_rows.replace(&streaming_outdir.display().to_string(), "OUTDIR")
        );

        let legacy_report = load_report_from_sqlite(&legacy_outdir.join("schemas.sqlite")).unwrap();
        let legacy_rendered = render_report(
            &legacy_report,
            &ReportSummary::FromSqlite {
                sqlite_path: "refs/schemas.sqlite".to_string(),
            },
            ReportDetail::CompactStdout,
        );
        let mut streaming_rendered = Vec::new();
        render_report_from_sqlite_to_writer(
            &streaming_outdir.join("schemas.sqlite"),
            &ReportSummary::FromSqlite {
                sqlite_path: "refs/schemas.sqlite".to_string(),
            },
            ReportDetail::CompactStdout,
            None,
            &mut streaming_rendered,
        )
        .unwrap();
        let streaming_rendered = String::from_utf8(streaming_rendered).unwrap();
        assert_eq!(
            legacy_rendered.replace(&legacy_outdir.display().to_string(), "OUTDIR"),
            streaming_rendered.replace(&streaming_outdir.display().to_string(), "OUTDIR")
        );

        drop(legacy_conn);
        drop(streaming_conn);
        fs::remove_dir_all(legacy_outdir).unwrap();
        fs::remove_dir_all(streaming_outdir).unwrap();
    }

    #[test]
    fn streaming_jsonl_skips_incomplete_trailing_record() {
        let complete_outdir = temp_outdir("complete-jsonl-tail");
        let truncated_outdir = temp_outdir("truncated-jsonl-tail");
        let complete = "{\"kind\":\"snapshot\"}\n{\"kind\":\"event\"}\n";
        let truncated = "{\"kind\":\"snapshot\"}\n{\"kind\":\"event\"}\n{\"kind\":\"partial";

        stream_jsonl_reader_to_outdir(complete.as_bytes(), &complete_outdir, true, "tail", false)
            .unwrap();
        stream_jsonl_reader_to_outdir(truncated.as_bytes(), &truncated_outdir, true, "tail", false)
            .unwrap();

        let complete_conn = Connection::open(complete_outdir.join("schemas.sqlite")).unwrap();
        let truncated_conn = Connection::open(truncated_outdir.join("schemas.sqlite")).unwrap();
        let complete_rows = dump_index_rows_for_test(&complete_conn).unwrap();
        let truncated_rows = dump_index_rows_for_test(&truncated_conn).unwrap();
        assert_eq!(
            complete_rows.replace(&complete_outdir.display().to_string(), "OUTDIR"),
            truncated_rows.replace(&truncated_outdir.display().to_string(), "OUTDIR")
        );

        drop(complete_conn);
        drop(truncated_conn);
        fs::remove_dir_all(complete_outdir).unwrap();
        fs::remove_dir_all(truncated_outdir).unwrap();
    }

    #[test]
    fn stream_acc_flush_matches_existing_sqlite_upsert_path() {
        let root_base = "refs/root_item";
        let object = object(json!({
            "kind": "event",
            "payload": {
                "id": 1,
                "items": [{"sku": "a"}, {"sku": "b", "qty": 2}],
                "matrix": [[{"nested": true}]]
            }
        }));
        let array_indexes = vec![7];

        let mut legacy_conn = Connection::open_in_memory().unwrap();
        create_stream_tables(&legacy_conn).unwrap();
        {
            let tx = legacy_conn.transaction().unwrap();
            record_streamed_object_occurrence(
                &tx,
                Path::new("refs"),
                root_base,
                &object,
                Role::RootCollection,
                &array_indexes,
            )
            .unwrap();
            tx.commit().unwrap();
        }

        let mut acc_conn = Connection::open_in_memory().unwrap();
        create_stream_tables(&acc_conn).unwrap();
        let mut acc = StreamAcc::default();
        record_streamed_object_occurrence_to_acc(
            &mut acc,
            Path::new("refs"),
            root_base,
            &object,
            Role::RootCollection,
            &array_indexes,
        )
        .unwrap();
        {
            let tx = acc_conn.transaction().unwrap();
            acc.flush_to_sqlite(&tx).unwrap();
            tx.commit().unwrap();
        }

        assert_eq!(
            dump_stream_rows_for_test(&legacy_conn).unwrap(),
            dump_stream_rows_for_test(&acc_conn).unwrap()
        );
    }

    fn dump_index_rows_for_test(conn: &Connection) -> Result<String> {
        let mut out = String::new();
        for sql in [
            "SELECT 'schema_paths', schema_path, object_path FROM schema_paths ORDER BY schema_path, object_path",
            "SELECT 'array_index_refs', array_path, array_index_path, schema_path FROM array_index_refs ORDER BY array_path, array_index_path, schema_path",
            "SELECT 'schema_definitions', schema_path, schema_kind, schema_json FROM schema_definitions ORDER BY schema_path",
            "SELECT 'schema_object_counts', schema_path, object_count FROM schema_object_counts ORDER BY schema_path",
            "SELECT 'schema_field_counts', schema_path, field_name, field_type, field_count FROM schema_field_counts ORDER BY schema_path, field_name, field_type",
            "SELECT 'schema_relations', from_schema_path, to_schema_path, relation_kind, fk_owner, fk_candidate, field_name, field_type, cardinality, required, mixed, nested_array_depth, COALESCE(via_schema_path, ''), COALESCE(via_array_path, ''), parent_schema_path, child_schema_path, parent_object_count, child_object_count, field_count FROM schema_relations ORDER BY from_schema_path, to_schema_path, relation_kind, field_name, field_type, COALESCE(via_schema_path, ''), COALESCE(via_array_path, '')",
        ] {
            let mut stmt = conn.prepare(sql)?;
            let column_count = stmt.column_count();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                for index in 0..column_count {
                    if index > 0 {
                        out.push('\t');
                    }
                    let value = row
                        .get_ref(index)?
                        .as_str()
                        .map(str::to_string)
                        .or_else(|_| row.get::<_, i64>(index).map(|value| value.to_string()))?;
                    out.push_str(&value);
                }
                out.push('\n');
            }
        }
        Ok(out)
    }

    fn dump_stream_rows_for_test(conn: &Connection) -> Result<String> {
        let mut out = String::new();
        for sql in [
            "SELECT 'stream_object_totals', path_base, object_count FROM stream_object_totals ORDER BY path_base",
            "SELECT 'stream_object_field_facts', path_base, field_name, present_count, array_count, boolean_count, null_count, number_count, object_count, string_count FROM stream_object_field_facts ORDER BY path_base, field_name",
            "SELECT 'stream_array_member_facts', path_base, depth, member_count, array_count, boolean_count, null_count, number_count, object_count, string_count FROM stream_array_member_facts ORDER BY path_base, depth",
            "SELECT 'stream_item_schema_counts', role, path_base, canonical_json, schema_json, object_count FROM stream_item_schema_counts ORDER BY role, path_base, canonical_json",
            "SELECT 'stream_item_schema_index_refs', role, path_base, canonical_json, array_index_path, index_path_key FROM stream_item_schema_index_refs ORDER BY role, path_base, array_index_path",
            "SELECT 'stream_item_schema_field_counts', role, path_base, canonical_json, field_name, field_count FROM stream_item_schema_field_counts ORDER BY role, path_base, canonical_json, field_name",
        ] {
            let mut stmt = conn.prepare(sql)?;
            let column_count = stmt.column_count();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                for index in 0..column_count {
                    if index > 0 {
                        out.push('\t');
                    }
                    let value = row
                        .get_ref(index)?
                        .as_str()
                        .map(str::to_string)
                        .or_else(|_| row.get::<_, i64>(index).map(|value| value.to_string()))?;
                    out.push_str(&value);
                }
                out.push('\n');
            }
        }
        Ok(out)
    }
}
