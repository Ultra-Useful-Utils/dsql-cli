use crate::{
    app::{ExecutionEvent, ExecutionSink},
    error::ApplicationError,
};
use serde::Serialize;
use std::io::Write;

const OUTPUT_ERROR: &str = "could not write JSON Lines output";
const EVENT_ERROR: &str = "invalid JSON Lines execution event order";

/// A streaming, version-1 framed JSON Lines execution sink.
pub(crate) struct JsonlExecutionSink<W: Write + Send> {
    writer: W,
    next_result: u64,
    active_result: Option<ActiveResult>,
    failed: bool,
}

struct ActiveResult {
    index: u64,
    width: usize,
    rows: u64,
}

impl<W: Write + Send> JsonlExecutionSink<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            writer,
            next_result: 0,
            active_result: None,
            failed: false,
        }
    }

    #[cfg(test)]
    fn into_inner(self) -> W {
        self.writer
    }

    fn write_frame<T: Serialize>(&mut self, frame: &T) -> Result<(), ApplicationError> {
        serde_json::to_writer(&mut self.writer, frame).map_err(|error| {
            output_error(error.io_error_kind() == Some(std::io::ErrorKind::BrokenPipe))
        })?;
        self.writer
            .write_all(b"\n")
            .and_then(|_| self.writer.flush())
            .map_err(|error| output_error(error.kind() == std::io::ErrorKind::BrokenPipe))
    }

    fn invalid_event() -> ApplicationError {
        ApplicationError::runtime(EVENT_ERROR)
    }
}

fn output_error(broken_pipe: bool) -> ApplicationError {
    if broken_pipe {
        ApplicationError::broken_pipe(OUTPUT_ERROR)
    } else {
        ApplicationError::runtime(OUTPUT_ERROR)
    }
}

impl<W: Write + Send> ExecutionSink for JsonlExecutionSink<W> {
    fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
        if self.failed {
            return Err(Self::invalid_event());
        }

        match event {
            ExecutionEvent::Columns(columns) => {
                if self.active_result.is_some() {
                    return Err(Self::invalid_event());
                }
                let result = self.next_result;
                self.write_frame(&ColumnsFrame {
                    version: 1,
                    kind: "columns",
                    result,
                    columns: &columns,
                })?;
                self.next_result += 1;
                self.active_result = Some(ActiveResult {
                    index: result,
                    width: columns.len(),
                    rows: 0,
                });
            }
            ExecutionEvent::Row(values) => {
                let (result, width) = match self.active_result.as_ref() {
                    Some(active) => (active.index, active.width),
                    None => return Err(Self::invalid_event()),
                };
                if values.len() != width {
                    return Err(Self::invalid_event());
                }
                self.write_frame(&RowFrame {
                    version: 1,
                    kind: "row",
                    result,
                    values: &values,
                })?;
                self.active_result
                    .as_mut()
                    .expect("active result checked above")
                    .rows += 1;
            }
            ExecutionEvent::CommandComplete { rows } => match self.active_result.as_ref() {
                Some(active) => {
                    if active.rows != rows {
                        return Err(Self::invalid_event());
                    }
                    let result = active.index;
                    self.write_frame(&CompleteFrame {
                        version: 1,
                        kind: "complete",
                        result,
                        rows,
                    })?;
                    self.active_result = None;
                }
                None => self.write_frame(&CommandFrame {
                    version: 1,
                    kind: "command",
                    rows,
                })?,
            },
            ExecutionEvent::Notice(message) => self.write_frame(&NoticeFrame {
                version: 1,
                kind: "notice",
                message: &message,
            })?,
            ExecutionEvent::Error {
                sqlstate,
                diagnostic,
            } => {
                self.write_frame(&ErrorFrame {
                    version: 1,
                    kind: "error",
                    sqlstate: sqlstate.as_deref(),
                    diagnostic: &diagnostic,
                })?;
                self.failed = true;
            }
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ColumnsFrame<'a> {
    version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    result: u64,
    columns: &'a [String],
}

#[derive(Serialize)]
struct RowFrame<'a> {
    version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    result: u64,
    values: &'a [Option<String>],
}

#[derive(Serialize)]
struct CompleteFrame {
    version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    result: u64,
    rows: u64,
}

#[derive(Serialize)]
struct CommandFrame {
    version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    rows: u64,
}

#[derive(Serialize)]
struct NoticeFrame<'a> {
    version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    message: &'a str,
}

