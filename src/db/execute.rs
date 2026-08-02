use crate::{
    app::{ExecutionEvent, ExecutionSink},
    error::ApplicationError,
};

#[derive(Debug, Eq, PartialEq)]
pub(super) enum StreamEvent {
    Columns(Vec<String>),
    Row(Vec<Option<String>>),
    CommandComplete(u64),
}

pub(super) fn emit_stream_events(
    events: impl IntoIterator<Item = StreamEvent>,
    sink: &mut dyn ExecutionSink,
) -> Result<(), ApplicationError> {
    for event in events {
        sink.emit(match event {
            StreamEvent::Columns(columns) => ExecutionEvent::Columns(columns),
            StreamEvent::Row(row) => ExecutionEvent::Row(row),
            StreamEvent::CommandComplete(rows) => ExecutionEvent::CommandComplete { rows },
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{StreamEvent, emit_stream_events};
    use crate::{
        app::{ExecutionEvent, ExecutionSink},
        error::ApplicationError,
    };

    #[test]
    fn events_are_emitted_sequentially_without_collecting() {
        struct Sink(Vec<ExecutionEvent>);
        impl ExecutionSink for Sink {
            fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
                self.0.push(event);
                Ok(())
            }
        }

        let mut sink = Sink(Vec::new());
        emit_stream_events(
            vec![
                StreamEvent::Columns(vec!["id".into()]),
                StreamEvent::Row(vec![Some("1".into())]),
                StreamEvent::CommandComplete(1),
            ],
            &mut sink,
        )
        .expect("events emit");

        assert_eq!(
            sink.0,
            vec![
                ExecutionEvent::Columns(vec!["id".into()]),
                ExecutionEvent::Row(vec![Some("1".into())]),
                ExecutionEvent::CommandComplete { rows: 1 },
            ]
        );
    }

    #[test]
    fn zero_row_results_still_emit_columns_and_completion() {
        struct Sink(Vec<ExecutionEvent>);
        impl ExecutionSink for Sink {
            fn emit(&mut self, event: ExecutionEvent) -> Result<(), ApplicationError> {
                self.0.push(event);
                Ok(())
            }
        }

        let mut sink = Sink(Vec::new());
        emit_stream_events(
            vec![
                StreamEvent::Columns(vec!["Name".into()]),
                StreamEvent::CommandComplete(0),
            ],
            &mut sink,
        )
        .expect("empty result emits metadata");

        assert_eq!(
            sink.0,
            vec![
                ExecutionEvent::Columns(vec!["Name".into()]),
                ExecutionEvent::CommandComplete { rows: 0 },
            ]
        );
    }

    #[test]
    fn sink_failure_stops_before_the_next_stream_message() {
        struct FailingSink;
        impl ExecutionSink for FailingSink {
            fn emit(&mut self, _: ExecutionEvent) -> Result<(), ApplicationError> {
                Err(ApplicationError::runtime("output closed"))
            }
        }
        struct Events {
            next_calls: usize,
        }
        impl Iterator for Events {
            type Item = StreamEvent;

            fn next(&mut self) -> Option<Self::Item> {
                self.next_calls += 1;
                match self.next_calls {
                    1 => Some(StreamEvent::Columns(vec!["id".into()])),
                    2 => panic!("stream was read after sink failure"),
                    _ => None,
                }
            }
        }

        let mut sink = FailingSink;
        assert!(emit_stream_events(Events { next_calls: 0 }, &mut sink).is_err());
    }
}
