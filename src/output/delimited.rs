use crate::{
    app::{ExecutionEvent, ExecutionSink},
    error::ApplicationError,
    output::escape_terminal_text,
};
use std::io::Write;

/// A machine-readable delimiter selected for a result stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DelimitedFormat {
    Csv,
    Tsv,
}

impl DelimitedFormat {
    const fn delimiter(self) -> u8 {
        match self {
            Self::Csv => b',',
            Self::Tsv => b'\t',
        }
    }
}

/// Streams one row-producing result set as CSV or TSV.
///
/// The null encoding is deliberately reversible: null is written as the exact
/// `\N` field, while every non-null field starting with `\` gains one leading
/// backslash. Therefore empty text remains empty and literal `\N` is written
/// as `\\N`, rather than colliding with null.
pub(crate) struct DelimitedSink<W: Write + Send, D: Write + Send> {
    writer: csv::Writer<W>,
    diagnostics: D,
    column_count: Option<usize>,
    row_set_seen: bool,
    result_complete: bool,
    terminal: bool,
    escape_data_controls: bool,
}

impl<W, D> DelimitedSink<W, D>
where
    W: Write + Send,
    D: Write + Send,
{
    pub(crate) fn new(format: DelimitedFormat, output: W, diagnostics: D) -> Self {
        Self {
            writer: csv::WriterBuilder::new()
                .delimiter(format.delimiter())
                .from_writer(output),
            diagnostics,
            column_count: None,
            row_set_seen: false,
            result_complete: false,
            terminal: false,
            escape_data_controls: false,
        }
    }

    pub(crate) fn csv(output: W, diagnostics: D) -> Self {
        Self::new(DelimitedFormat::Csv, output, diagnostics)
    }

    pub(crate) fn tsv(output: W, diagnostics: D) -> Self {
        Self::new(DelimitedFormat::Tsv, output, diagnostics)
    }

    pub(crate) fn with_terminal_escaping(mut self, enabled: bool) -> Self {
        self.escape_data_controls = enabled;
        self
    }

    fn write_record<I, T>(&mut self, record: I) -> Result<(), ApplicationError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        self.writer.write_record(record).map_err(|error| {
            delimited_write_error(matches!(
                error.kind(),
                csv::ErrorKind::Io(error) if error.kind() == std::io::ErrorKind::BrokenPipe
            ))
        })
    }

    fn flush(&mut self) -> Result<(), ApplicationError> {
        self.writer
            .flush()
            .map_err(|error| delimited_write_error(error.kind() == std::io::ErrorKind::BrokenPipe))
    }

    fn diagnostic(&mut self, message: impl std::fmt::Display) -> Result<(), ApplicationError> {
        writeln!(self.diagnostics, "{message}")
            .map_err(|_| ApplicationError::runtime("could not write diagnostics"))
    }

    fn encode_cell(value: Option<String>, escape_controls: bool) -> String {
        match value {
            None => r"\N".to_owned(),
            Some(value) => {
                let value = if escape_controls {
                    escape_terminal_text(&value)
                } else {
                    value
                };
                if value.starts_with('\\') {
                    format!("\\{value}")
                } else {
                    value
                }
            }
        }
    }
}

fn delimited_write_error(broken_pipe: bool) -> ApplicationError {
    if broken_pipe {
        ApplicationError::broken_pipe("could not write delimited output")
    } else {
        ApplicationError::runtime("could not write delimited output")
    }
}

