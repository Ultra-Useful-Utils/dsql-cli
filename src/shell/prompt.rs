use crate::{app::TransactionState, output::escape_terminal_text};
use reedline::{Prompt, PromptEditMode, PromptHistorySearch};
use std::borrow::Cow;

pub(crate) struct ShellPrompt {
    left: String,
}

impl ShellPrompt {
    pub(crate) fn new(cluster_id: &str, database_role: &str, state: TransactionState) -> Self {
        let marker = match state {
            TransactionState::Idle => "=",
            TransactionState::Active => "=*",
            TransactionState::Failed => "=!",
            TransactionState::Uncertain => "=?",
        };
        Self {
            left: format!(
                "{}/{}{marker}> ",
                escape_terminal_text(cluster_id),
                escape_terminal_text(database_role)
            ),
        }
    }

    #[cfg(test)]
    fn render(&self) -> &str {
        &self.left
    }
}

impl Prompt for ShellPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.left)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("...> ")
    }

    fn render_prompt_history_search_indicator(&self, _: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("history search: ")
    }
}

#[cfg(test)]
mod tests {
    use super::ShellPrompt;
    use crate::app::TransactionState;

    #[test]
    fn prompt_identifies_cluster_role_and_transaction_state() {
        let cases = [
            (TransactionState::Idle, "cluster-1/app_user=> "),
            (TransactionState::Active, "cluster-1/app_user=*> "),
            (TransactionState::Failed, "cluster-1/app_user=!> "),
            (TransactionState::Uncertain, "cluster-1/app_user=?> "),
        ];

        for (state, expected) in cases {
            assert_eq!(
                ShellPrompt::new("cluster-1", "app_user", state).render(),
                expected
            );
        }
    }

    #[test]
    fn prompt_escapes_terminal_controls_in_connection_metadata() {
        assert_eq!(
            ShellPrompt::new(
                "cluster-1",
                "role\u{1b}]0;owned\u{7}",
                TransactionState::Idle
            )
            .render(),
            "cluster-1/role\\u{001b}]0;owned\\u{0007}=> "
        );
    }
}
