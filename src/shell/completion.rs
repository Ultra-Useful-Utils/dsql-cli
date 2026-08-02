use crate::app::MetadataSnapshot;
use reedline::{Completer, Span, Suggestion};
use std::{
    collections::BTreeSet,
    sync::{Arc, RwLock},
};

const MAX_SUGGESTIONS: usize = 100;

const LOCAL_WORDS: &[&str] = &[
    "ABORT",
    "ALTER",
    "ANALYZE",
    "BEGIN",
    "COMMIT",
    "CREATE",
    "DELETE",
    "DROP",
    "END",
    "EXPLAIN",
    "FROM",
    "GRANT",
    "GROUP",
    "INSERT",
    "INTO",
    "JOIN",
    "LIMIT",
    "ORDER",
    "ROLLBACK",
    "SELECT",
    "SET",
    "SHOW",
    "START",
    "TRUNCATE",
    "UPDATE",
    "VALUES",
    "WHERE",
    "WITH",
    "\\?",
    "\\conninfo",
    "\\d",
    "\\dn",
    "\\dt",
    "\\du",
    "\\pager",
    "\\q",
    "\\refresh",
    "\\timing",
    "\\x",
];

pub(crate) type SharedCompletionSnapshot = Arc<RwLock<MetadataSnapshot>>;

pub(crate) struct SqlCompleter {
    snapshot: SharedCompletionSnapshot,
}

impl SqlCompleter {
    pub(crate) fn new(snapshot: SharedCompletionSnapshot) -> Self {
        Self { snapshot }
    }
}

impl Completer for SqlCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let pos = pos.min(line.len());
        if !line.is_char_boundary(pos) {
            return Vec::new();
        }
        let start = line[..pos]
            .char_indices()
            .rev()
            .find_map(|(index, character)| {
                (!token_character(character)).then_some(index + character.len_utf8())
            })
            .unwrap_or(0);
        let prefix = &line[start..pos];
        let local_values: BTreeSet<_> = LOCAL_WORDS.iter().map(|word| (*word).to_owned()).collect();
        let mut catalog_values = BTreeSet::new();

        // Completion runs on Reedline's input path. A contended reload must not
        // delay a keypress; local words remain available until the next try_read.
        if let Ok(snapshot) = self.snapshot.try_read() {
            catalog_values.extend(snapshot.schemas().iter().cloned());
            for relation in snapshot.relations() {
                catalog_values.insert(relation.relation().to_owned());
                catalog_values.insert(format!("{}.{}", relation.schema(), relation.relation()));
            }
            catalog_values.extend(
                snapshot
                    .columns()
                    .iter()
                    .map(|column| column.column().to_owned()),
            );
            catalog_values.extend(snapshot.roles().iter().map(|role| role.name().to_owned()));
        }
        catalog_values.retain(|value| !value.chars().any(char::is_control));

        let mut values: Vec<_> = local_values
            .iter()
            .filter(|value| {
                value
                    .get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
            })
            .cloned()
            .collect();
        values.extend(catalog_values.into_iter().filter(|value| {
            !local_values.contains(value)
                && value
                    .get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        }));
        values.truncate(MAX_SUGGESTIONS);
        values
            .into_iter()
            .map(|value| Suggestion {
                value,
                span: Span::new(start, pos),
                append_whitespace: false,
                ..Suggestion::default()
            })
            .collect()
    }
}

fn token_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '.' | '\\')
}

#[cfg(test)]
mod tests {
    use super::{MAX_SUGGESTIONS, SqlCompleter};
    use crate::app::{ColumnName, DatabaseRole, MetadataSnapshot, RelationName};
    use reedline::Completer;
    use std::sync::{Arc, RwLock};

    fn snapshot() -> Arc<RwLock<MetadataSnapshot>> {
        Arc::new(RwLock::new(MetadataSnapshot::empty()))
    }

