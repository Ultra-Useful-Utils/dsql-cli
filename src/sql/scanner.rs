//! Lexical SQL statement framing.
//!
//! This deliberately recognizes only the PostgreSQL lexical constructs needed
//! to find statement terminators. It does not validate or otherwise interpret
//! SQL, so server-specific syntax passes through unchanged.

/// The lexical state of input that has not yet been terminated by a semicolon.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Completeness {
    /// No string, identifier, or comment is open. The input may still lack a
    /// statement terminator, which batch callers can distinguish with
    /// [`StatementStream::pending_is_trivia`].
    Complete,
    SingleQuotedString,
    EscapeString,
    DoubleQuotedIdentifier,
    DollarQuotedString,
    BlockComment,
}

impl Completeness {
    /// A concise, user-facing description suitable for a continuation prompt.
    #[cfg(test)]
    pub(crate) const fn description(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::SingleQuotedString => "single-quoted string",
            Self::EscapeString => "escape string",
            Self::DoubleQuotedIdentifier => "double-quoted identifier",
            Self::DollarQuotedString => "dollar-quoted string",
            Self::BlockComment => "block comment",
        }
    }
}

/// Transaction-control statements relevant to session state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransactionControl {
    Begin,
    Commit,
    Rollback,
    Savepoint,
    Release,
    RollbackTo,
    Other,
}

/// An exact, semicolon-terminated slice of submitted SQL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Statement {
    text: String,
    transaction_control: TransactionControl,
}

impl Statement {
    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn into_text(self) -> String {
        self.text
    }

    pub(crate) fn transaction_control(&self) -> TransactionControl {
        self.transaction_control
    }
}

/// Incrementally frames semicolon-terminated SQL statements.
///
/// Bytes are retained verbatim until a terminator is found, allowing callers to
/// concatenate emitted statements and the remaining suffix to recover exactly the
/// submitted input. Re-scanning the pending suffix on each push keeps lexical
/// behavior correct when a construct is split across input chunks.
pub(crate) const MAX_STATEMENT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub(crate) struct StatementStream {
    pending: String,
}

impl StatementStream {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn push(&mut self, input: &str) -> Vec<Statement> {
        self.push_inner(input, None)
            .expect("unbounded test framing cannot exceed a limit")
    }

    pub(crate) fn push_bounded(
        &mut self,
        input: &str,
        max_statement_bytes: usize,
    ) -> Result<Vec<Statement>, ()> {
        self.push_inner(input, Some(max_statement_bytes))
    }

    fn push_inner(
        &mut self,
        input: &str,
        max_statement_bytes: Option<usize>,
    ) -> Result<Vec<Statement>, ()> {
        self.pending.push_str(input);

        let mut statements = Vec::new();
        let mut start = 0;
        for end in terminated_statement_ends(&self.pending) {
            if max_statement_bytes.is_some_and(|limit| end - start > limit) {
                return Err(());
            }
            let text = self.pending[start..end].to_owned();
            let transaction_control = classify_transaction_control(&text);
            statements.push(Statement {
                text,
                transaction_control,
            });
            start = end;
        }
        if start != 0 {
            self.pending.drain(..start);
        }
        if max_statement_bytes.is_some_and(|limit| self.pending.len() > limit) {
            return Err(());
        }
        Ok(statements)
    }

    /// Returns input that has not yet reached a lexical statement terminator.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> &str {
        &self.pending
    }

    /// Returns the current lexical state of pending input.
    pub(crate) fn completeness(&self) -> Completeness {
        lexical_completeness(&self.pending)
    }

    /// Whether the unterminated suffix contains only whitespace and complete
    /// comments. This lets batch callers accept harmless trailing input while
    /// still rejecting a final statement that lacks its terminator.
    pub(crate) fn pending_is_trivia(&self) -> bool {
        self.completeness() == Completeness::Complete
            && skip_trivia(self.pending.as_bytes(), 0) == self.pending.len()
    }

    pub(crate) fn take_complete_statement(&mut self) -> Option<Statement> {
        if self.completeness() != Completeness::Complete || self.pending_is_trivia() {
            return None;
        }
        let text = std::mem::take(&mut self.pending);
        let transaction_control = classify_transaction_control(&text);
        Some(Statement {
            text,
            transaction_control,
        })
    }
}

