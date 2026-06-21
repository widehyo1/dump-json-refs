# json-refs

## Build

```bash
cargo build --release
```

Binary:

```bash
./target/release/json-refs --outdir refs input.json
./target/release/json-refs --jsonl --outdir refs input.jsonl
cat input.json | ./target/release/json-refs --outdir refs
```

## Output

The command removes and recreates the output directory, then creates:

- schema JSON files under `refs/`
- relative symlinks for duplicate canonical schemas on Unix
- `refs/schemas.sqlite`

SQLite schema:

```sql
CREATE TABLE schema_paths (
  schema_path TEXT NOT NULL,
  object_path TEXT PRIMARY KEY
);

CREATE TABLE array_index_refs (
  array_path TEXT NOT NULL,
  array_index INTEGER NOT NULL,
  schema_path TEXT NOT NULL,
  PRIMARY KEY (array_path, array_index)
);
```

## Semantics

- JSON object input: root path is `<filename-without-ext>` or `root` for stdin.
- JSON top-level array of objects: root collection path is `<filename-without-ext>_ref` or `root_item` for stdin.
- JSONL input: every JSON value must be an object; root collection naming is the same as top-level array.
- Primitive labels: `string`, `number`, `boolean`, `null`, optional suffix `?`, union via `|`.
- Primitive arrays: `array(string)`, `array(number|null)`-style labels.
- Object and object-array fields point to `<path>.json`; nullable or missing container values do not add `?` or `|null` to that reference.
- Object arrays:
  - homogeneous object-array schema: one `<array_path>.json`
  - heterogeneous object-array schema: `<array_path>.json` contains `{ "$refs_mut": "<array_path>/" }`; distinct schemas are stored as `<array_path>/<first_index>.json`
- Top-level collection:
  - homogeneous: one `<root_collection>.json`
  - heterogeneous: distinct `<root_collection>/<first_index>.json` files only
