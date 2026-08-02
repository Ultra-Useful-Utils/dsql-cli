use super::{
    delimited::DelimitedSink, expanded::ExpandedExecutionSink, jsonl::JsonlExecutionSink,
    table::TableExecutionSink,
};
use crate::app::{ExecutionEvent, ExecutionSink};
use std::{io, sync::Arc};

const ROWS: u64 = 1_000_000;

#[derive(Clone, Default)]
struct CountingWriter(Arc<std::sync::Mutex<WriteStats>>);

#[derive(Default)]
struct WriteStats {
    bytes: u64,
    largest_write: usize,
}

impl CountingWriter {
    fn stats(&self) -> (u64, usize) {
        let stats = self.0.lock().expect("writer stats");
        (stats.bytes, stats.largest_write)
    }
}

impl io::Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut stats = self.0.lock().expect("writer stats");
        stats.bytes += buffer.len() as u64;
        stats.largest_write = stats.largest_write.max(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn stream_rows(sink: &mut dyn ExecutionSink) {
    sink.emit(ExecutionEvent::Columns(vec!["value".into()]))
        .expect("columns");
    for _ in 0..ROWS {
        sink.emit(ExecutionEvent::Row(vec![Some("x".into())]))
            .expect("row");
    }
    sink.emit(ExecutionEvent::CommandComplete { rows: ROWS })
        .expect("complete");
}

#[test]
#[ignore = "one-million-row streaming acceptance fixture"]
fn one_million_rows_stream_in_every_output_format() {
    let diagnostics = CountingWriter::default();
    let outputs = [
        {
            let output = CountingWriter::default();
            stream_rows(&mut TableExecutionSink::new(
                output.clone(),
                diagnostics.clone(),
                80,
            ));
            output
        },
        {
            let output = CountingWriter::default();
            stream_rows(&mut ExpandedExecutionSink::new(
                output.clone(),
                diagnostics.clone(),
            ));
            output
        },
        {
            let output = CountingWriter::default();
            stream_rows(&mut DelimitedSink::csv(output.clone(), diagnostics.clone()));
            output
        },
        {
            let output = CountingWriter::default();
            stream_rows(&mut DelimitedSink::tsv(output.clone(), diagnostics.clone()));
            output
        },
        {
            let output = CountingWriter::default();
            stream_rows(&mut JsonlExecutionSink::new(output.clone()));
            output
        },
    ];

    for output in outputs {
        let (bytes, largest_write) = output.stats();
        assert!(bytes >= ROWS, "every row reached the writer");
        assert!(largest_write <= 64 * 1024, "writes remain bounded");
    }
}

#[test]
fn large_fields_stream_in_machine_output_formats() {
    for format in ["csv", "tsv", "jsonl"] {
        let output = CountingWriter::default();
        let diagnostics = CountingWriter::default();
        let mut sink: Box<dyn ExecutionSink> = match format {
            "csv" => Box::new(DelimitedSink::csv(output.clone(), diagnostics.clone())),
            "tsv" => Box::new(DelimitedSink::tsv(output.clone(), diagnostics.clone())),
            "jsonl" => Box::new(JsonlExecutionSink::new(output.clone())),
            _ => unreachable!(),
        };
        sink.emit(ExecutionEvent::Columns(vec!["value".into()]))
            .expect("columns");
        sink.emit(ExecutionEvent::Row(vec![Some("x".repeat(8 * 1024 * 1024))]))
            .expect("large row");
        sink.emit(ExecutionEvent::CommandComplete { rows: 1 })
            .expect("complete");

        assert!(output.stats().0 > 8 * 1024 * 1024);
    }
}
