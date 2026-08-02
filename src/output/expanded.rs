use crate::{
    app::{ExecutionEvent, ExecutionSink},
    error::ApplicationError,
    output::escape_terminal_text,
};
use std::io::Write;
use unicode_width::UnicodeWidthStr;

/// A streaming, terminal-safe renderer for expanded (`\x`) query output.
pub(crate) struct ExpandedExecutionSink<Output, Diagnostics> {
    output: Output,
    diagnostics: Diagnostics,
    result: Option<ResultSet>,
    rows: u64,
    needs_separator: bool,
    terminal: bool,
    failed: Option<WriteFailure>,
}

struct ResultSet {
    columns: Vec<String>,
    label_width: usize,
}

#[derive(Clone, Copy)]
enum WriteTarget {
    Output,
    Diagnostics,
}

#[derive(Clone, Copy)]
struct WriteFailure {
    target: WriteTarget,
    broken_pipe: bool,
}

impl<Output, Diagnostics> ExpandedExecutionSink<Output, Diagnostics>
where
    Output: Write + Send,
    Diagnostics: Write + Send,
{
    pub(crate) fn new(output: Output, diagnostics: Diagnostics) -> Self {
        Self {
            output,
            diagnostics,
            result: None,
            rows: 0,
            needs_separator: false,
            terminal: false,
            failed: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn into_writers(self) -> (Output, Diagnostics) {
        (self.output, self.diagnostics)
    }

    fn render_columns(&mut self, columns: Vec<String>) -> Result<(), ApplicationError> {
        if self.result.is_some() {
            return Err(invalid_event());
        }
        if self.needs_separator {
            self.write_output("\n")?;
        }
        let columns: Vec<_> = columns
            .into_iter()
            .map(|column| escape_terminal_text(&column))
            .collect();
        let label_width = columns
            .iter()
            .map(|column| UnicodeWidthStr::width(column.as_str()))
            .max()
            .unwrap_or(0);
        self.result = Some(ResultSet {
            columns,
            label_width,
        });
        self.rows = 0;
        self.needs_separator = true;
        Ok(())
    }

    fn render_row(&mut self, row: Vec<Option<String>>) -> Result<(), ApplicationError> {
        let Some(result) = self.result.take() else {
            return Err(invalid_event());
        };
        if row.len() != result.columns.len() {
            self.result = Some(result);
            return Err(invalid_event());
        }

        self.rows += 1;
        self.write_output(&format!("-[ RECORD {} ]-\n", self.rows))?;
        for (column, value) in result.columns.iter().zip(row) {
            self.write_output(&format!(
                "{column:<label_width$} | ",
                label_width = result.label_width
            ))?;
            if let Some(value) = value {
                self.write_escaped_output(&value)?;
            } else {
                self.write_output("NULL")?;
            }
            self.write_output("\n")?;
        }
        self.result = Some(result);
        Ok(())
    }

    fn complete(&mut self, rows: u64) -> Result<(), ApplicationError> {
        if self.result.is_some() && self.rows != rows {
            return Err(invalid_event());
        }
        if self.result.take().is_some() {
            let noun = if rows == 1 { "row" } else { "rows" };
            self.write_output(&format!("({rows} {noun})\n"))?;
        } else {
            if self.needs_separator {
                self.write_output("\n")?;
            }
            self.write_output(&format!("({rows} rows affected)\n"))?;
            self.needs_separator = true;
        }
        Ok(())
    }

    fn diagnostic(&mut self, message: String) -> Result<(), ApplicationError> {
        self.write_diagnostics(&message)?;
        self.write_diagnostics("\n")
    }

    fn write_output(&mut self, value: &str) -> Result<(), ApplicationError> {
        self.write(value, WriteTarget::Output)
    }

    fn write_escaped_output(&mut self, value: &str) -> Result<(), ApplicationError> {
        const CHUNK_BYTES: usize = 4096;

        let mut chunk = String::with_capacity(CHUNK_BYTES);
        for character in value.chars() {
            let escaped = match character {
                '\n' => "\\n".to_owned(),
                '\r' => "\\r".to_owned(),
                '\t' => "\\t".to_owned(),
                character if character.is_control() => {
                    format!("\\u{{{:04x}}}", character as u32)
                }
                character => character.to_string(),
            };
            if chunk.len() + escaped.len() > CHUNK_BYTES && !chunk.is_empty() {
                self.write_output(&chunk)?;
                chunk.clear();
            }
            chunk.push_str(&escaped);
        }
        if !chunk.is_empty() {
            self.write_output(&chunk)?;
        }
        Ok(())
    }

    fn write_diagnostics(&mut self, value: &str) -> Result<(), ApplicationError> {
        self.write(value, WriteTarget::Diagnostics)
    }

    fn write(&mut self, value: &str, target: WriteTarget) -> Result<(), ApplicationError> {
        if let Some(failure) = self.failed {
            return Err(write_error(failure));
        }
        let writer: &mut dyn Write = match target {
            WriteTarget::Output => &mut self.output,
            WriteTarget::Diagnostics => &mut self.diagnostics,
        };
        if let Err(error) = writer
            .write_all(value.as_bytes())
            .and_then(|()| writer.flush())
        {
            let failure = WriteFailure {
                target,
                broken_pipe: error.kind() == std::io::ErrorKind::BrokenPipe,
            };
            self.failed = Some(failure);
            return Err(write_error(failure));
        }
        Ok(())
    }
}

impl<Output, Diagnostics> ExecutionSink for ExpandedExecutionSink<Output, Diagnostics>
where
    Output: Write + Send,
    Diagnostics: Write + Send,
{
    fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
        if let Some(failure) = self.failed {
            return Err(write_error(failure));
        }
        if self.terminal {
            return Err(invalid_event());
        }
        match event {
            ExecutionEvent::Columns(columns) => self.render_columns(columns),
            ExecutionEvent::Row(row) => self.render_row(row),
            ExecutionEvent::CommandComplete { rows } => self.complete(rows),
            ExecutionEvent::Notice(notice) => {
                self.diagnostic(format!("notice: {}", escape_terminal_text(&notice)))
            }
            ExecutionEvent::Error {
                sqlstate,
                diagnostic,
            } => {
                let state = sqlstate
                    .as_deref()
                    .map(|state| format!(" [{}]", escape_terminal_text(state)))
                    .unwrap_or_default();
                self.diagnostic(format!(
                    "error{state}: {}",
                    escape_terminal_text(&diagnostic)
                ))?;
                self.terminal = true;
                Ok(())
            }
        }
    }
}

fn write_error(failure: WriteFailure) -> ApplicationError {
    let diagnostic = match failure.target {
        WriteTarget::Output => "could not render query output",
        WriteTarget::Diagnostics => "could not render query diagnostics",
    };
    if failure.broken_pipe {
        ApplicationError::broken_pipe(diagnostic)
    } else {
        ApplicationError::runtime(diagnostic)
    }
}

fn invalid_event() -> ApplicationError {
    ApplicationError::runtime("invalid expanded execution event order")
}

#[cfg(test)]
mod tests {
    use crate::{
        app::{ExecutionEvent, ExecutionSink},
        output::expanded::ExpandedExecutionSink,
    };
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Default)]
    struct ChunkWriter(Arc<Mutex<(usize, usize)>>);

    impl ChunkWriter {
        fn max_write(&self) -> usize {
            self.0.lock().expect("writer state").0
        }
    }

    impl Write for ChunkWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let mut state = self.0.lock().expect("writer state");
            state.0 = state.0.max(buffer.len());
            state.1 += buffer.len();
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn huge_expanded_field_is_written_in_bounded_chunks() {
        let output = ChunkWriter::default();
        let observation = output.clone();
        let mut sink = ExpandedExecutionSink::new(output, io::sink());
        sink.emit(ExecutionEvent::Columns(vec!["value".into()]))
            .expect("columns");
        sink.emit(ExecutionEvent::Row(vec![Some("x".repeat(8 * 1024 * 1024))]))
            .expect("large row");
        sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
            .expect("complete");

        assert!(observation.max_write() <= 4096);
    }

    #[test]
    fn expanded_rows_preserve_nulls_and_escape_terminal_controls() {
        let mut sink = ExpandedExecutionSink::new(Vec::new(), Vec::new());
        sink.emit(ExecutionEvent::Columns(vec!["id".into(), "note".into()]))
            .expect("columns render");
        sink.emit(ExecutionEvent::Row(vec![
            None,
            Some("first\nsecond\t\u{1b}[2J".into()),
        ]))
        .expect("row renders");
        sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
            .expect("completion renders");

        let (output, _) = sink.into_writers();
        assert_eq!(
            String::from_utf8(output).expect("utf-8"),
            "-[ RECORD 1 ]-\nid   | NULL\nnote | first\\nsecond\\t\\u{001b}[2J\n(1 row)\n"
        );
    }

    #[test]
    fn expanded_rows_stream_with_stable_record_boundaries() {
        let mut sink = ExpandedExecutionSink::new(Vec::new(), Vec::new());
        sink.emit(ExecutionEvent::Columns(vec!["name".into(), "id".into()]))
            .expect("columns render");
        sink.emit(ExecutionEvent::Row(vec![
            Some("Ada".into()),
            Some("1".into()),
        ]))
        .expect("first row renders");
        sink.emit(ExecutionEvent::Row(vec![
            Some("Bob".into()),
            Some("2".into()),
        ]))
        .expect("second row renders");
        sink.emit(ExecutionEvent::CommandComplete { rows: 2 })
            .expect("completion renders");

        let (output, _) = sink.into_writers();
        assert_eq!(
            String::from_utf8(output).expect("utf-8"),
            "-[ RECORD 1 ]-\nname | Ada\nid   | 1\n-[ RECORD 2 ]-\nname | Bob\nid   | 2\n(2 rows)\n"
        );
    }
}
