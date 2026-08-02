#![allow(dead_code)] // Foundation contract used by discovery and connection milestones.

use std::{error::Error, fmt};

pub(crate) const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;
const MAX_ERROR_CHAIN_DEPTH: usize = 8;
const TRUNCATION_MARKER: &str = "[truncated]";

/// The process result class. Keep this crate-private so command-line behavior
/// can remain stable without making the error model a public API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ErrorCategory {
    Runtime,
    Usage,
    Interrupted,
}

impl ErrorCategory {
    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            Self::Runtime => 1,
            Self::Usage => 2,
            Self::Interrupted => 130,
        }
    }
}

/// A user-facing application diagnostic with an optionally chained, sanitized
/// source. Raw external errors must be converted here rather than retained.
#[derive(Debug)]
pub(crate) struct ApplicationError {
    category: ErrorCategory,
    diagnostic: String,
    source: Option<SanitizedSource>,
    quiet: bool,
}

impl ApplicationError {
    pub(crate) fn runtime(diagnostic: impl Into<String>) -> Self {
        Self::new(ErrorCategory::Runtime, diagnostic)
    }

    pub(crate) fn usage(diagnostic: impl Into<String>) -> Self {
        Self::new(ErrorCategory::Usage, diagnostic)
    }

    pub(crate) fn interrupted() -> Self {
        Self::new(ErrorCategory::Interrupted, "interrupted")
    }

    pub(crate) fn broken_pipe(diagnostic: impl Into<String>) -> Self {
        let mut error = Self::runtime(diagnostic);
        error.quiet = true;
        error
    }

    pub(crate) fn category(&self) -> ErrorCategory {
        self.category
    }

    pub(crate) const fn exit_code(&self) -> i32 {
        self.category.exit_code()
    }

    pub(crate) const fn is_quiet(&self) -> bool {
        self.quiet
    }

    pub(crate) fn with_source(mut self, source: impl Error + 'static) -> Self {
        self.source = Some(SanitizedSource::from_error(&source));
        self
    }

    fn new(category: ErrorCategory, diagnostic: impl Into<String>) -> Self {
        Self {
            category,
            diagnostic: sanitize_diagnostic(&diagnostic.into()),
            source: None,
            quiet: false,
        }
    }
}

impl fmt::Display for ApplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.diagnostic)?;
        if let Some(source) = &self.source {
            write!(formatter, ": {source}")?;
        }
        Ok(())
    }
}

impl Error for ApplicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

pub(crate) fn dsql_connection_failure(
    sqlstate: Option<&str>,
    diagnostic: &str,
) -> ApplicationError {
    let normalized = diagnostic.to_ascii_lowercase();
    let sqlstate = sqlstate
        .map(|value| format!(" ({value})"))
        .unwrap_or_default();

    if normalized.contains("wrong user to action mapping") {
        ApplicationError::runtime(format!(
            "Aurora DSQL token type does not match the database role{sqlstate}; admin requires DbConnectAdmin and custom roles require DbConnect"
        ))
    } else if normalized.contains("iam authentication failed") {
        ApplicationError::runtime(format!(
            "Aurora DSQL IAM authentication failed for the database role{sqlstate}; refresh AWS credentials and generate a new authentication token"
        ))
    } else if normalized.contains("role ") && normalized.contains(" does not exist") {
        ApplicationError::runtime(format!(
            "Aurora DSQL database role is not authorized for this IAM principal{sqlstate}; create the database role and its IAM mapping"
        ))
    } else if normalized.contains("tls")
        || normalized.contains("ssl")
        || normalized.contains("certificate")
    {
        ApplicationError::runtime(format!(
            "could not verify the Aurora DSQL TLS connection{sqlstate}; verify the cluster endpoint and trusted root certificates"
        ))
    } else {
        ApplicationError::runtime(format!(
            "could not establish a secure Aurora DSQL connection{sqlstate}"
        ))
    }
}