/// Classifies only the leading transaction-control keywords of a statement.
/// Unknown SQL is intentionally classified as [`TransactionControl::Other`].
pub(crate) fn classify_transaction_control(statement: &str) -> TransactionControl {
    let keywords = leading_keywords(statement, 3);
    let is = |index: usize, keyword: &str| {
        keywords
            .get(index)
            .is_some_and(|word| word.eq_ignore_ascii_case(keyword))
    };

    if is(0, "BEGIN") || (is(0, "START") && is(1, "TRANSACTION")) {
        TransactionControl::Begin
    } else if is(0, "COMMIT") || is(0, "END") {
        TransactionControl::Commit
    } else if (is(0, "ROLLBACK") || is(0, "ABORT")) && is(1, "TO") {
        TransactionControl::RollbackTo
    } else if is(0, "ROLLBACK") || is(0, "ABORT") {
        TransactionControl::Rollback
    } else if is(0, "SAVEPOINT") {
        TransactionControl::Savepoint
    } else if is(0, "RELEASE") {
        TransactionControl::Release
    } else {
        TransactionControl::Other
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LexicalState {
    Normal,
    SingleQuotedString,
    EscapeString,
    DoubleQuotedIdentifier,
    DollarQuotedString(String),
    LineComment,
    BlockComment(usize),
}

fn terminated_statement_ends(input: &str) -> Vec<usize> {
    let mut ends = Vec::new();
    scan(input, |index| ends.push(index));
    ends
}

fn lexical_completeness(input: &str) -> Completeness {
    match scan(input, |_| {}) {
        LexicalState::Normal | LexicalState::LineComment => Completeness::Complete,
        LexicalState::SingleQuotedString => Completeness::SingleQuotedString,
        LexicalState::EscapeString => Completeness::EscapeString,
        LexicalState::DoubleQuotedIdentifier => Completeness::DoubleQuotedIdentifier,
        LexicalState::DollarQuotedString(_) => Completeness::DollarQuotedString,
        LexicalState::BlockComment(_) => Completeness::BlockComment,
    }
}

/// Scans an entire UTF-8 string and calls `terminated` with each byte index
/// immediately after a semicolon outside lexical constructs.
fn scan(input: &str, mut terminated: impl FnMut(usize)) -> LexicalState {
    let bytes = input.as_bytes();
    let mut index = 0;
    let mut state = LexicalState::Normal;

    while index < bytes.len() {
        match &mut state {
            LexicalState::Normal => match bytes[index] {
                b'\'' => {
                    state = if is_escape_string_prefix(bytes, index) {
                        LexicalState::EscapeString
                    } else {
                        LexicalState::SingleQuotedString
                    };
                    index += 1;
                }
                b'"' => {
                    state = LexicalState::DoubleQuotedIdentifier;
                    index += 1;
                }
                b'-' if bytes.get(index + 1) == Some(&b'-') => {
                    state = LexicalState::LineComment;
                    index += 2;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    state = LexicalState::BlockComment(1);
                    index += 2;
                }
                b'$' => {
                    if let Some(delimiter) = dollar_quote_delimiter(input, index) {
                        index += delimiter.len();
                        state = LexicalState::DollarQuotedString(delimiter.to_owned());
                    } else {
                        index += 1;
                    }
                }
                b';' => {
                    index += 1;
                    terminated(index);
                }
                _ => index += 1,
            },
            LexicalState::SingleQuotedString => {
                if bytes[index] == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                    } else {
                        state = LexicalState::Normal;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            LexicalState::EscapeString => {
                if bytes[index] == b'\\' {
                    index += usize::from(index + 1 < bytes.len()) + 1;
                } else if bytes[index] == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                    } else {
                        state = LexicalState::Normal;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            LexicalState::DoubleQuotedIdentifier => {
                if bytes[index] == b'"' {
                    if bytes.get(index + 1) == Some(&b'"') {
                        index += 2;
                    } else {
                        state = LexicalState::Normal;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            LexicalState::DollarQuotedString(delimiter) => {
                if bytes[index..].starts_with(delimiter.as_bytes()) {
                    index += delimiter.len();
                    state = LexicalState::Normal;
                } else {
                    index += 1;
                }
            }
            LexicalState::LineComment => {
                if matches!(bytes[index], b'\n' | b'\r') {
                    state = LexicalState::Normal;
                }
                index += 1;
            }
            LexicalState::BlockComment(depth) => {
                if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                    *depth += 1;
                    index += 2;
                } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    *depth -= 1;
                    index += 2;
                    if *depth == 0 {
                        state = LexicalState::Normal;
                    }
                } else {
                    index += 1;
                }
            }
        }
    }
    state
}

fn is_escape_string_prefix(bytes: &[u8], quote_index: usize) -> bool {
    matches!(bytes.get(quote_index.wrapping_sub(1)), Some(b'e' | b'E'))
        && (quote_index == 1 || !is_identifier_continue(bytes[quote_index - 2]))
}

fn dollar_quote_delimiter(input: &str, start: usize) -> Option<&str> {
    let bytes = input.as_bytes();
    debug_assert_eq!(bytes[start], b'$');
    if start != 0 && is_identifier_continue(bytes[start - 1]) {
        return None;
    }
    let next = *bytes.get(start + 1)?;
    if next == b'$' {
        return Some(&input[start..start + 2]);
    }
    if !is_identifier_start(next) {
        return None;
    }

    let mut end = start + 2;
    while let Some(&byte) = bytes.get(end) {
        if byte == b'$' {
            return Some(&input[start..=end]);
        }
        if !is_identifier_continue(byte) {
            return None;
        }
        end += 1;
    }
    None
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit() || byte == b'$'
}

pub(crate) fn leading_keywords(input: &str, limit: usize) -> Vec<&str> {
    let bytes = input.as_bytes();
    let mut keywords = Vec::with_capacity(limit);
    let mut index = 0;

    while keywords.len() < limit {
        index = skip_trivia(bytes, index);
        let start = index;
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_alphabetic())
        {
            index += 1;
        }
        if start == index {
            break;
        }
        keywords.push(&input[start..index]);
    }

    keywords
}

fn skip_trivia(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if bytes.get(index..index + 2) == Some(b"--") {
            index += 2;
            while !matches!(bytes.get(index), None | Some(b'\n' | b'\r')) {
                index += 1;
            }
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"/*") {
            index += 2;
            let mut depth = 1;
            while depth > 0 {
                match bytes.get(index..index + 2) {
                    Some(b"/*") => {
                        depth += 1;
                        index += 2;
                    }
                    Some(b"*/") => {
                        depth -= 1;
                        index += 2;
                    }
                    Some(_) => index += 1,
                    None => return index,
                }
            }
            continue;
        }
        return index;
    }
}

#[cfg(test)]
mod tests {
    use super::{Completeness, StatementStream, TransactionControl};

    fn statements(input: &str) -> Vec<String> {
        let mut stream = StatementStream::new();
        stream
            .push(input)
            .into_iter()
            .map(|statement| statement.into_text())
            .collect()
    }

    #[test]
    fn splits_semicolons_only_in_normal_sql() {
        assert_eq!(
            statements("SELECT 1; SELECT 2;"),
            ["SELECT 1;", " SELECT 2;"]
        );
    }

    #[test]
    fn preserves_semicolons_inside_lexical_constructs() {
        let input =
            "SELECT ';', E'\\\\;\\\'x', \"a;b\", $$d;e$$; -- f;\n/* g; /* h; */ */ SELECT 2;";
        assert_eq!(
            statements(input),
            [
                "SELECT ';', E'\\\\;\\\'x', \"a;b\", $$d;e$$;",
                " -- f;\n/* g; /* h; */ */ SELECT 2;",
            ]
        );
    }

    #[test]
    fn recognizes_legal_dollar_quote_tags_but_not_illegal_ones() {
        assert_eq!(
            statements("SELECT $tag_9$;$tag_9$; SELECT $; x;"),
            ["SELECT $tag_9$;$tag_9$;", " SELECT $;", " x;"]
        );
    }

    #[test]
    fn does_not_treat_a_dollar_tag_inside_an_identifier_as_a_quote() {
        assert_eq!(
            statements("SELECT column$tag$; still_sql;"),
            ["SELECT column$tag$;", " still_sql;"]
        );
    }

    #[test]
    fn handles_unicode_inside_dollar_quoted_strings() {
        assert_eq!(
            statements("SELECT $tag$λ;漢字$tag$; SELECT 2;"),
            ["SELECT $tag$λ;漢字$tag$;", " SELECT 2;"]
        );
    }

    #[test]
    fn handles_nested_block_comments() {
        assert_eq!(
            statements("/* outer; /* inner; */ still outer; */ SELECT 1;"),
            ["/* outer; /* inner; */ still outer; */ SELECT 1;"]
        );
    }

    #[test]
    fn reports_incomplete_lexical_states_without_emitting_them() {
        let cases = [
            ("SELECT '", "single-quoted string"),
            ("SELECT E'\\\\", "escape string"),
            ("SELECT \"name", "double-quoted identifier"),
            ("SELECT $tag$body", "dollar-quoted string"),
            ("SELECT 1 /* nested", "block comment"),
        ];

        for (input, expected) in cases {
            let mut stream = StatementStream::new();
            assert!(stream.push(input).is_empty());
            assert_eq!(stream.completeness().description(), expected, "{input}");
            assert_eq!(stream.pending(), input);
        }
    }

    #[test]
    fn distinguishes_trailing_trivia_from_an_unterminated_statement() {
        let mut stream = StatementStream::new();
        stream.push("SELECT 1; -- finished\n/* also finished */");
        assert!(stream.pending_is_trivia());

        let mut stream = StatementStream::new();
        stream.push("SELECT 1");
        assert!(!stream.pending_is_trivia());
    }

    #[test]
    fn standard_and_escape_strings_have_distinct_backslash_rules() {
        assert_eq!(
            statements("SELECT '\\'; SELECT 1;"),
            ["SELECT '\\';", " SELECT 1;"]
        );
        assert_eq!(
            statements("SELECT E'\\\'; still string;'; SELECT 1;"),
            ["SELECT E'\\\'; still string;';", " SELECT 1;"]
        );
    }

    #[test]
    fn classifies_leading_transaction_control_after_comments() {
        let cases = [
            (" BEGIN;", TransactionControl::Begin),
            ("/* lead */ START TRANSACTION;", TransactionControl::Begin),
            ("COMMIT;", TransactionControl::Commit),
            ("END;", TransactionControl::Commit),
            ("ROLLBACK;", TransactionControl::Rollback),
            ("ABORT;", TransactionControl::Rollback),
            ("SAVEPOINT one;", TransactionControl::Savepoint),
            ("RELEASE SAVEPOINT one;", TransactionControl::Release),
            ("ROLLBACK TO SAVEPOINT one;", TransactionControl::RollbackTo),
            ("ROLLBACK TO one;", TransactionControl::RollbackTo),
            ("START;", TransactionControl::Other),
            ("AWS IAM GRANT role;", TransactionControl::Other),
        ];

        for (input, expected) in cases {
            let mut stream = StatementStream::new();
            let statements = stream.push(input);
            assert_eq!(statements.len(), 1, "{input}");
            assert_eq!(statements[0].transaction_control(), expected, "{input}");
        }
    }

    #[test]
    fn classifies_case_insensitive_transaction_keywords() {
        let mut stream = StatementStream::new();
        let statements = stream.push("-- comment\nstart transaction; rollback to savepoint work;");
        assert_eq!(
            statements[0].transaction_control(),
            TransactionControl::Begin
        );
        assert_eq!(
            statements[1].transaction_control(),
            TransactionControl::RollbackTo
        );
    }

    #[test]
    fn property_corpus_preserves_concatenation_and_never_splits_inside_constructs() {
        let corpus = [
            "",
            "SELECT 1",
            "SELECT 1;",
            "SELECT ';';SELECT 2;",
            "SELECT E'one\\\';two;'; SELECT 2;",
            "SELECT \"semi;colon\"; SELECT 2;",
            "SELECT $$semi;colon$$; SELECT 2;",
            "SELECT $tag_9$semi;colon$tag_9$; SELECT 2;",
            "-- semi;\nSELECT 1;",
            "/* outer; /* inner; */ outer; */ SELECT 1;",
            "SELECT 'unterminated;",
            "SELECT E'unterminated\\\\;",
            "SELECT \"unterminated;",
            "SELECT $tag$unterminated;",
            "SELECT 1 /* unterminated;",
            "AWS IAM GRANT foo; COMMIT;",
        ];

        for input in corpus {
            for chunks in chunkings(input) {
                let mut stream = StatementStream::new();
                let mut output = String::new();
                for chunk in chunks {
                    for statement in stream.push(chunk) {
                        output.push_str(statement.text());
                    }
                }
                output.push_str(stream.pending());
                assert_eq!(output, input, "input={input:?}");
            }
        }
    }

    #[test]
    fn bounded_stream_limits_each_statement_not_the_total_input() {
        let mut stream = StatementStream::new();
        assert_eq!(
            stream
                .push_bounded("SELECT 1;SELECT 2;", 9)
                .expect("each statement fits")
                .len(),
            2
        );

        let mut stream = StatementStream::new();
        assert!(
            stream
                .push_bounded("SELECT ", 9)
                .expect("prefix fits")
                .is_empty()
        );
        assert!(stream.push_bounded("12;", 9).is_err());
    }

    #[test]
    fn complete_unterminated_suffix_can_be_taken_for_command_input() {
        let mut stream = StatementStream::new();
        let statements = stream
            .push_bounded("SELECT 1; SELECT 2", 64)
            .expect("input fits");

        assert_eq!(statements.len(), 1);
        assert_eq!(
            stream
                .take_complete_statement()
                .expect("complete suffix")
                .text(),
            " SELECT 2"
        );
        assert!(stream.pending_is_trivia());
    }

    fn chunkings(input: &str) -> Vec<Vec<&str>> {
        let mut chunkings = vec![vec![input]];
        for (index, _) in input.char_indices().skip(1) {
            chunkings.push(vec![&input[..index], &input[index..]]);
        }
        chunkings
    }

    #[test]
    fn complete_state_is_exposed_for_unterminated_but_lexically_closed_sql() {
        let mut stream = StatementStream::new();
        assert!(stream.push("SELECT unknown_dsql_syntax").is_empty());
        assert_eq!(stream.completeness(), Completeness::Complete);
    }
}
