use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use std::io;

use anyhow::{Context, Result};
use crossbeam::channel::Receiver;
use crossterm::event::Event;
#[cfg(unix)]
use crossterm::{
    cursor::Show,
    event::{DisableFocusChange, EnableFocusChange},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dua::Config;
use dua::traverse::TraversalStats;
use dua::{
    ByteFormat, WalkOptions, WalkResult,
    traverse::{Traversal, TreeIndex},
};
use tui::{Terminal, backend::Backend};

use crate::interactive::widgets::{Language, MainWindow};

use super::{DisplayOptions, state::AppState};

/// Restores the user's terminal, suspends the process, and reinitializes the TUI after resume.
///
/// The previous frame is invalidated after resume so the caller's next draw repaints the complete
/// UI. This function does not draw by itself; the event loop draws normally after handling the
/// suspend key event.
#[cfg(unix)]
pub fn suspend_terminal<B>(terminal: &mut Terminal<B>, focus_change: bool) -> Result<()>
where
    B: Backend,
{
    let mut stderr = io::stderr();
    if focus_change {
        execute!(stderr, DisableFocusChange)?;
    }
    execute!(stderr, Show)?;
    disable_raw_mode()?;
    execute!(stderr, LeaveAlternateScreen)?;

    // This suspends the program, and anything that follows undoes the lines above.
    signal_hook::low_level::raise(signal_hook::consts::signal::SIGTSTP)?;

    enable_raw_mode()?;
    execute!(stderr, EnterAlternateScreen)?;
    if focus_change {
        execute!(stderr, EnableFocusChange)?;
    }
    // `Terminal::clear()` queries the cursor position, racing the input thread for its response.
    // This triggers a redraw as well without that issue.
    terminal.swap_buffers();
    Ok(())
}

/// State and methods representing the interactive disk usage analyser for the terminal
pub struct TerminalApp {
    pub config: Config,
    pub traversal: Traversal,
    #[cfg(test)]
    pub stats: TraversalStats,
    pub display: DisplayOptions,
    pub state: AppState,
    pub window: MainWindow,
}

impl TerminalApp {
    #[expect(
        clippy::too_many_arguments,
        reason = "initial traversal and its load duration are explicit initialization state"
    )]
    pub fn initialize<B>(
        terminal: &mut Terminal<B>,
        walk_options: WalkOptions,
        byte_format: ByteFormat,
        entry_check: bool,
        input: Vec<PathBuf>,
        root_path: Option<PathBuf>,
        config: Config,
        mut traversal: Traversal,
        snapshot_load_duration: Option<Duration>,
        snapshot_write_back: Option<(PathBuf, Option<i32>)>,
        snapshot_cache_dir: PathBuf,
        snapshot_compression: Option<i32>,
    ) -> Result<TerminalApp>
    where
        B: Backend,
    {
        terminal
            .hide_cursor()
            .map_err(|err| anyhow::Error::msg(err.to_string()))?;
        terminal
            .clear()
            .map_err(|err| anyhow::Error::msg(err.to_string()))?;

        let display = DisplayOptions::new(byte_format);
        let window = MainWindow::default();

        let read_only = snapshot_load_duration.is_some();
        let mut state = AppState::new(
            walk_options,
            input,
            root_path,
            read_only,
            snapshot_write_back,
            snapshot_cache_dir,
            snapshot_compression,
        );
        if config.gitignore == Some(false) {
            state.gitignored_entries = None;
        }
        if config.cleanup_heuristics == Some(false) {
            state.cleanup_candidates = None;
        }
        state.allow_entry_check = entry_check && !read_only;
        if read_only {
            state.gitignored_entries = None;
            state.stats = TraversalStats {
                entries_traversed: u64::try_from(traversal.tree.len().saturating_sub(1))
                    .unwrap_or(u64::MAX),
                elapsed: snapshot_load_duration,
                io_errors: traversal
                    .tree
                    .indices()
                    .filter(|index| {
                        traversal
                            .tree
                            .data(*index)
                            .is_some_and(|entry| entry.metadata_io_error)
                    })
                    .count()
                    .try_into()
                    .unwrap_or(u64::MAX),
                total_bytes: Some(
                    traversal
                        .tree
                        .data(traversal.root_index)
                        .expect("traversal root exists")
                        .size,
                ),
                ..TraversalStats::default()
            };
        }

        state.navigation_mut().view_root = traversal.root_index;
        let tree_view = state.tree_view(&mut traversal);
        state.entries = tree_view.sorted_entries(
            tree_view.traversal.root_index,
            state.sorting,
            state.entry_check(),
        );
        state.navigation_mut().selected = state.entries.first().map(|b| b.index);

        if let Some(candidates) = state.cleanup_candidates.as_mut() {
            *candidates = super::cleanup::cleanup_candidates(&state.entries);
        }
        state.reset_message();

        let app = TerminalApp {
            config,
            traversal,
            display,
            state,
            #[cfg(test)]
            stats: TraversalStats::default(),
            window,
        };
        Ok(app)
    }

    pub fn traverse(&mut self) -> Result<()> {
        self.state.traverse(&self.traversal, None)?;
        Ok(())
    }

    pub fn traverse_clean(&mut self, depth: Option<usize>) -> Result<()> {
        self.state.clean_hub = Some(super::clean_hub::CleanHub::new(
            self.state.root_paths.clone(),
            depth,
        ));
        self.state.entries.clear();
        self.state.root_path = None;
        self.traverse()
    }

    pub fn traverse_and_export(
        &mut self,
        path: PathBuf,
        compression_level: Option<i32>,
    ) -> Result<()> {
        self.state
            .traverse(&self.traversal, Some((path, compression_level)))?;
        Ok(())
    }

    pub fn process_events<B>(
        &mut self,
        terminal: &mut Terminal<B>,
        events: Receiver<Event>,
    ) -> Result<WalkResult>
    where
        B: Backend,
    {
        let result = self.state.process_events(
            &mut self.window,
            &mut self.traversal,
            &mut self.display,
            terminal,
            events,
            &self.config,
        );
        if result.is_err() {
            self.state.deletion = None;
        }
        result
    }

    pub fn process_events_once<B>(
        &mut self,
        terminal: &mut Terminal<B>,
        events: Receiver<Event>,
    ) -> Result<WalkResult>
    where
        B: Backend,
    {
        let result = self.state.process_events_once(
            &mut self.window,
            &mut self.traversal,
            &mut self.display,
            terminal,
            events,
            &self.config,
        );
        if result.is_err() {
            self.state.deletion = None;
        }
        result
    }
}

