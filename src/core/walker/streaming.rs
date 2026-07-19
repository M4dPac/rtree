//! Streaming tree traversal — renders output during DFS walk.
//!
//! Unlike `OrderedEngine` which builds a full tree in memory,
//! `StreamingEngine` writes entries directly to output as they are visited.
//! This reduces memory usage for large directory trees.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use super::{common, count_stats, EntryWriter};
use crate::config::Config;
use crate::core::entry::{Entry, EntryType};
use crate::core::walker::TreeStats;
use crate::error::TreeError;
use crate::i18n;

/// Result of streaming traversal.
pub struct StreamingResult {
    pub errors: Vec<TreeError>,
    pub truncated: bool,
}

/// Mutable traversal state shared across the recursive DFS walk.
///
/// Collects invariants (`needs_file_id`) and per-level state (`ancestors`)
/// to avoid threading them through every recursive call.
struct StreamState {
    count: usize,
    max_entries: Option<usize>,
    truncated: bool,
    visited: HashSet<common::VisitedKey>,
    root_device: Option<u64>,
    /// Whether `Entry` constructors should fetch OS file identity.
    needs_file_id: bool,
    /// Stack of `is_last` flags for ancestor levels.
    /// Push on descend, pop on return — avoids per-directory Vec allocation.
    ancestors: Vec<bool>,
}

impl StreamState {
    fn new(max_entries: Option<usize>, root_device: Option<u64>, needs_file_id: bool) -> Self {
        Self {
            count: 0,
            max_entries,
            truncated: false,
            visited: HashSet::new(),
            root_device,
            needs_file_id,
            ancestors: Vec::new(),
        }
    }

    /// Check if we've reached the entry limit.
    fn should_stop(&self) -> bool {
        self.max_entries.is_some_and(|max| self.count >= max)
    }
}

/// Streaming tree traversal engine.
///
/// Performs DFS traversal and writes text output inline,
/// computing is_last/ancestors_last on the fly.
pub struct StreamingEngine<'a> {
    config: &'a Config,
    entry_writer: &'a dyn EntryWriter,
}

impl<'a> StreamingEngine<'a> {
    pub fn new(config: &'a Config, entry_writer: &'a dyn EntryWriter) -> Self {
        Self {
            config,
            entry_writer,
        }
    }

    /// Traverse directory tree and write text output directly.
    ///
    /// Returns non-fatal traversal errors.
    pub fn traverse_and_render<W: Write>(
        &self,
        root: &Path,
        writer: &mut W,
        stats: &mut TreeStats,
    ) -> Result<StreamingResult, TreeError> {
        let config = self.config;
        let mut errors = Vec::new();
        let needs_file_id = common::needs_file_id(config);

        // When --long-paths is requested, resolve relative root to absolute
        // first — the \\?\ prefix only works with absolute paths.
        // canonicalize also normalises `.` and `..` which is critical on
        // Windows where \\?\ disables path normalisation.
        let long_root_buf = common::resolve_long_root(root, config.long_paths);
        let root = long_root_buf.as_path();

        // Root entry
        let root_entry = Entry::from_path(root, 0, needs_file_id, config.show_permissions)?;
        self.entry_writer.write_entry(writer, &root_entry, config)?;
        stats.directories += 1;

        // DFS children with max_entries tracking
        let mut state = StreamState::new(
            config.max_entries,
            common::compute_root_device(config, root),
            needs_file_id,
        );

        if config.one_fs && state.root_device.is_none() {
            errors.push(TreeError::Io(
                root.to_path_buf(),
                std::io::Error::other(
                    "--one-fs: cannot determine root volume; cross-device check skipped",
                ),
            ));
        }

        let mut ignore = common::IgnoreMatcher::new(!config.no_ignore);
        // See engine.rs: seed ancestor .gitignore/.rtignore rules so
        // root-anchored patterns above `root` are not silently dropped.
        ignore.seed_from_ancestors(root);

        state.visited.insert(common::make_visited_key(root));
        self.emit_children(
            root,
            1,
            false,
            writer,
            stats,
            &mut errors,
            &mut state,
            &ignore,
        )?;

        // Report
        if !config.no_report {
            writeln!(writer)?;
            let report = i18n::format_report(
                i18n::current(),
                stats.directories.saturating_sub(1),
                stats.files,
            );
            writeln!(writer, "{}", report)?;
        }

        Ok(StreamingResult {
            errors,
            truncated: state.truncated,
        })
    }

    /// Emit NTFS Alternate Data Streams for an entry.
    ///
    /// Each stream is written as a child at `depth + 1`.
    /// Caller must push/pop `state.ancestors` around this call.
    fn emit_ads<W: Write>(
        &self,
        entry: &Entry,
        depth: usize,
        config: &Config,
        writer: &mut W,
        stats: &mut TreeStats,
        state: &mut StreamState,
    ) -> Result<(), TreeError> {
        let streams = crate::platform::get_alternate_streams(&entry.path);
        let num_streams = streams.len();
        for (si, stream) in streams.into_iter().enumerate() {
            if state.should_stop() {
                state.truncated = true;
                return Ok(());
            }
            let mut ads_entry = Entry::from_ads(&entry.path, stream.name, stream.size, depth + 1);
            ads_entry.is_last = si == num_streams - 1;
            ads_entry.ancestors_last = state.ancestors.clone();
            self.entry_writer.write_entry(writer, &ads_entry, config)?;
            count_stats(&ads_entry, stats);
            state.count += 1;
        }
        Ok(())
    }

