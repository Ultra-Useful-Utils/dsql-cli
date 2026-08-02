use crate::{
    app::{ExecutionEvent, ExecutionSink},
    error::ApplicationError,
    output::escape_terminal_text,
};
use std::io::Write;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A streaming, terminal-safe renderer for query execution events.
///
/// It retains only the current result set's headers and calculated widths, so
/// rows are written as they arrive rather than collected before rendering.
pub(crate) struct TableExecutionSink<Output, Diagnostics> {
    output: Output,
    diagnostics: Diagnostics,
    display_width: usize,
    result: Option<ResultSet>,
    needs_separator: bool,
    terminal: bool,
    failed: Option<WriteFailure>,
}

struct ResultSet {
    widths: Vec<usize>,
    rows: u64,
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

impl<Output, Diagnostics> TableExecutionSink<Output, Diagnostics>
where
    Output: Write + Send,
    Diagnostics: Write + Send,
{
    pub(crate) fn new(output: Output, diagnostics: Diagnostics, display_width: usize) -> Self {
        Self {
            output,
            diagnostics,
            display_width,
            result: None,
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

        let headers: Vec<String> = columns
            .iter()
            .map(|column| escape_terminal_text_to_width(column, self.display_width))
            .collect();
        let widths = column_widths(&headers, self.display_width);
        let rendered_headers = headers
            .iter()
            .zip(&widths)
            .map(|(header, width)| truncate_to_width(header, *width))
            .collect::<Vec<_>>();
        self.write_output(&table_line(&rendered_headers, &widths))?;
        self.write_output("\n")?;
        self.write_output(&separator_line(&widths))?;
        self.write_output("\n")?;
        self.result = Some(ResultSet { widths, rows: 0 });
        self.needs_separator = true;
        Ok(())
    }

    fn render_row(&mut self, row: Vec<Option<String>>) -> Result<(), ApplicationError> {
        let Some(result) = self.result.as_ref() else {
            return Err(invalid_event());
        };
        if row.len() != result.widths.len() {
            return Err(invalid_event());
        }
        let widths = result.widths.clone();
        let cells = widths
            .iter()
            .enumerate()
            .map(|(index, width)| match &row[index] {
                Some(value) => escape_terminal_text_to_width(value, *width),
                None => truncate_to_width("NULL", *width),
            })
            .collect::<Vec<_>>();
        let line = table_line(&cells, &widths);
        self.write_output(&line)?;
        self.write_output("\n")?;
        self.result.as_mut().expect("active result").rows += 1;
        Ok(())
    }

    fn complete(&mut self, rows: u64) -> Result<(), ApplicationError> {
        if self
            .result
            .as_ref()
            .is_some_and(|result| result.rows != rows)
        {
            return Err(invalid_event());
        }
        if self.result.take().is_some() {
            self.write_output(&format!("({rows} rows)\n"))?;
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

    fn write_diagnostics(&mut self, value: &str) -> Result<(), ApplicationError> {
        self.write(value, WriteTarget::Diagnostics)
    }

    fn write(&mut self, value: &str, target: WriteTarget) -> Result<(), ApplicationError> {
        if let Some(failed) = self.failed {
            return Err(write_error(failed));
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

impl<Output, Diagnostics> ExecutionSink for TableExecutionSink<Output, Diagnostics>
where
    Output: Write + Send,
    Diagnostics: Write + Send,
{
    fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
        if let Some(failed) = self.failed {
            return Err(write_error(failed));
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
    ApplicationError::runtime("invalid table execution event order")
}

fn column_widths(headers: &[String], display_width: usize) -> Vec<usize> {
    if headers.is_empty() {
        return Vec::new();
    }

    // A one-character column plus one separator per boundary is the smallest
    // representation that retains every (including duplicate) column.
    let available = display_width
        .saturating_sub(headers.len() - 1)
        .max(headers.len());
    let desired: Vec<usize> = headers
        .iter()
        .map(|header| UnicodeWidthStr::width(header.as_str()).max(1))
        .collect();
    let mut widths = vec![1; headers.len()];
    let mut remaining = available - headers.len();

    // First make room for headers. Iterating in source order gives stable
    // results for equal-width and duplicate column names.
    while remaining > 0 {
        let mut grew = false;
        for index in 0..widths.len() {
            if remaining == 0 {
                break;
            }
            if widths[index] < desired[index] {
                widths[index] += 1;
                remaining -= 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    // Extra room is shared deterministically, allowing useful values even
    // when their headers are short (for example, an `id` column).
    for index in (0..widths.len()).cycle().take(remaining) {
        widths[index] += 1;
    }
    widths
}

fn table_line(cells: &[String], widths: &[usize]) -> String {
    cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| pad_to_width(cell, *width))
        .collect::<Vec<_>>()
        .join("|")
}

fn pad_to_width(value: &str, width: usize) -> String {
    let mut padded = value.to_owned();
    padded.push_str(&" ".repeat(width.saturating_sub(UnicodeWidthStr::width(value))));
    padded
}

fn separator_line(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join("+")
}

fn truncate_to_width(value: &str, width: usize) -> String {
    let value_width = UnicodeWidthStr::width(value);
    if value_width <= width {
        return value.to_owned();
    }

    let marker = ".".repeat(width.min(3));
    let content_width = width.saturating_sub(marker.len());
    let mut output = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > content_width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push_str(&marker);
    output
}

fn escape_terminal_text_to_width(value: &str, width: usize) -> String {
    let mut prefix = String::with_capacity(width);
    let mut used = 0;
    let mut truncated = false;

    for character in value.chars() {
        let escaped = match character {
            '\n' => Some("\\n".to_owned()),
            '\r' => Some("\\r".to_owned()),
            '\t' => Some("\\t".to_owned()),
            character if character.is_control() => Some(format!("\\u{{{:04x}}}", character as u32)),
            _ => None,
        };
        let character_width = escaped
            .as_deref()
            .map(UnicodeWidthStr::width)
            .unwrap_or_else(|| UnicodeWidthChar::width(character).unwrap_or(0));
        if used + character_width > width {
            truncated = true;
            break;
        }
        if let Some(escaped) = escaped {
            prefix.push_str(&escaped);
        } else {
            prefix.push(character);
        }
        used += character_width;
    }

    if !truncated {
        return prefix;
    }

    let marker = ".".repeat(width.min(3));
    let content_width = width.saturating_sub(marker.len());
    let mut output = String::with_capacity(width);
    let mut used = 0;
    for character in prefix.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > content_width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push_str(&marker);
    output
}

#[cfg(test)]
mod tests {
    use super::{TableExecutionSink, escape_terminal_text_to_width};
    use crate::app::{ExecutionEvent, ExecutionSink};
    use std::io::{self, Write};

    fn render(events: Vec<ExecutionEvent>, width: usize) -> (String, String) {
        let mut sink = TableExecutionSink::new(Vec::new(), Vec::new(), width);
        for event in events {
            sink.emit(event).expect("event renders");
        }
        let (output, diagnostics) = sink.into_writers();
        (
            String::from_utf8(output).expect("output is utf-8"),
            String::from_utf8(diagnostics).expect("diagnostics are utf-8"),
        )
    }

    #[test]
    fn huge_terminal_cell_is_escaped_only_to_its_display_budget() {
        let value = format!("{}\u{1b}[2J", "x".repeat(8 * 1024 * 1024));
        let rendered = escape_terminal_text_to_width(&value, 12);

        assert_eq!(rendered.len(), 12);
        assert!(rendered.ends_with("..."));
    }

    #[test]
    fn narrow_and_wide_tables_have_bounded_deterministic_widths() {
        let (narrow, _) = render(
            vec![
                ExecutionEvent::Columns(vec!["identifier".into(), "description".into()]),
                ExecutionEvent::Row(vec![Some("abcdef".into()), Some("0123456789".into())]),
                ExecutionEvent::CommandComplete { rows: 1 },
            ],
            9,
        );
        assert_eq!(narrow, "i...|d...\n----+----\na...|0...\n(1 rows)\n");

        let (wide, _) = render(
            vec![
                ExecutionEvent::Columns(vec!["id".into(), "name".into()]),
                ExecutionEvent::Row(vec![Some("7".into()), Some("Ada".into())]),
                ExecutionEvent::CommandComplete { rows: 1 },
            ],
            12,
        );
        assert_eq!(wide, "id   |name  \n-----+------\n7    |Ada   \n(1 rows)\n");
    }

    #[test]
    fn controls_and_unicode_are_safe_and_measured_by_display_width() {
        let (output, diagnostics) = render(
            vec![
                ExecutionEvent::Columns(vec!["値".into(), "値".into()]),
                ExecutionEvent::Row(vec![Some("猫\n\t\u{1b}[2J".into()), Some(String::new())]),
                ExecutionEvent::Notice("watch\r\u{7}".into()),
                ExecutionEvent::CommandComplete { rows: 1 },
                ExecutionEvent::Error {
                    sqlstate: Some("22000".into()),
                    diagnostic: "bad\ninput".into(),
                },
            ],
            13,
        );
        assert_eq!(
            output,
            "値    |値    \n------+------\n猫\\...|      \n(1 rows)\n"
        );
        assert_eq!(
            diagnostics,
            "notice: watch\\r\\u{0007}\nerror [22000]: bad\\ninput\n"
        );
    }

    #[test]
    fn null_empty_duplicate_columns_and_zero_rows_remain_distinct() {
        let (output, _) = render(
            vec![
                ExecutionEvent::Columns(vec!["x".into(), "x".into(), "empty".into()]),
                ExecutionEvent::Row(vec![None, Some(String::new()), Some("z".into())]),
                ExecutionEvent::CommandComplete { rows: 1 },
                ExecutionEvent::Columns(vec!["none".into()]),
                ExecutionEvent::CommandComplete { rows: 0 },
            ],
            20,
        );
        assert_eq!(
            output,
            "x    |x    |empty   \n-----+-----+--------\nNULL |     |z       \n(1 rows)\n\nnone                \n--------------------\n(0 rows)\n"
        );
    }

    #[test]
    fn command_only_and_multiple_results_have_explicit_boundaries() {
        let (output, _) = render(
            vec![
                ExecutionEvent::CommandComplete { rows: 2 },
                ExecutionEvent::Columns(vec!["x".into()]),
                ExecutionEvent::CommandComplete { rows: 0 },
            ],
            5,
        );
        assert_eq!(output, "(2 rows affected)\n\nx    \n-----\n(0 rows)\n");
    }

    #[test]
    fn malformed_event_order_and_row_width_are_rejected() {
        let mut row_before_columns = TableExecutionSink::new(Vec::new(), Vec::new(), 20);
        assert!(
            row_before_columns
                .emit(ExecutionEvent::Row(vec![Some("lost".into())]))
                .is_err()
        );

        let mut duplicate_columns = TableExecutionSink::new(Vec::new(), Vec::new(), 20);
        duplicate_columns
            .emit(ExecutionEvent::Columns(vec!["first".into()]))
            .expect("first columns");
        assert!(
            duplicate_columns
                .emit(ExecutionEvent::Columns(vec!["second".into()]))
                .is_err()
        );

        let mut wrong_width = TableExecutionSink::new(Vec::new(), Vec::new(), 20);
        wrong_width
            .emit(ExecutionEvent::Columns(vec!["one".into(), "two".into()]))
            .expect("columns");
        assert!(
            wrong_width
                .emit(ExecutionEvent::Row(vec![Some("one".into())]))
                .is_err()
        );

        let mut wrong_count = TableExecutionSink::new(Vec::new(), Vec::new(), 20);
        wrong_count
            .emit(ExecutionEvent::Columns(vec!["one".into()]))
            .expect("columns");
        assert!(
            wrong_count
                .emit(ExecutionEvent::CommandComplete { rows: 1 })
                .is_err()
        );
    }

    #[test]
    fn error_is_terminal() {
        let mut sink = TableExecutionSink::new(Vec::new(), Vec::new(), 20);
        sink.emit(ExecutionEvent::Error {
            sqlstate: None,
            diagnostic: "failed".into(),
        })
        .expect("error diagnostic");
        assert!(
            sink.emit(ExecutionEvent::CommandComplete { rows: 0 })
                .is_err()
        );
    }

    struct BrokenWriter;

    impl Write for BrokenWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn broken_output_returns_a_stable_error_and_stops() {
        let mut sink = TableExecutionSink::new(BrokenWriter, Vec::new(), 20);
        let first = sink
            .emit(ExecutionEvent::Columns(vec!["x".into()]))
            .expect_err("closed output fails");
        let second = sink
            .emit(ExecutionEvent::Notice("not written".into()))
            .expect_err("sink remains stopped");
        assert_eq!(first.to_string(), "could not render query output");
        assert_eq!(second.to_string(), "could not render query output");
        assert!(first.is_quiet());
        assert!(second.is_quiet());
    }
}