pub(crate) fn dsql_database_failure(sqlstate: &str, diagnostic: &str) -> ApplicationError {
    let normalized = diagnostic.to_ascii_lowercase();
    match sqlstate {
        "40001" if normalized.contains("oc000") => ApplicationError::runtime(
            "database statement failed (40001, OC000); transaction conflicted with another transaction; retry the transaction explicitly",
        ),
        "40001" if normalized.contains("oc001") => ApplicationError::runtime(
            "database statement failed (40001, OC001); schema changed concurrently; retry the transaction explicitly",
        ),
        "40001" => ApplicationError::runtime(
            "database statement failed (40001); Aurora DSQL concurrency conflict; retry the transaction explicitly",
        ),
        "53300" => ApplicationError::runtime(
            "Aurora DSQL connection limit reached (53300); close idle connections or wait before reconnecting",
        ),
        "53400" => ApplicationError::runtime(
            "Aurora DSQL resource limit exceeded (53400); reduce the statement or transaction size",
        ),
        _ => ApplicationError::runtime(format!("database statement failed ({sqlstate})")),
    }
}

pub(crate) fn redact_diagnostic(diagnostic: &str) -> String {
    sanitize_diagnostic(diagnostic)
}

pub(crate) fn sanitize_terminal_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        if character == '\n' || character == '\t' || !character.is_control() {
            output.push(character);
        } else {
            use std::fmt::Write;
            let _ = write!(output, "\\u{{{:04x}}}", character as u32);
        }
    }
    output
}

pub(crate) fn bounded_error_chain_text(error: &(dyn Error + 'static)) -> String {
    use std::fmt::Write as _;

    let mut output = BoundedDiagnostic::default();
    let mut current = Some(error);
    let mut depth = 0;
    while let Some(error) = current {
        if depth > 0 {
            let _ = output.write_str(": ");
        }
        let _ = write!(output, "{error}");
        current = error.source();
        depth += 1;
        if depth > MAX_ERROR_CHAIN_DEPTH {
            if current.is_some() {
                output.truncated = true;
            }
            break;
        }
    }
    output.finish()
}

#[derive(Default)]
struct BoundedDiagnostic {
    value: String,
    truncated: bool,
}

impl BoundedDiagnostic {
    fn finish(mut self) -> String {
        if self.truncated {
            self.value.push_str(TRUNCATION_MARKER);
        }
        self.value
    }
}

impl fmt::Write for BoundedDiagnostic {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let content_limit = MAX_DIAGNOSTIC_BYTES.saturating_sub(TRUNCATION_MARKER.len());
        let remaining = content_limit.saturating_sub(self.value.len());
        if value.len() <= remaining {
            self.value.push_str(value);
        } else {
            let end = floor_char_boundary(value, remaining);
            self.value.push_str(&value[..end]);
            self.truncated = true;
        }
        Ok(())
    }
}

/// An owned copy of an external error's diagnostic. Its chain contains only
/// further owned, redacted diagnostics; no unsafe external error is retained.
#[derive(Debug)]
struct SanitizedSource {
    diagnostic: String,
    source: Option<Box<SanitizedSource>>,
}

impl SanitizedSource {
    fn from_error(error: &(dyn Error + 'static)) -> Self {
        Self::from_error_at_depth(error, 0)
    }

    fn from_error_at_depth(error: &(dyn Error + 'static), depth: usize) -> Self {
        let source = if depth >= MAX_ERROR_CHAIN_DEPTH {
            error.source().map(|_| {
                Box::new(Self {
                    diagnostic: "[error chain truncated]".into(),
                    source: None,
                })
            })
        } else {
            error
                .source()
                .map(|source| Box::new(Self::from_error_at_depth(source, depth + 1)))
        };
        Self {
            diagnostic: sanitize_diagnostic(&error.to_string()),
            source,
        }
    }
}

impl fmt::Display for SanitizedSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.diagnostic)?;
        if let Some(source) = &self.source {
            write!(formatter, ": {source}")?;
        }
        Ok(())
    }
}

