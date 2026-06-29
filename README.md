# dump-json-refs

`dump-json-refs` is a Rust CLI for extracting canonical metadata from JSON and
JSONL datasets. It walks object structures, writes deduplicated schema reference
files, indexes schema/object/field metadata into SQLite, and generates relation
seeds that help reverse engineer a relational database model from semi-structured
data.

The tool is intended for database reverse engineering workflows where raw
JSON exports need to be turned into inspectable schema candidates, field
frequency statistics, and foreign-key-like relationship graphs.

## install

Install from crates.io:

```bash
cargo install dump-json-refs
```

Or install directly from Git:

```bash
cargo install --git https://github.com/widehyo1/dump-json-refs.git
```

## Usage

```bash
dump-json-refs --outdir refs input.json
dump-json-refs --jsonl --outdir refs input.jsonl
cat input.json | dump-json-refs --outdir refs
dump-json-refs --outdir refs --graph input.json
dump-json-refs --from-sqlite refs/schemas.sqlite --graph-format mermaid-md
```

## Large JSONL inputs

For JSONL input, `dump-json-refs` streams records instead of loading the whole
file into memory. This keeps large JSONL extraction practical while preserving
the same schema, field count, object path, alias, and array index reference
semantics as normal JSONL processing.

## Output

The command removes and recreates the output directory, then creates:

- canonical schema JSON files under `refs/`
- relative symlinks for duplicate canonical schemas on Unix, or copied schema
  files on platforms without symlink support
- `refs/schemas.sqlite`
- an optional schema relation graph, such as `refs/schema.mmd`,
  `refs/schema.md`, or `refs/schema.dot`

Stdout prints a compact report with schemas, field counts, aliases, and summary
metadata. Use `--output <file>` to write the full report, including object path
and array index mappings, while keeping stdout to the summary.

## Graphs

Graph generation reads `schema_relations` from SQLite. It can run immediately
after extraction or replay from an existing index:

```bash
dump-json-refs --graph input.json
dump-json-refs --graph refs/schema.dot --graph-format dot input.json
dump-json-refs --from-sqlite refs/schemas.sqlite --graph refs/schema.md --graph-format mermaid-md
```

Graph options:

- `--graph [FILE]`: writes a graph projection. If `FILE` is omitted, the
  default path depends on the format.
- `--graph-format mermaid|mermaid-md|dot`: selects Mermaid, fenced Mermaid
  Markdown, or Graphviz DOT output. Mermaid is the default.
- `--graph-rankdir LR|TB|RL|BT`: selects graph direction for Mermaid and DOT.
- `--graph-include-marked`: includes structural marker relations such as nested
  array items; otherwise graphs include foreign-key candidates only.
- `--from-sqlite [FILE]`: loads report and graph data from an existing SQLite
  index without regenerating refs. If the flag is used without a value,
  `refs/schemas.sqlite` is used.

SQLite schema:

```sql
CREATE TABLE schema_paths (
  schema_path TEXT NOT NULL,
  object_path TEXT PRIMARY KEY
);

CREATE TABLE array_index_refs (
  array_path TEXT NOT NULL,
  array_index_path TEXT NOT NULL,
  schema_path TEXT NOT NULL,
  PRIMARY KEY (array_path, array_index_path)
);

CREATE TABLE schema_definitions (
  schema_path TEXT PRIMARY KEY,
  schema_kind TEXT NOT NULL,
  schema_json TEXT NOT NULL
);

CREATE TABLE schema_object_counts (
  schema_path TEXT PRIMARY KEY,
  object_count INTEGER NOT NULL CHECK (object_count > 0)
);

CREATE TABLE schema_field_counts (
  schema_path TEXT NOT NULL,
  field_name TEXT NOT NULL,
  field_type TEXT NOT NULL,
  field_count INTEGER NOT NULL CHECK (field_count > 0),
  PRIMARY KEY (schema_path, field_name)
);

CREATE TABLE schema_relations (
  relation_id INTEGER PRIMARY KEY AUTOINCREMENT,
  from_schema_path TEXT NOT NULL,
  to_schema_path TEXT NOT NULL,
  relation_kind TEXT NOT NULL,
  fk_owner TEXT NOT NULL,
  fk_candidate INTEGER NOT NULL CHECK (fk_candidate IN (0, 1)),
  field_name TEXT NOT NULL,
  field_type TEXT NOT NULL,
  cardinality TEXT NOT NULL,
  required INTEGER NOT NULL CHECK (required IN (0, 1)),
  mixed INTEGER NOT NULL CHECK (mixed IN (0, 1)),
  nested_array_depth INTEGER NOT NULL DEFAULT 0 CHECK (nested_array_depth >= 0),
  via_schema_path TEXT,
  via_array_path TEXT,
  parent_schema_path TEXT NOT NULL,
  child_schema_path TEXT NOT NULL,
  parent_object_count INTEGER NOT NULL DEFAULT 0 CHECK (parent_object_count >= 0),
  child_object_count INTEGER NOT NULL DEFAULT 0 CHECK (child_object_count >= 0),
  field_count INTEGER NOT NULL DEFAULT 0 CHECK (field_count >= 0),
  UNIQUE (
    from_schema_path,
    to_schema_path,
    relation_kind,
    field_name,
    field_type,
    via_schema_path,
    via_array_path
  )
);
```

## Semantics

- JSON object input: root path is `<filename-without-ext>` or `root` for stdin.
- JSON top-level array of objects: root collection path is `<filename-without-ext>_ref` or `root_item` for stdin.
- JSONL input: every JSON value must be an object; root collection naming is the same as top-level array.
- Primitive labels: `string`, `number`, `boolean`, `null`, optional suffix `?`, union via `|`.
- Object fields use a plain reference such as `<path>.json`; nullable or missing object fields do not add `?` or `|null` to that reference.
- Every array field uses `array(...)`: primitive arrays use labels such as `array(string)`, object arrays use `array(<path>.json)`, nested arrays add another wrapper, and mixed object/string items use `array(<path>.json|string)`.
- A field observed both as an array and as a scalar preserves both shapes, for example `array(<path>.json)|string`.
- Object arrays use one `<array_path>.json` when homogeneous. Heterogeneous object arrays use `<array_path>.json` containing `{ "$refs_mut": "<array_path>/" }`, with distinct schemas stored as `<array_path>/<index_path>.json`.
- `array_index_refs.array_index_path` is a JSON array of every enclosing array position; `[1,0]` identifies the first nested item inside the second outer item.
- JSONL input ignores raw NUL padding only at the start of a physical record.
- An incomplete trailing JSONL record may be ignored to tolerate partially written files.
- Other malformed JSON remains an error that identifies its line.
- Top-level collection:
  - homogeneous: one `<root_collection>.json`
  - heterogeneous: distinct `<root_collection>/<first_index>.json` files only

Each walked JSON object increments the `object_count` of the canonical schema selected by `schema_paths` or `array_index_refs`. Each key present in that object increments its `field_count`; a key with a `null` value is present, while an omitted key is not. `$refs_mut` array containers are structural schemas, not objects, and are not counted.

`schema_relations` stores inferred links between canonical schemas. Relation
rows describe direct object references, array item references, heterogeneous
array variants, and nested array markers. Rows marked with `fk_candidate = 1`
are the default graph source for relational reverse engineering; marked
structural rows can be included with `--graph-include-marked`.