impl<W, D> ExecutionSink for DelimitedSink<W, D>
where
    W: Write + Send,
    D: Write + Send,
{
    fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
        if self.terminal {
            return Err(ApplicationError::runtime(
                "invalid delimited execution event order",
            ));
        }
        match event {
            ExecutionEvent::Columns(columns) => {
                if self.row_set_seen {
                    return Err(ApplicationError::runtime(
                        "delimited output supports exactly one row-producing result set",
                    ));
                }
                self.column_count = Some(columns.len());
                self.row_set_seen = true;
                self.result_complete = false;
                let escape_controls = self.escape_data_controls;
                self.write_record(columns.into_iter().map(|column| {
                    if escape_controls {
                        escape_terminal_text(&column)
                    } else {
                        column
                    }
                }))
            }
            ExecutionEvent::Row(row) => {
                let Some(column_count) = self.column_count else {
                    return Err(ApplicationError::runtime(
                        "received a row before its column description",
                    ));
                };
                if self.result_complete {
                    return Err(ApplicationError::runtime(
                        "received a row after result completion",
                    ));
                }
                if row.len() != column_count {
                    return Err(ApplicationError::runtime(format!(
                        "row has {} values but the result has {column_count} columns",
                        row.len()
                    )));
                }
                let escape_controls = self.escape_data_controls;
                self.write_record(
                    row.into_iter()
                        .map(|value| Self::encode_cell(value, escape_controls)),
                )
            }
            ExecutionEvent::CommandComplete { .. } => {
                if self.row_set_seen && !self.result_complete {
                    self.flush()?;
                    self.result_complete = true;
                }
                Ok(())
            }
            ExecutionEvent::Notice(notice) => {
                self.diagnostic(format_args!("notice: {}", escape_terminal_text(&notice)))
            }
            ExecutionEvent::Error {
                sqlstate,
                diagnostic,
            } => {
                // A driver error terminates a result without a command-complete
                // event, so preserve already-streamed records before reporting it.
                self.flush()?;
                let result = match sqlstate {
                    Some(sqlstate) => self.diagnostic(format_args!(
                        "error [{}]: {}",
                        escape_terminal_text(&sqlstate),
                        escape_terminal_text(&diagnostic)
                    )),
                    None => self
                        .diagnostic(format_args!("error: {}", escape_terminal_text(&diagnostic))),
                };
                result?;
                self.terminal = true;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DelimitedSink;
    use crate::app::{ExecutionEvent, ExecutionSink};
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("writer state").clone()).expect("utf-8")
        }
    }

    impl io::Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("writer state")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct BrokenWriter;

    impl io::Write for BrokenWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
    }

    #[test]
    fn csv_quotes_duplicate_headers_and_special_values() {
        let output = SharedWriter::default();
        let diagnostics = SharedWriter::default();
        let mut sink = DelimitedSink::csv(output.clone(), diagnostics);

        sink.emit(ExecutionEvent::Columns(vec![
            "id".into(),
            "id".into(),
            "note".into(),
        ]))
        .expect("headers");
        sink.emit(ExecutionEvent::Row(vec![
            Some("first,field".into()),
            Some("a \"quote\"".into()),
            Some("two\nlines".into()),
        ]))
        .expect("row");
        sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
            .expect("complete");

        assert_eq!(
            output.text(),
            "id,id,note\n\"first,field\",\"a \"\"quote\"\"\",\"two\nlines\"\n"
        );
    }

    #[test]
    fn tsv_quotes_tabs_newlines_and_quotes() {
        let output = SharedWriter::default();
        let mut sink = DelimitedSink::tsv(output.clone(), SharedWriter::default());

        sink.emit(ExecutionEvent::Columns(vec!["value".into()]))
            .expect("headers");
        sink.emit(ExecutionEvent::Row(vec![Some("a\tb\n\"quoted\"".into())]))
            .expect("row");
        sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
            .expect("complete");

        assert_eq!(output.text(), "value\n\"a\tb\n\"\"quoted\"\"\"\n");
    }

    #[test]
    fn terminal_machine_output_escapes_csi_osc_and_c1_controls() {
        let output = SharedWriter::default();
        let mut sink = DelimitedSink::csv(output.clone(), SharedWriter::default())
            .with_terminal_escaping(true);

        sink.emit(ExecutionEvent::Columns(vec!["na\u{9b}me".into()]))
            .expect("headers");
        sink.emit(ExecutionEvent::Row(vec![Some(
            "\u{1b}[2J\u{1b}]8;;https://example.invalid\u{7}link\u{9d}".into(),
        )]))
        .expect("row");
        sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
            .expect("complete");

        let rendered = output.text();
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{9b}'));
        assert!(!rendered.contains('\u{9d}'));
        assert!(rendered.contains(r"\u{001b}[2J"));
    }

    #[test]
    fn null_empty_and_literal_null_marker_remain_distinct() {
        let output = SharedWriter::default();
        let mut sink = DelimitedSink::csv(output.clone(), SharedWriter::default());

        sink.emit(ExecutionEvent::Columns(vec![
            "null".into(),
            "empty".into(),
            "text".into(),
        ]))
        .expect("headers");
        sink.emit(ExecutionEvent::Row(vec![
            None,
            Some(String::new()),
            Some(r"\N".into()),
        ]))
        .expect("row");
        sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
            .expect("complete");

        // Null is exactly `\N`; literal `\N` is escaped with another slash.
        assert_eq!(output.text(), "null,empty,text\n\\N,,\\\\N\n");
    }

    #[test]
    fn a_second_row_set_fails_after_command_only_results() {
        let output = SharedWriter::default();
        let mut sink = DelimitedSink::csv(output.clone(), SharedWriter::default());

        sink.emit(ExecutionEvent::CommandComplete { rows: 0 })
            .expect("command-only result");
        sink.emit(ExecutionEvent::Columns(vec!["first".into()]))
            .expect("first result");
        sink.emit(ExecutionEvent::CommandComplete { rows: 0 })
            .expect("first completion");
        let error = sink
            .emit(ExecutionEvent::Columns(vec!["second".into()]))
            .expect_err("second row set is refused immediately");

        assert!(
            error
                .to_string()
                .contains("exactly one row-producing result set")
        );
        assert_eq!(output.text(), "first\n");
    }

    #[test]
    fn row_width_must_match_columns() {
        let mut sink = DelimitedSink::csv(SharedWriter::default(), SharedWriter::default());
        sink.emit(ExecutionEvent::Columns(vec!["one".into(), "two".into()]))
            .expect("headers");

        let error = sink
            .emit(ExecutionEvent::Row(vec![Some("only one".into())]))
            .expect_err("width mismatch");

        assert!(
            error
                .to_string()
                .contains("1 values but the result has 2 columns")
        );
    }

    #[test]
    fn notices_and_errors_are_diagnostics_after_partial_output() {
        let output = SharedWriter::default();
        let diagnostics = SharedWriter::default();
        let mut sink = DelimitedSink::csv(output.clone(), diagnostics.clone());

        sink.emit(ExecutionEvent::Columns(vec!["id".into()]))
            .expect("headers");
        sink.emit(ExecutionEvent::Row(vec![Some("1".into())]))
            .expect("partial row");
        sink.emit(ExecutionEvent::Notice("still\r\u{1b}[2J running".into()))
            .expect("notice");
        sink.emit(ExecutionEvent::Error {
            sqlstate: Some("22012".into()),
            diagnostic: "division\nby zero".into(),
        })
        .expect("error diagnostic");

        assert_eq!(output.text(), "id\n1\n");
        assert_eq!(
            diagnostics.text(),
            "notice: still\\r\\u{001b}[2J running\nerror [22012]: division\\nby zero\n"
        );
        assert!(
            sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
                .is_err()
        );
    }

    #[test]
    fn broken_output_maps_to_a_stable_application_error() {
        let mut sink = DelimitedSink::csv(BrokenWriter, SharedWriter::default());
        sink.emit(ExecutionEvent::Columns(vec!["id".into()]))
            .expect("buffered headers");

        let error = sink
            .emit(ExecutionEvent::CommandComplete { rows: 0 })
            .expect_err("flush observes broken pipe");

        assert_eq!(error.to_string(), "could not write delimited output");
        assert!(error.is_quiet());
    }
}