impl Error for SanitizedSource {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

const REDACTED: &str = "[REDACTED]";

fn sanitize_diagnostic(input: &str) -> String {
    let input_end = floor_char_boundary(input, input.len().min(MAX_DIAGNOSTIC_BYTES));
    let input_was_truncated = input_end < input.len();
    let sanitized = sanitize_terminal_text(&redact(&input[..input_end]));
    if !input_was_truncated && sanitized.len() <= MAX_DIAGNOSTIC_BYTES {
        return sanitized;
    }

    let content_limit = MAX_DIAGNOSTIC_BYTES.saturating_sub(TRUNCATION_MARKER.len());
    let content_end = floor_char_boundary(&sanitized, sanitized.len().min(content_limit));
    let mut bounded = sanitized[..content_end].to_owned();
    bounded.push_str(TRUNCATION_MARKER);
    bounded
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn redact(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut copied_until = 0;
    let mut index = 0;

    while index < bytes.len() {
        if !is_key_boundary(bytes, index) {
            index += 1;
            continue;
        }

        let key_start = index;
        let mut separator = index;
        while separator < bytes.len() && is_key_byte(bytes[separator]) {
            separator += 1;
        }
        let key_end = separator;
        if index > 0
            && matches!(bytes[index - 1], b'\'' | b'"')
            && bytes.get(separator) == Some(&bytes[index - 1])
        {
            separator += 1;
            while matches!(bytes.get(separator), Some(b' ' | b'\t')) {
                separator += 1;
            }
        }
        if separator == key_start
            || separator == bytes.len()
            || !matches!(bytes[separator], b'=' | b':')
        {
            index += 1;
            continue;
        }

        let key = &input[key_start..key_end];
        if !is_secret_key(key) {
            index = separator + 1;
            continue;
        }

        let value_start = separator + 1;
        let quoted_value_end = quoted_secret_value_end(bytes, value_start);
        let value_end = if bytes[separator] == b':' {
            quoted_value_end.unwrap_or_else(|| header_value_end(bytes, value_start))
        } else {
            quoted_value_end.unwrap_or_else(|| {
                if starts_quoted_secret_value(bytes, value_start) {
                    bytes.len()
                } else {
                    secret_value_end(bytes, value_start)
                }
            })
        };
        output.push_str(&input[copied_until..value_start]);
        output.push_str(REDACTED);
        copied_until = value_end;
        index = value_end;
    }

    output.push_str(&input[copied_until..]);
    output
}

fn is_key_boundary(bytes: &[u8], index: usize) -> bool {
    index == 0
        || matches!(
            bytes[index - 1],
            b'?' | b'&'
                | b';'
                | b','
                | b' '
                | b'\t'
                | b'\n'
                | b'\r'
                | b'{'
                | b'['
                | b'('
                | b'\''
                | b'"'
        )
}

fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'%')
}

fn secret_value_end(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len()
        && !matches!(
            bytes[index],
            b'&' | b';' | b',' | b' ' | b'\t' | b'\n' | b'\r'
        )
    {
        index += 1;
    }
    index
}

fn quoted_secret_value_end(bytes: &[u8], value_start: usize) -> Option<usize> {
    let mut quote_start = value_start;
    while matches!(bytes.get(quote_start), Some(b' ' | b'\t')) {
        quote_start += 1;
    }

    let quote = *bytes.get(quote_start)?;
    if !matches!(quote, b'\'' | b'\"') {
        return None;
    }

    let mut index = quote_start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            byte if byte == quote => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

fn starts_quoted_secret_value(bytes: &[u8], mut value_start: usize) -> bool {
    while matches!(bytes.get(value_start), Some(b' ' | b'\t')) {
        value_start += 1;
    }
    matches!(bytes.get(value_start), Some(b'\'' | b'\"'))
}

fn header_value_end(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
        index += 1;
    }
    index
}

