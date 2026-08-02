pub(crate) mod delimited;
#[allow(dead_code)] // SH-004 infrastructure is intentionally not wired into shell settings yet.
pub(crate) mod expanded;
pub(crate) mod jsonl;
#[allow(dead_code)] // SH-004 infrastructure is intentionally not wired into shell settings yet.
pub(crate) mod pager;
pub(crate) mod table;
pub(crate) mod timing;

#[cfg(test)]
mod stress;

pub(crate) fn escape_terminal_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{{{:04x}}}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped
}