    /// Read, filter, sort, and emit children of a single directory,
    /// then recursively descend into subdirectories (DFS).
    #[allow(clippy::too_many_arguments)]
    fn emit_children<W: Write>(
        &self,
        dir: &Path,
        depth: usize,
        parent_matched: bool,
        writer: &mut W,
        stats: &mut TreeStats,
        errors: &mut Vec<TreeError>,
        state: &mut StreamState,
        ignore: &common::IgnoreMatcher,
    ) -> Result<(), TreeError> {
        let config = self.config;

        // Stack overflow protection
        if depth >= common::MAX_INTERNAL_DEPTH {
            errors.push(TreeError::MaxDepthExceeded(dir.to_path_buf()));
            return Ok(());
        }

        // max_depth: children at `depth` shown only if depth <= max.
        // Engine equivalent: parent at depth-1 checks `(depth-1) >= max`.
        if let Some(max) = config.max_depth {
            if depth > max {
                return Ok(());
            }
        }

        let mut current_ignore = ignore.clone_for_child();
        current_ignore.push_dir(dir);

        let dir_entries = match common::read_sorted_children(dir, config) {
            common::ReadDirResult::Entries(entries) => entries,
            common::ReadDirResult::ReadError(e) => {
                errors.push(TreeError::from_io(dir.to_path_buf(), e));
                return Ok(());
            }
            common::ReadDirResult::FilelimitExceeded(_) => return Ok(()),
        };

        // Pre-filter to know total count (needed for is_last)
        let filtered: Vec<_> = dir_entries
            .iter()
            .filter(
                |de| match common::filter_entry(config, de, parent_matched, &current_ignore) {
                    common::FilterResult::Include { .. } => true,
                    common::FilterResult::Reserved => {
                        errors.push(TreeError::ReservedName(de.path()));
                        false
                    }
                    common::FilterResult::Exclude => false,
                },
            )
            .collect();

        let total = filtered.len();
        for (i, dir_entry) in filtered.iter().enumerate() {
            // max_entries: stop before writing
            if state.should_stop() {
                state.truncated = true;
                return Ok(());
            }

            let is_last = i == total - 1;

            match Entry::from_dir_entry(
                dir_entry,
                depth,
                state.needs_file_id,
                config.show_permissions,
            ) {
                Ok(mut entry) => {
                    entry.is_last = is_last;
                    entry.ancestors_last = state.ancestors.clone();

                    let should_descend = match &entry.entry_type {
                        EntryType::Directory => true,
                        EntryType::Symlink { broken: false, .. } if config.follow_symlinks => {
                            entry.path.is_dir()
                        }
                        EntryType::Junction { .. } if config.show_junctions => true,
                        _ => false,
                    };

                    let descend = if should_descend {
                        let visit_key = common::make_visited_key(&entry.path);
                        if state.visited.insert(visit_key) {
                            true
                        } else {
                            entry.recursive_link = true;
                            false
                        }
                    } else {
                        false
                    };

                    let descend = descend
                        && match common::check_one_fs(state.root_device, &entry.path) {
                            common::OnefsCheck::Proceed => true,
                            common::OnefsCheck::DifferentDevice => false,
                            common::OnefsCheck::Unknown => {
                                errors.push(TreeError::Io(
                                    entry.path.clone(),
                                    std::io::Error::other("cannot determine volume for --one-fs"),
                                ));
                                false
                            }
                        };

                    self.entry_writer.write_entry(writer, &entry, config)?;
                    count_stats(&entry, stats);
                    state.count += 1;

                    // ADS and recursive descent use push/pop on state.ancestors.
                    // The `let r = f(); pop(); r?;` pattern ensures pop() executes
                    // even when f() returns Err (output I/O failure).
                    if config.show_streams && !descend {
                        state.ancestors.push(is_last);
                        let r = self.emit_ads(&entry, depth, config, writer, stats, state);
                        state.ancestors.pop();
                        r?;
                        if state.truncated {
                            return Ok(());
                        }
                    }

                    if descend {
                        let child_name = dir_entry.file_name();
                        let child_name_str = child_name.to_string_lossy();
                        let child_parent_matched =
                            parent_matched || config.filter.dir_matches_include(&child_name_str);
                        state.ancestors.push(is_last);
                        let r = self.emit_children(
                            &entry.path,
                            depth + 1,
                            child_parent_matched,
                            writer,
                            stats,
                            errors,
                            state,
                            &current_ignore,
                        );
                        state.ancestors.pop();
                        r?;
                        if state.truncated {
                            return Ok(());
                        }
                    }
                }
                Err(e) => errors.push(e),
            }
        }

        Ok(())
    }
}
