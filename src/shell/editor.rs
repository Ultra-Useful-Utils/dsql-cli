use crate::{
    shell::completion::{SharedCompletionSnapshot, SqlCompleter},
    sql::scanner::{MAX_STATEMENT_BYTES, StatementStream},
};
use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, DefaultHinter, Emacs, FileBackedHistory, Highlighter, History, HistoryItem,
    HistoryItemId, HistorySessionId, KeyCode, KeyModifiers, MenuBuilder, Reedline, ReedlineEvent,
    ReedlineMenu, SearchQuery, StyledText, ValidationResult, Validator, default_emacs_keybindings,
};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

const HISTORY_CAPACITY: usize = 1_000;
const MAX_HISTORY_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_HISTORY_ENTRY_BYTES: usize = MAX_HISTORY_FILE_BYTES / HISTORY_CAPACITY;

pub(crate) struct SqlValidator;

struct SqlHighlighter;

impl Highlighter for SqlHighlighter {
    fn highlight(&self, line: &str, _: usize) -> StyledText {
        let mut highlighted = StyledText::new();
        let word_start = line.len() - line.trim_start().len();
        let word_end = line[word_start..]
            .find(|character: char| !character.is_ascii_alphabetic() && character != '\\')
            .map_or(line.len(), |offset| word_start + offset);
        let word = &line[word_start..word_end];
        let is_keyword = word.starts_with('\\')
            || [
                "ABORT",
                "ALTER",
                "BEGIN",
                "COMMIT",
                "CREATE",
                "DELETE",
                "DROP",
                "END",
                "EXPLAIN",
                "GRANT",
                "INSERT",
                "RELEASE",
                "REVOKE",
                "ROLLBACK",
                "SAVEPOINT",
                "SELECT",
                "SET",
                "SHOW",
                "START",
                "TRUNCATE",
                "UPDATE",
                "WITH",
            ]
            .iter()
            .any(|keyword| word.eq_ignore_ascii_case(keyword));

        highlighted.push((Style::default(), line[..word_start].to_owned()));
        highlighted.push((
            if is_keyword {
                Color::Cyan.bold()
            } else {
                Style::default()
            },
            word.to_owned(),
        ));
        highlighted.push((Style::default(), line[word_end..].to_owned()));
        highlighted
    }
}

impl Validator for SqlValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if line.trim().is_empty() {
            return ValidationResult::Complete;
        }
        if line.len() > MAX_STATEMENT_BYTES {
            return ValidationResult::Complete;
        }
        if line.starts_with('\\') && !line.contains(['\n', '\r']) {
            return ValidationResult::Complete;
        }
        let mut stream = StatementStream::new();
        if stream.push_bounded(line, MAX_STATEMENT_BYTES).is_err() {
            return ValidationResult::Complete;
        }
        if stream.pending_is_trivia() {
            ValidationResult::Complete
        } else {
            ValidationResult::Incomplete
        }
    }
}

pub(crate) fn build_editor(
    no_history: bool,
    explicit_history_file: Option<PathBuf>,
    snapshot: SharedCompletionSnapshot,
) -> Reedline {
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    let editor = Reedline::create()
        .with_validator(Box::new(SqlValidator))
        .with_hinter(Box::new(DefaultHinter::default()))
        .with_highlighter(Box::new(SqlHighlighter))
        .with_completer(Box::new(SqlCompleter::new(snapshot)))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("completion_menu"),
        )))
        .with_edit_mode(Box::new(Emacs::new(keybindings)))
        .with_history_exclusion_prefix(Some(" ".into()));
    if no_history {
        return editor;
    }

    let path = history_path(
        explicit_history_file.as_deref(),
        env::var_os("XDG_DATA_HOME").as_deref(),
    );
    match secure_history(&path) {
        Ok(history) => editor.with_history(Box::new(history)),
        Err(_) => {
            eprintln!("warning: interactive shell history is unavailable for this session");
            editor
        }
    }
}

pub(crate) fn history_path(
    explicit: Option<&Path>,
    data_home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| match data_home {
            Some(path) => PathBuf::from(path).join("dsql/history"),
            None => env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/share/dsql/history"),
        })
}