pub(super) fn write_snapshot_atomically(
    path: &Path,
    traversal: &Traversal,
    roots: &[TreeIndex],
    compression_level: Option<i32>,
    language: Language,
) -> Result<()> {
    let t = language.ui_text();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("{}{}", t.snapshot_temporary_failed, path.display()))?;
    dua::snapshot::write(temporary.as_file_mut(), traversal, roots, compression_level)
        .with_context(|| format!("{}{}", t.snapshot_write_failed, path.display()))?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .into_temp_path()
        .persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("{}{}", t.snapshot_install_failed, path.display()))?;
    Ok(())
}

/// Resolve the snapshot cache directory from `DUA_SNAPSHOT_DIR`, defaulting to
/// `~/.cache/dua/snapshots`.
pub fn snapshot_cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DUA_SNAPSHOT_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("dua")
        .join("snapshots")
}

/// Longest sanitized root-path portion of a cache snapshot file name; the timestamp suffix
/// and extension must still fit into common 255-byte file-name limits.
const SNAPSHOT_STEM_MAX_CHARS: usize = 160;

/// Turn the traversal's root paths into a file-name-safe stem: path separators become `-`,
/// multiple roots are joined with `-`, and overly long stems keep their most distinctive tail.
pub(super) fn snapshot_cache_stem(roots: &[PathBuf]) -> String {
    let mut stem = roots
        .iter()
        .map(|path| {
            path.components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(part) => Some(
                        part.to_string_lossy()
                            .replace(['/', '\\', ' '], "-")
                            .trim_matches('-')
                            .to_string(),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("-")
        })
        .collect::<Vec<_>>()
        .join("-");
    if stem.chars().count() > SNAPSHOT_STEM_MAX_CHARS {
        stem = stem
            .chars()
            .skip(stem.chars().count() - SNAPSHOT_STEM_MAX_CHARS)
            .collect();
    }
    stem
}

/// Build the cache snapshot file name `<stem>_<YYYYMMDD-HHMMSS>.snap`, appending `-N` when a
/// file with the same name already exists.
pub(super) fn snapshot_cache_file_name(stem: &str, timestamp: &str, dir: &Path) -> String {
    let base = format!("{stem}_{timestamp}");
    if !dir.join(format!("{base}.snap")).exists() {
        return format!("{base}.snap");
    }
    (1..100)
        .map(|n| format!("{base}-{n}.snap"))
        .find(|name| !dir.join(name).exists())
        .unwrap_or_else(|| format!("{base}-0.snap"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::TerminalApp;

    #[test]
    fn snapshot_cache_stem_sanitizes_root_paths() {
        assert_eq!(
            snapshot_cache_stem(&[PathBuf::from("/tmp/some dir/x")]),
            "tmp-some-dir-x"
        );
        assert_eq!(
            snapshot_cache_stem(&[PathBuf::from("relative/path")]),
            "relative-path"
        );
        assert_eq!(
            snapshot_cache_stem(&[PathBuf::from("/a/b"), PathBuf::from("c/d")]),
            "a-b-c-d"
        );
    }

    #[test]
    fn snapshot_cache_stem_keeps_its_distinctive_tail_when_too_long() {
        let long = "x".repeat(300);
        let stem = snapshot_cache_stem(&[PathBuf::from(format!("/prefix/{long}"))]);
        assert_eq!(stem.chars().count(), SNAPSHOT_STEM_MAX_CHARS);
        assert!(stem.ends_with(&long[..80]), "the tail is preserved");
    }

    #[test]
    fn snapshot_cache_file_name_appends_counter_on_collision() {
        let dir = tempfile::tempdir().expect("temp dir");
        let timestamp = "20260927-150101";
        let first = snapshot_cache_file_name("stem", timestamp, dir.path());
        assert_eq!(first, "stem_20260927-150101.snap");
        std::fs::write(dir.path().join(first), b"").expect("seed collision");
        let second = snapshot_cache_file_name("stem", timestamp, dir.path());
        assert_eq!(second, "stem_20260927-150101-1.snap");
    }

    impl TerminalApp {
        pub fn run_until_traversed<B>(
            &mut self,
            terminal: &mut Terminal<B>,
            events: Receiver<Event>,
        ) -> Result<WalkResult>
        where
            B: Backend,
        {
            while self.state.scan.is_some() {
                self.state.process_event(
                    &mut self.window,
                    &mut self.traversal,
                    &mut self.display,
                    terminal,
                    &events,
                    &self.config,
                )?;
            }
            Ok(WalkResult {
                num_errors: self.stats.io_errors,
            })
        }
    }
}