    #[test]
    fn completes_local_keywords_before_catalog_is_loaded() {
        let mut completer = SqlCompleter::new(snapshot());
        let suggestions = completer.complete("sel", 3);

        assert!(
            suggestions
                .iter()
                .any(|suggestion| suggestion.value == "SELECT")
        );
        assert!(
            suggestions
                .iter()
                .all(|suggestion| suggestion.value != "orders")
        );
        assert_eq!(suggestions[0].span.start, 0);
        assert_eq!(suggestions[0].span.end, 3);
    }

    #[test]
    fn observes_catalog_updates_and_matches_prefixes_case_insensitively() {
        let snapshot = snapshot();
        *snapshot.write().expect("snapshot lock") = MetadataSnapshot::new(
            vec!["public".into()],
            vec![RelationName::new("public", "orders")],
            vec![ColumnName::new("public", "orders", "order_id")],
            vec![DatabaseRole::Custom("reporter".into())],
            None,
            false,
        );
        let mut completer = SqlCompleter::new(snapshot);
        let suggestions = completer.complete("SELECT ORD", 10);
        let values: Vec<_> = suggestions
            .iter()
            .map(|suggestion| suggestion.value.as_str())
            .collect();

        assert!(values.contains(&"orders"));
        assert!(values.contains(&"order_id"));
        assert_eq!(suggestions[0].span.start, 7);
        assert_eq!(suggestions[0].span.end, 10);

        let all = completer.complete("", 0);
        let all_values: Vec<_> = all
            .iter()
            .map(|suggestion| suggestion.value.as_str())
            .collect();
        assert!(all_values.contains(&"public"));
        assert!(all_values.contains(&"public.orders"));
        assert!(all_values.contains(&"reporter"));
    }

    #[test]
    fn catalog_controls_are_never_offered_to_the_terminal_editor() {
        let snapshot = Arc::new(RwLock::new(MetadataSnapshot::new(
            vec!["safe".into(), "bad\u{1b}]8;;url\u{7}".into()],
            vec![RelationName::new("public", "bad\u{9b}name")],
            Vec::new(),
            Vec::new(),
            None,
            false,
        )));
        let mut completer = SqlCompleter::new(snapshot);

        let suggestions = completer.complete("", 0);
        assert!(
            suggestions
                .iter()
                .any(|suggestion| suggestion.value == "safe")
        );
        assert!(
            suggestions
                .iter()
                .all(|suggestion| { !suggestion.value.chars().any(char::is_control) })
        );
    }

    #[test]
    fn invalidated_snapshot_hides_catalog_values_but_keeps_local_words() {
        let snapshot = snapshot();
        *snapshot.write().expect("snapshot lock") = MetadataSnapshot::new(
            vec!["public".into()],
            vec![RelationName::new("public", "orders")],
            vec![ColumnName::new("public", "orders", "order_id")],
            vec![DatabaseRole::Custom("reporter".into())],
            None,
            false,
        );
        snapshot.write().expect("snapshot lock").invalidate();
        let mut completer = SqlCompleter::new(snapshot);

        assert!(completer.complete("order_i", 7).is_empty());
        assert!(
            completer
                .complete("sel", 3)
                .iter()
                .any(|suggestion| suggestion.value == "SELECT")
        );
    }

    #[test]
    fn bounds_suggestions_and_never_blocks_on_snapshot_writer() {
        let snapshot = snapshot();
        *snapshot.write().expect("snapshot lock") = MetadataSnapshot::new(
            (0..200).map(|index| format!("item_{index}")).collect(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            false,
        );
        let mut completer = SqlCompleter::new(snapshot.clone());
        let all_suggestions = completer.complete("", 0);
        assert!(all_suggestions.len() <= MAX_SUGGESTIONS);
        assert!(
            all_suggestions
                .iter()
                .any(|suggestion| suggestion.value == "SELECT")
        );

        let writer = snapshot.write().expect("snapshot lock");
        let suggestions = completer.complete("SEL", 3);
        drop(writer);
        assert!(
            suggestions
                .iter()
                .any(|suggestion| suggestion.value == "SELECT")
        );
        assert!(
            suggestions
                .iter()
                .all(|suggestion| suggestion.value != "item_0")
        );
    }
}
