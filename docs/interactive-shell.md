# Interactive Shell

Running `dsql` against a cluster selector without `-c`, `-f`, or redirected standard input opens the interactive shell.

## Editing And Control

- SQL input remains in the editor until the lexical scanner finds a complete semicolon-terminated statement.
- Ctrl+C clears an idle input buffer. During execution, the first Ctrl+C stops further result output and requests PostgreSQL cancellation; another Ctrl+C ends the shell without replaying the statement.
- Ctrl+D exits from an empty input buffer.
- Ctrl+R searches shell history. History hints and SQL keyword completion are available while editing.
- SIGTERM and SIGHUP end the shell and restore terminal raw mode, including while a statement is running.

## History Privacy

History is stored at `$XDG_DATA_HOME/dsql/history`, or `$HOME/.local/share/dsql/history` when `XDG_DATA_HOME` is unset. On Unix, the persisted file is created with mode `0600`, existing symlinks are rejected, and Reedline remains bound to the verified file descriptor so replacing the pathname cannot redirect later history writes.

- `--no-history` disables history file creation and writes for the session.
- `--history-file PATH` selects an alternate history file.
- Existing history files are limited to 16 MiB, and individual entries larger than roughly 16 KiB are not retained.
- Input whose first character is a space is not persisted.
- History setup or write failures warn on stderr without ending an established cluster connection.
- Authentication tokens and generated connection strings are never submitted through the editor and are not written to history.

## Shell Commands

| Command | Behavior |
| --- | --- |
| `\q` | Exit the shell. |
| `\?` | Show supported shell commands. |
| `\conninfo` | Show terminal-safe cluster, Region, database role, connection age, and reconnect state without exposing the endpoint or AWS account. |
| `\d [pattern]` | List visible relations using a bound pattern parameter. |
| `\dt [pattern]` | List visible tables using a bound pattern parameter. |
| `\dn [pattern]` | List visible schemas using a bound pattern parameter. |
| `\du` | List visible database roles. |
| `\x [on\|off\|auto]` | Control expanded output. With `auto`, terminals narrower than 80 columns use expanded output. Omitting the argument toggles on or off. |
| `\timing [on\|off]` | Control elapsed-time diagnostics. Omitting the argument toggles the setting. |
| `\pager [on\|off]` | Control optional `less -FRX` paging. Omitting the argument toggles the setting. |
| `\refresh` | Reload bounded completion metadata while no transaction is active, failed, or uncertain. |
| `\metrics` | Open the metrics dashboard for the connected cluster. |

Meta-commands are recognized only as single-line input beginning with `\` when no SQL buffer is pending. Unknown commands and malformed arguments fail locally. Catalog patterns are data, not SQL text; quoted patterns use doubled double quotes to represent a literal quote.

## Completion And Output

Keyword completion is local. Catalog completion reads a bounded snapshot loaded after connection, so pressing Tab never performs network I/O. Permission denial or a metadata timeout leaves keyword completion available.

Table output uses the current terminal width. Expanded output, timing, and paging apply to subsequent statements. Pager commands are spawned directly without shell evaluation; a missing or early-exiting pager falls back cleanly.

## Metrics Dashboard

`\metrics` opens a temporary full-screen view using the connected cluster's AWS profile, Region, and cluster ID. Press `q` or Esc to return, `r` to refresh, or `1`, `2`, `3`, or `4` to select the 15-minute, 1-hour, 6-hour, or 24-hour range. Missing samples remain gaps or `No data`.

The SQL connection remains open while the dashboard is displayed. If it ages past the session refresh threshold, the shell refreshes it before the next SQL statement after returning. CloudWatch access requires only `cloudwatch:GetMetricData`; see [IAM Permissions](iam.md).
