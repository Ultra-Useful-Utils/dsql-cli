# Output formats

`dsql` writes results as tables by default, or as CSV, TSV, or framed JSON Lines
when selected with `--format`. All formats preserve result order. A database `NULL`
is distinct from an empty string.

## Table

Table output is intended for people. It displays each row-producing result set,
including duplicate column names, and command-completion information. It supports
multiple result sets and Unicode display width. Embedded newlines, tabs, and
terminal control characters are escaped visibly so each value remains on one row.

## CSV and TSV

CSV and TSV stream one row-producing result set; a second row-producing result
set is an error. Delimiters, quotes, and embedded newlines use the normal format
escaping rules. `NULL` is encoded as `\N`. A non-null value beginning with a
backslash is encoded with one additional leading backslash, so `\\N` means the
literal text `\N` and the convention remains reversible. Empty text is encoded as
an empty field, not as `\N`.

When CSV or TSV is written directly to a terminal, terminal control characters
are rendered as visible escapes to prevent query data from controlling the
terminal. Redirected output remains byte-for-byte machine data under the encoding
rules above.

## Framed JSON Lines (version 1)

JSON Lines is a streaming protocol: every frame is one UTF-8 JSON object followed
by `\n`. Version 1 uses these lossless frames, with fields and meanings frozen:

```json
{"version":1,"type":"columns","result":0,"columns":["id","id"]}
{"version":1,"type":"row","result":0,"values":["first",null]}
{"version":1,"type":"complete","result":0,"rows":1}
{"version":1,"type":"command","rows":3}
{"version":1,"type":"notice","message":"server notice"}
{"version":1,"type":"error","sqlstate":"23505","diagnostic":"duplicate key"}
```

- `columns` begins a row-producing result. `result` starts at zero and increments
  for each `columns` frame. `columns` is ordered and may contain duplicate names.
- `row` belongs to its `result`; `values` is an ordered array of strings or JSON
  `null` values. JSON escaping preserves Unicode, control characters, and embedded
  newlines without changing their data values.
- `complete` closes a row-producing result and reports its row count.
- `command` reports a command-only completion and has no `result` field.
- `notice` can occur while results stream.
- `error` is a final execution error. `sqlstate` is a string or `null`; diagnostic
  text is the diagnostic supplied to the output layer.

Consumers must ignore unknown frame types and unknown fields so later protocol
versions can add optional frames without breaking existing parsers. Consumers must
still use the `version` field to select a compatible schema.

If execution fails after rows have already been written, those frames remain valid
and an `error` frame is written when output is still available; there is no
synthetic `complete` frame for that partial result. The command exits with its
runtime failure status after writing the error frame.

The renderer serializes and writes one frame at a time. It retains only the active
result's column width, result index, and row count, plus the current frame; memory
does not grow with the number of rows or completed results.

## Memory behavior

All output modes consume one database row at a time. CSV and TSV use the `csv`
writer's fixed internal buffer; JSON Lines serializes one frame directly to its
writer; table output retains only bounded column widths; expanded output retains
only column labels and writes field values in 4 KiB chunks. Pager output writes
directly to the pager pipe and does not collect the result first. Memory therefore
depends on the active row and its fields, not on the number of rows already emitted.

The ignored one-million-row acceptance fixture exercises table, expanded, CSV,
TSV, and JSON Lines output with a writer that retains no output and verifies that
every row is written through bounded write calls:

```console
cargo test --release --locked one_million_rows_stream_in_every_output_format -- --ignored
```

On the Linux release build used for PERF-001, this fixture completed in 1.09
seconds with a 6,792 KiB maximum resident set size as reported by
`/usr/bin/time -v`. The counting writers discard bytes, so this measures the
renderers and fixture rather than retaining their approximately linear output.

File and standard-input scripts are read in fixed 64 KiB chunks. Every SQL
statement is limited to 16 MiB, and each complete `-c` or interactive submission
is also limited to 16 MiB so many small statements cannot bypass the bound. Completion
metadata retains at most 1 MiB per catalog query and rejects fields over 1 KiB.
Diagnostics are limited to 64 KiB and eight chained causes. Cluster discovery is
limited to 10,000 entries while still following every normal `ListClusters` page.
