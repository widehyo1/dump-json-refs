# dump-json-refs

## install

```bash
cargo install --git https://github.com/widehyo1/dump-json-refs.git
```

Binary:

```bash
dump-json-refs --outdir refs input.json
dump-json-refs --jsonl --outdir refs input.jsonl
cat input.json | dump-json-refs --outdir refs
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
  array_index_path TEXT NOT NULL,
  schema_path TEXT NOT NULL,
  PRIMARY KEY (array_path, array_index_path)
);

CREATE TABLE schema_object_counts (
  schema_path TEXT PRIMARY KEY,
  object_count INTEGER NOT NULL CHECK (object_count > 0)
);

CREATE TABLE schema_field_counts (
  schema_path TEXT NOT NULL,
  field_name TEXT NOT NULL,
  field_count INTEGER NOT NULL CHECK (field_count > 0),
  PRIMARY KEY (schema_path, field_name)
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
- JSONL input ignores raw NUL padding only at the start of a physical record; other malformed JSON remains an error that identifies its line.
- Top-level collection:
  - homogeneous: one `<root_collection>.json`
  - heterogeneous: distinct `<root_collection>/<first_index>.json` files only

Each walked JSON object increments the `object_count` of the canonical schema selected by `schema_paths` or `array_index_refs`. Each key present in that object increments its `field_count`; a key with a `null` value is present, while an omitted key is not. `$refs_mut` array containers are structural schemas, not objects, and are not counted.