fn is_secret_key(key: &str) -> bool {
    let key = percent_decode(key).to_ascii_lowercase();
    key.starts_with("x-amz-")
        || matches!(
            key.as_str(),
            "access_token"
                | "accesskey"
                | "access_key"
                | "access_key_id"
                | "api_key"
                | "aws_access_key_id"
                | "aws_secret_access_key"
                | "aws_session_token"
                | "apikey"
                | "authorization"
                | "client_secret"
                | "credential"
                | "credentials"
                | "id_token"
                | "password"
                | "passwd"
                | "pwd"
                | "secret"
                | "secret_access_key"
                | "security_token"
                | "signature"
                | "sig"
                | "token"
        )
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push(high * 16 + low);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApplicationError, ErrorCategory, bounded_error_chain_text, dsql_connection_failure,
        dsql_database_failure,
    };
    use std::{error::Error, fmt};

    const TOKEN: &str = "AQoDYXdzEJr...very-secret-session-token";
    const SIGNATURE: &str = "0123456789abcdef0123456789abcdef";

    #[derive(Debug)]
    struct ChainedError {
        diagnostic: String,
        source: Option<Box<dyn Error + Send + Sync>>,
    }

    impl fmt::Display for ChainedError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(&self.diagnostic)
        }
    }

    impl Error for ChainedError {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.source
                .as_deref()
                .map(|source| source as &(dyn Error + 'static))
        }
    }

    #[test]
    fn runtime_diagnostic_redacts_top_level_secrets_in_display_and_debug() {
        let error = ApplicationError::runtime(format!(
            "authentication failed: token={TOKEN}; password=hunter2; Authorization: Bearer bearer-secret"
        ));

        let display = error.to_string();
        let debug = format!("{error:?}");

        for rendered in [&display, &debug] {
            assert!(!rendered.contains(TOKEN));
            assert!(!rendered.contains("hunter2"));
            assert!(!rendered.contains("bearer-secret"));
            assert!(rendered.contains("authentication failed"));
            assert!(rendered.contains("[REDACTED]"));
        }
        assert_eq!(
            display,
            "authentication failed: token=[REDACTED]; password=[REDACTED]; Authorization:[REDACTED]"
        );
    }

    #[test]
    fn source_chain_is_owned_and_redacts_nested_secrets() {
        let error = ApplicationError::runtime("could not connect").with_source(ChainedError {
            diagnostic: "request failed while connecting to cluster".into(),
            source: Some(Box::new(ChainedError {
                diagnostic: format!(
                    "https://example.test/connect?X-Amz-Credential=AKIA%2Fcredential&X-Amz-Signature={SIGNATURE}&X-Amz-Security-Token={TOKEN}"
                ),
                source: None,
            })),
        });

        let source = error.source().expect("sanitized source").to_string();
        let display = error.to_string();
        let debug = format!("{error:?}");

        for rendered in [&source, &display, &debug] {
            assert!(!rendered.contains("AKIA%2Fcredential"));
            assert!(!rendered.contains(SIGNATURE));
            assert!(!rendered.contains(TOKEN));
            assert!(rendered.contains("request failed"));
            assert!(rendered.contains("X-Amz-Credential=[REDACTED]"));
        }
        assert_eq!(
            display,
            "could not connect: request failed while connecting to cluster: https://example.test/connect?X-Amz-Credential=[REDACTED]&X-Amz-Signature=[REDACTED]&X-Amz-Security-Token=[REDACTED]"
        );
    }

    #[test]
    fn source_chain_redacts_quoted_secrets_without_retaining_raw_errors() {
        let secret = r#"nested secret;,& with \"escaped\" quote/$+?%"#;
        let error = ApplicationError::runtime("could not connect").with_source(ChainedError {
            diagnostic: "request failed".into(),
            source: Some(Box::new(ChainedError {
                diagnostic: format!("credential='{secret}' retryable=true"),
                source: None,
            })),
        });

        let source = error.source().expect("sanitized source").to_string();
        let display = error.to_string();
        let debug = format!("{error:?}");

        for rendered in [&source, &display, &debug] {
            assert!(!rendered.contains(secret));
            assert!(rendered.contains("credential=[REDACTED] retryable=true"));
        }
    }

    #[test]
    fn signed_url_query_parameters_are_redacted_with_encoded_values() {
        let error = ApplicationError::runtime(
            "request failed: https://example.test/?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIA%2F20260724%2Fus-east-1&X-Amz-Date=20260724T000000Z&X-Amz-Expires=900&X-Amz-SignedHeaders=host&X-Amz-Signature=deadbeef&X-Amz-Security-Token=encoded%2Btoken%3D",
        );

        let rendered = error.to_string();

        assert!(rendered.contains("https://example.test/?"));
        assert!(rendered.contains("X-Amz-Algorithm=[REDACTED]"));
        assert!(rendered.contains("X-Amz-Signature=[REDACTED]"));
        assert!(rendered.contains("X-Amz-Security-Token=[REDACTED]"));
        assert!(!rendered.contains("AWS4-HMAC-SHA256"));
        assert!(!rendered.contains("AKIA%2F20260724"));
        assert!(!rendered.contains("deadbeef"));
        assert!(!rendered.contains("encoded%2Btoken%3D"));
    }

    #[test]
    fn encoded_generic_query_secret_is_redacted_without_changing_ordinary_parameters() {
        let error = ApplicationError::runtime(
            "retry https://example.test/?attempt=3&token=token%2Bwith%2Fencoded%3Dcharacters&region=us-east-1",
        );

        assert_eq!(
            error.to_string(),
            "retry https://example.test/?attempt=3&token=[REDACTED]&region=us-east-1"
        );
    }

    #[test]
    fn quoted_assignment_values_consume_whitespace_delimiters_and_escaped_quotes() {
        let double_quoted_secret = r#"double value;,& with \"escaped\" quote=and:colon/$+?%"#;
        let single_quoted_secret = r#"single value;,& with \'escaped\' quote=and:colon/$+?%"#;
        let error = ApplicationError::runtime(format!(
            "token=\"{double_quoted_secret}\" password='{single_quoted_secret}' retryable=true"
        ));

        let rendered = error.to_string();

        assert!(!rendered.contains(double_quoted_secret));
        assert!(!rendered.contains(single_quoted_secret));
        assert_eq!(
            rendered,
            "token=[REDACTED] password=[REDACTED] retryable=true"
        );
    }

    #[test]
    fn quoted_colon_values_preserve_following_ordinary_context() {
        let secret = r#"Bearer value;,& with \"escaped\" credentials/$+?%"#;
        let error =
            ApplicationError::runtime(format!("Authorization: \"{secret}\" request_id=abc123"));

        let rendered = error.to_string();

        assert!(!rendered.contains(secret));
        assert_eq!(rendered, "Authorization:[REDACTED] request_id=abc123");
    }

    #[test]
    fn unclosed_quoted_values_are_redacted_through_the_remaining_diagnostic() {
        let secret_suffix = "secret value;,& including a suffix";
        let error = ApplicationError::runtime(format!("token=\"{secret_suffix}"));

        let rendered = error.to_string();

        assert!(!rendered.contains(secret_suffix));
        assert_eq!(rendered, "token=[REDACTED]");
    }

    #[test]
    fn aws_credential_environment_names_are_redacted() {
        let error = ApplicationError::runtime(
            "AWS_ACCESS_KEY_ID=synthetic-access-key AWS_SECRET_ACCESS_KEY=secret-access-key AWS_SESSION_TOKEN=session-token",
        );

        let rendered = error.to_string();

        for secret in ["synthetic-access-key", "secret-access-key", "session-token"] {
            assert!(!rendered.contains(secret));
        }
        assert_eq!(
            rendered,
            "AWS_ACCESS_KEY_ID=[REDACTED] AWS_SECRET_ACCESS_KEY=[REDACTED] AWS_SESSION_TOKEN=[REDACTED]"
        );
    }

    #[test]
    fn ordinary_diagnostic_context_is_preserved() {
        let error = ApplicationError::runtime(
            "TLS connection to cluster endpoint failed after 3 seconds: certificate expired",
        );

        assert_eq!(
            error.to_string(),
            "TLS connection to cluster endpoint failed after 3 seconds: certificate expired"
        );
    }

    #[test]
    fn dsql_occ_diagnostics_preserve_sqlstate_and_explain_the_conflict() {
        assert_eq!(
            dsql_database_failure("40001", "change conflicts with another transaction (OC000)")
                .to_string(),
            "database statement failed (40001, OC000); transaction conflicted with another transaction; retry the transaction explicitly"
        );
        assert_eq!(
            dsql_database_failure(
                "40001",
                "schema has been updated by another transaction (OC001)"
            )
            .to_string(),
            "database statement failed (40001, OC001); schema changed concurrently; retry the transaction explicitly"
        );
    }

    #[test]
    fn dsql_limit_diagnostics_are_actionable_and_preserve_sqlstate() {
        assert_eq!(
            dsql_database_failure("53300", "too many connections").to_string(),
            "Aurora DSQL connection limit reached (53300); close idle connections or wait before reconnecting"
        );
        assert_eq!(
            dsql_database_failure("53400", "transaction is too large").to_string(),
            "Aurora DSQL resource limit exceeded (53400); reduce the statement or transaction size"
        );
    }

    #[test]
    fn connection_diagnostics_cover_iam_token_mapping_database_role_and_tls_failures() {
        assert_eq!(
            dsql_connection_failure(
                Some("28P01"),
                "IAM authentication failed for user \"application\" token=secret-token"
            )
            .to_string(),
            "Aurora DSQL IAM authentication failed for the database role (28P01); refresh AWS credentials and generate a new authentication token"
        );
        assert_eq!(
            dsql_connection_failure(
                Some("28P01"),
                "Wrong user to action mapping. user: admin, action: DbConnect"
            )
            .to_string(),
            "Aurora DSQL token type does not match the database role (28P01); admin requires DbConnectAdmin and custom roles require DbConnect"
        );
        assert_eq!(
            dsql_connection_failure(Some("28000"), "Role application does not exist").to_string(),
            "Aurora DSQL database role is not authorized for this IAM principal (28000); create the database role and its IAM mapping"
        );
        assert_eq!(
            dsql_connection_failure(Some("08006"), "TLS error: certificate verify failed")
                .to_string(),
            "could not verify the Aurora DSQL TLS connection (08006); verify the cluster endpoint and trusted root certificates"
        );
    }

    #[test]
    fn classified_connection_diagnostics_do_not_echo_external_secrets() {
        let rendered = dsql_connection_failure(
            Some("28P01"),
            "IAM authentication failed token=secret-token password=hunter2",
        )
        .to_string();

        assert!(!rendered.contains("secret-token"));
        assert!(!rendered.contains("hunter2"));
    }

    #[test]
    fn structured_and_debug_style_secrets_are_redacted() {
        let error = ApplicationError::runtime(
            r#"request failed: {"token":"json-secret"} Credentials { access_key_id: "AKIA-SECRET" }"#,
        );

        let rendered = error.to_string();
        assert!(!rendered.contains("json-secret"));
        assert!(!rendered.contains("AKIA-SECRET"));
        assert!(rendered.contains(r#""token":[REDACTED]"#));
        assert!(rendered.contains("access_key_id:[REDACTED]"));
    }

    #[test]
    fn application_errors_escape_terminal_controls_at_the_boundary() {
        assert_eq!(
            ApplicationError::runtime("failed\u{1b}[2J\nnext\u{7}").to_string(),
            "failed\\u{001b}[2J\nnext\\u{0007}"
        );
    }

    #[test]
    fn application_diagnostics_have_a_fixed_rendered_byte_limit() {
        let error = ApplicationError::runtime(format!(
            "failure token=secret-token {}",
            "x".repeat(super::MAX_DIAGNOSTIC_BYTES * 2)
        ));
        let rendered = error.to_string();

        assert!(rendered.len() <= super::MAX_DIAGNOSTIC_BYTES);
        assert!(!rendered.contains("secret-token"));
        assert!(rendered.ends_with("[truncated]"));
    }

    #[test]
    fn external_error_chains_have_a_fixed_depth_limit() {
        let mut source: Option<Box<dyn Error + Send + Sync>> = None;
        for _ in 0..32 {
            source = Some(Box::new(ChainedError {
                diagnostic: "nested failure".into(),
                source,
            }));
        }
        let error = ApplicationError::runtime("request failed").with_source(ChainedError {
            diagnostic: "outer failure".into(),
            source,
        });
        let rendered = error.to_string();

        assert!(rendered.contains("error chain truncated"));
        assert!(rendered.matches("nested failure").count() <= super::MAX_ERROR_CHAIN_DEPTH);
    }

    #[test]
    fn temporary_error_chain_classification_text_is_bounded() {
        let error = ChainedError {
            diagnostic: "x".repeat(super::MAX_DIAGNOSTIC_BYTES * 2),
            source: None,
        };

        assert!(bounded_error_chain_text(&error).len() <= super::MAX_DIAGNOSTIC_BYTES);
    }

    #[test]
    fn error_categories_have_stable_exit_codes() {
        assert_eq!(ApplicationError::runtime("network failed").exit_code(), 1);
        assert_eq!(ApplicationError::usage("missing Region").exit_code(), 2);
        assert_eq!(ApplicationError::interrupted().exit_code(), 130);
        assert_eq!(
            ApplicationError::usage("missing Region").category(),
            ErrorCategory::Usage
        );
        assert!(!ApplicationError::runtime("network failed").is_quiet());
        let broken_pipe = ApplicationError::broken_pipe("output closed");
        assert!(broken_pipe.is_quiet());
        assert_eq!(broken_pipe.exit_code(), 1);
    }
}
