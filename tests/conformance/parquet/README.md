# Parquet files for tests

Small files that other programs wrote, and the values that another program
reads from them. `crates/table/src/parquet/samples.rs` imports all of them and
compares.

- `arrow-*` are written by pyarrow 26.0.1, and their values are read by DuckDB
  1.4.3 (DuckDB-WASM 1.32.0). The values of `arrow-split`, `arrow-digits` and
  `arrow-nanos` are read by pyarrow itself: DuckDB does not read these encodings,
  rounds decimals of more than 38 digits, and cuts times of day to microseconds.
- `duckdb-*` are written by DuckDB, and their values are read by pyarrow.

Each `.tsv` file has the names of the columns in its first line and the rows
after it. Fields are separated by tabs; a tab, a line feed, a carriage return
and a backslash in a text are written `\t`, `\n`, `\r` and `\\`. A blank field is
a missing value, an empty text, a number that is not finite, or a date outside
the years 1 to 9999. Columns that are not imported have no field.

Files that hold the same values share one file of them:

- `arrow-types.tsv`: `arrow-old`, `arrow-snappy`, `arrow-gzip`, `arrow-zstd`,
  `arrow-lz4`, `arrow-checksums`; its first line also goes with `arrow-empty`.
- `arrow-encodings.tsv`: `arrow-encodings`, `arrow-encodings-zstd`.
- `duckdb-types.tsv`: `duckdb-snappy`, `duckdb-gzip`, `duckdb-zstd`,
  `duckdb-lz4_raw`, `duckdb-v2`.

Everything here is made by the two scripts, which need nothing but pyarrow and
the npm package `@duckdb/duckdb-wasm` with its Parquet extension:

```
python -I make.py write <directory>
node duckdb.mjs <dist of @duckdb/duckdb-wasm> write <directory>
node duckdb.mjs <dist of @duckdb/duckdb-wasm> read <directory> arrow-snappy ...
python -I make.py read <directory> arrow-split arrow-digits arrow-nanos duckdb-snappy ...
```

What neither program writes is covered by files that the tests write
themselves: `crates/table/src/parquet/encode.rs`.