#[derive(Serialize)]
struct ErrorFrame<'a> {
    version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    sqlstate: Option<&'a str>,
    diagnostic: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{self, Write};

    fn frames(sink: JsonlExecutionSink<Vec<u8>>) -> Vec<Value> {
        String::from_utf8(sink.into_inner())
            .expect("JSON is UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON frame"))
            .collect()
    }

    #[test]
    fn preserves_duplicate_columns_null_empty_and_escaped_text() {
        let mut sink = JsonlExecutionSink::new(Vec::new());
        sink.emit(ExecutionEvent::Columns(vec!["name".into(), "name".into()]))
            .expect("columns");
        sink.emit(ExecutionEvent::Row(vec![
            None,
            Some("\nempty? \u{0001} β".into()),
        ]))
        .expect("row");
        sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
            .expect("complete");

        assert_eq!(
            frames(sink),
            vec![
                serde_json::json!({"version": 1, "type": "columns", "result": 0, "columns": ["name", "name"]}),
                serde_json::json!({"version": 1, "type": "row", "result": 0, "values": [null, "\nempty? \u{0001} β"]}),
                serde_json::json!({"version": 1, "type": "complete", "result": 0, "rows": 1}),
            ]
        );
    }

    #[test]
    fn assigns_result_indices_to_multiple_results_and_command_only_frames() {
        let mut sink = JsonlExecutionSink::new(Vec::new());
        sink.emit(ExecutionEvent::CommandComplete { rows: 3 })
            .expect("command");
        for value in ["one", "two"] {
            sink.emit(ExecutionEvent::Columns(vec!["value".into()]))
                .expect("columns");
            sink.emit(ExecutionEvent::Row(vec![Some(value.into())]))
                .expect("row");
            sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
                .expect("complete");
        }

        let frames = frames(sink);
        assert_eq!(
            frames[0],
            serde_json::json!({"version": 1, "type": "command", "rows": 3})
        );
        assert_eq!(frames[1]["result"], 0);
        assert_eq!(frames[3]["result"], 0);
        assert_eq!(frames[4]["result"], 1);
        assert_eq!(frames[6]["result"], 1);
    }

    #[test]
    fn writes_notice_and_final_partial_error_frames() {
        let mut sink = JsonlExecutionSink::new(Vec::new());
        sink.emit(ExecutionEvent::Notice("server\nnotice".into()))
            .expect("notice");
        sink.emit(ExecutionEvent::Columns(vec!["id".into()]))
            .expect("columns");
        sink.emit(ExecutionEvent::Row(vec![Some("1".into())]))
            .expect("row");
        sink.emit(ExecutionEvent::Error {
            sqlstate: None,
            diagnostic: "connection lost".into(),
        })
        .expect("error frame");

        assert_eq!(
            frames(sink),
            vec![
                serde_json::json!({"version": 1, "type": "notice", "message": "server\nnotice"}),
                serde_json::json!({"version": 1, "type": "columns", "result": 0, "columns": ["id"]}),
                serde_json::json!({"version": 1, "type": "row", "result": 0, "values": ["1"]}),
                serde_json::json!({"version": 1, "type": "error", "sqlstate": null, "diagnostic": "connection lost"}),
            ]
        );
    }

    #[test]
    fn rejects_malformed_event_order_and_completion_counts() {
        let mut sink = JsonlExecutionSink::new(Vec::new());
        assert_eq!(
            sink.emit(ExecutionEvent::Row(vec![]))
                .unwrap_err()
                .to_string(),
            EVENT_ERROR
        );
        sink.emit(ExecutionEvent::Columns(vec!["id".into()]))
            .expect("columns");
        assert_eq!(
            sink.emit(ExecutionEvent::Row(vec![
                Some("1".into()),
                Some("2".into())
            ]))
            .unwrap_err()
            .to_string(),
            EVENT_ERROR
        );
        assert_eq!(
            sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
                .unwrap_err()
                .to_string(),
            EVENT_ERROR
        );
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writer_failure_has_a_stable_application_error() {
        let mut sink = JsonlExecutionSink::new(FailingWriter);
        let error = sink
            .emit(ExecutionEvent::Notice("hello".into()))
            .unwrap_err();
        assert_eq!(error.to_string(), OUTPUT_ERROR);
        assert!(error.is_quiet());
    }

    #[test]
    fn parser_can_ignore_future_unknown_frames() {
        let input = concat!(
            "{\"version\":1,\"type\":\"columns\",\"result\":0,\"columns\":[\"id\"]}\n",
            "{\"version\":1,\"type\":\"progress\",\"percent\":50}\n",
            "{\"version\":1,\"type\":\"complete\",\"result\":0,\"rows\":0}\n",
        );
        let known: Vec<Value> = input
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON frame"))
            .filter(|frame: &Value| {
                matches!(
                    frame["type"].as_str(),
                    Some("columns" | "row" | "complete" | "command" | "notice" | "error")
                )
            })
            .collect();

        assert_eq!(known.len(), 2);
        assert_eq!(known[0]["type"], "columns");
        assert_eq!(known[1]["type"], "complete");
    }
}