fn secure_history(path: &Path) -> std::io::Result<DescriptorBoundHistory> {
    let file = prepare_history_file(path)?;
    if file.metadata()?.len() > MAX_HISTORY_FILE_BYTES as u64 {
        return Err(std::io::Error::other("history file is too large"));
    }
    #[cfg(target_os = "linux")]
    let descriptor_path = PathBuf::from(format!("/proc/self/fd/{}", descriptor(&file)));
    #[cfg(all(unix, not(target_os = "linux")))]
    let descriptor_path = PathBuf::from(format!("/dev/fd/{}", descriptor(&file)));
    #[cfg(not(unix))]
    let descriptor_path = path.to_path_buf();
    let inner = FileBackedHistory::with_file(HISTORY_CAPACITY, descriptor_path)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(DescriptorBoundHistory {
        inner,
        file,
        path: path.to_path_buf(),
    })
}

fn prepare_history_file(path: &Path) -> std::io::Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("history path is not a regular file"));
    }
    set_owner_only_permissions(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn descriptor(file: &fs::File) -> std::os::fd::RawFd {
    use std::os::fd::AsRawFd;

    file.as_raw_fd()
}

struct DescriptorBoundHistory {
    inner: FileBackedHistory,
    file: fs::File,
    path: PathBuf,
}

impl DescriptorBoundHistory {
    #[cfg(unix)]
    fn verify_path_identity(&self) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;

        let current = fs::symlink_metadata(&self.path)?;
        let opened = self.file.metadata()?;
        if !current.is_file() || current.dev() != opened.dev() || current.ino() != opened.ino() {
            return Err(std::io::Error::other("history path was replaced"));
        }
        Ok(())
    }
}

impl History for DescriptorBoundHistory {
    fn save(&mut self, mut item: HistoryItem) -> reedline::Result<HistoryItem> {
        if item.command_line.len() > MAX_HISTORY_ENTRY_BYTES {
            item.id = None;
            return Ok(item);
        }
        self.inner.save(item)
    }

    fn load(&self, id: HistoryItemId) -> reedline::Result<HistoryItem> {
        self.inner.load(id)
    }

    fn count(&self, query: SearchQuery) -> reedline::Result<i64> {
        self.inner.count(query)
    }

    fn search(&self, query: SearchQuery) -> reedline::Result<Vec<HistoryItem>> {
        self.inner.search(query)
    }

    fn update(
        &mut self,
        id: HistoryItemId,
        updater: &dyn Fn(HistoryItem) -> HistoryItem,
    ) -> reedline::Result<()> {
        self.inner.update(id, updater)
    }

    fn clear(&mut self) -> reedline::Result<()> {
        #[cfg(unix)]
        {
            self.verify_path_identity()
                .map_err(reedline::ReedlineError::from)?;
            let _ = self.inner.clear();
            self.file.set_len(0).map_err(reedline::ReedlineError::from)
        }
        #[cfg(not(unix))]
        {
            self.inner.clear()
        }
    }

    fn delete(&mut self, id: HistoryItemId) -> reedline::Result<()> {
        self.inner.delete(id)
    }

    fn sync(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        self.verify_path_identity()?;
        self.inner.sync()
    }

    fn session(&self) -> Option<HistorySessionId> {
        self.inner.session()
    }
}

