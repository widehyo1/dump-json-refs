use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "json-refs")]
#[command(
    about = "Generate compact JSON schema refs and a SQLite path index from JSON/JSONL input"
)]
struct Args {
    /// Force JSONL mode. Without this flag, *.jsonl input files are also treated as JSONL.
    #[arg(long)]
    jsonl: bool,

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

fn main() -> Result<()> {
    let args = Args::parse();
    run(args)
}

fn run(args: Args) -> Result<()> {
    reject_unsafe_outdir(&args.outdir)?;

    let (input, file_mode, base, source_is_jsonl) = read_input(&args)?;
    let jsonl_mode = args.jsonl || source_is_jsonl;
    let entries = normalize_input(&input, jsonl_mode, file_mode, &base)?;
    let records = distinct_schemas(&args.outdir, entries)?;

    if args.outdir.exists() {
        fs::remove_dir_all(&args.outdir)
            .with_context(|| format!("failed to remove {}", args.outdir.display()))?;
    }
    fs::create_dir_all(&args.outdir)
        .with_context(|| format!("failed to create {}", args.outdir.display()))?;

    write_records(&records)?;
    write_files_and_index(&args.outdir, &records)?;
    Ok(())
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
        let root = if file_mode {
            format!("{base}_ref")
        } else {
            "root_item".to_string()
        };
        let values = parse_jsonl_stream(input)?;
        if values.is_empty() {
            bail!("input is empty");
        }
        return values
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
            .collect();
    }

    let values = parse_json_stream(input)?;
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

fn distinct_schemas(outdir: &Path, entries: Vec<Entry>) -> Result<Vec<Record>> {
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
            .into_iter()
            .filter(|o| o.role == Role::RootCollection)
            .collect(),
    )?);
    Ok(records)
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

fn write_records(records: &[Record]) -> Result<()> {
    for record in records {
        println!("{}", serde_json::to_string(record)?);
    }
    Ok(())
}

fn write_files_and_index(outdir: &Path, records: &[Record]) -> Result<()> {
    let mut made_dirs = HashMap::<PathBuf, ()>::new();

    for record in records {
        let canonical = PathBuf::from(&record.schema_path);
        ensure_parent(&canonical, &mut made_dirs)?;
        fs::write(
            &canonical,
            format!("{}\n", serde_json::to_string(&record.schema)?),
        )
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
         CREATE TABLE array_index_refs (array_path TEXT NOT NULL, array_index_path TEXT NOT NULL, schema_path TEXT NOT NULL, PRIMARY KEY (array_path, array_index_path));",
    )?;

    for record in records {
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
    tx.commit()?;
    Ok(())
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

        let records = distinct_schemas(
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
        write_files_and_index(&outdir, &records).unwrap();

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
        assert!(Args::try_parse_from(["json-refs", "--jqfile", "schema.jq"]).is_err());

        let mut command = Args::command();
        let help = command.render_help().to_string();
        assert!(!help.contains("--jqfile"));
    }
}