#[cfg(unix)]
fn set_owner_only_permissions(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_: &fs::File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SqlValidator, build_editor, history_path, secure_history};
    use crate::{app::MetadataSnapshot, sql::scanner::MAX_STATEMENT_BYTES};
    use reedline::{History, HistoryItem, ValidationResult, Validator};
    use std::{
        ffi::OsStr,
        fs,
        path::PathBuf,
        sync::{Arc, RwLock},
        time::SystemTime,
    };

    #[test]
    fn validator_requires_a_statement_terminator_but_preserves_scanner_lexical_rules() {
        let validator = SqlValidator;
        assert!(matches!(
            validator.validate("SELECT 1"),
            ValidationResult::Incomplete
        ));
        assert!(matches!(
            validator.validate("SELECT ';';"),
            ValidationResult::Complete
        ));
        assert!(matches!(
            validator.validate("SELECT $tag$semi;colon$tag$;"),
            ValidationResult::Complete
        ));
        assert!(matches!(
            validator.validate("SELECT 'unterminated"),
            ValidationResult::Incomplete
        ));
    }

    #[test]
    fn validator_submits_a_single_line_meta_command_without_a_semicolon() {
        let validator = SqlValidator;
        assert!(matches!(
            validator.validate("\\conninfo"),
            ValidationResult::Complete
        ));
        assert!(matches!(
            validator.validate(" \\conninfo"),
            ValidationResult::Incomplete
        ));
        assert!(matches!(
            validator.validate("\\conninfo\nSELECT 1"),
            ValidationResult::Incomplete
        ));
    }

    #[test]
    fn validator_submits_an_oversized_unterminated_buffer_for_bounded_rejection() {
        let validator = SqlValidator;
        let input = "x".repeat(MAX_STATEMENT_BYTES + 1);

        assert!(matches!(
            validator.validate(&input),
            ValidationResult::Complete
        ));
    }

    #[test]
    fn history_path_uses_explicit_path_or_xdg_data_home() {
        assert_eq!(
            history_path(
                Some(PathBuf::from("/tmp/alternate-history").as_path()),
                Some(OsStr::new("/data"))
            ),
            PathBuf::from("/tmp/alternate-history")
        );
        assert_eq!(
            history_path(None, Some(OsStr::new("/data"))),
            PathBuf::from("/data/dsql/history")
        );
    }

    #[test]
    fn disabled_history_does_not_create_the_configured_file() {
        let path = temporary_history_path();
        let _editor = build_editor(
            true,
            Some(path.clone()),
            Arc::new(RwLock::new(MetadataSnapshot::empty())),
        );
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn persisted_history_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let path = temporary_history_path();
        let _editor = build_editor(
            false,
            Some(path.clone()),
            Arc::new(RwLock::new(MetadataSnapshot::empty())),
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("history metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_file(path).expect("remove history file");
    }

    #[cfg(unix)]
    #[test]
    fn history_rejects_symlinks_without_modifying_the_target() {
        use std::os::unix::fs::symlink;

        let target = temporary_history_path();
        let link = temporary_history_path();
        fs::write(&target, "keep").expect("target");
        symlink(&target, &link).expect("symlink");

        let _editor = build_editor(
            false,
            Some(link.clone()),
            Arc::new(RwLock::new(MetadataSnapshot::empty())),
        );

        assert_eq!(fs::read_to_string(&target).expect("target text"), "keep");
        fs::remove_file(link).expect("remove link");
        fs::remove_file(target).expect("remove target");
    }

    #[cfg(unix)]
    #[test]
    fn history_writes_remain_bound_to_the_verified_file_after_path_replacement() {
        let path = temporary_history_path();
        let original = temporary_history_path();
        let mut history = secure_history(&path).expect("secure history");
        fs::rename(&path, &original).expect("move verified file");
        fs::write(&path, "attacker-controlled\n").expect("replacement file");

        history
            .save(HistoryItem::from_command_line("SELECT secret;"))
            .expect("save history");
        history
            .sync()
            .expect_err("replaced history path must stop persistence");

        assert_eq!(
            fs::read_to_string(&path).expect("replacement text"),
            "attacker-controlled\n"
        );
        assert_eq!(fs::read_to_string(&original).expect("verified text"), "");
        fs::remove_file(path).expect("remove replacement");
        fs::remove_file(original).expect("remove verified file");
    }

    #[test]
    fn oversized_existing_history_is_rejected_before_reedline_loads_it() {
        let path = temporary_history_path();
        let file = fs::File::create(&path).expect("history file");
        file.set_len(super::MAX_HISTORY_FILE_BYTES as u64 + 1)
            .expect("oversized history");

        assert!(secure_history(&path).is_err());

        fs::remove_file(path).expect("remove history file");
    }

    #[test]
    fn oversized_submissions_are_not_retained_in_history() {
        let path = temporary_history_path();
        let mut history = secure_history(&path).expect("secure history");
        let oversized = "x".repeat(super::MAX_HISTORY_ENTRY_BYTES + 1);

        let saved = history
            .save(HistoryItem::from_command_line(oversized))
            .expect("ignore oversized history");
        history.sync().expect("sync history");

        assert_eq!(saved.id, None);
        assert_eq!(history.count_all().expect("history count"), 0);
        assert_eq!(fs::metadata(&path).expect("history metadata").len(), 0);
        fs::remove_file(path).expect("remove history file");
    }

    fn temporary_history_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "dsql-shell-history-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }
}
