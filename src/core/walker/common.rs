//! Shared helpers for tree traversal engines.
//!
//! Contains filtering, classification, and utility functions
//! used by both `OrderedEngine` and `StreamingEngine`.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::fs::{DirEntry, ReadDir};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::core::entry::Entry;
use crate::core::sorter::sort_entries;
use crate::core::tree::Tree;
use crate::error::TreeError;

/// Maximum internal recursion depth to prevent stack overflow.
/// Protects both sequential and parallel (8 MiB stack ≈ ~7 000 frames) modes.
pub const MAX_INTERNAL_DEPTH: usize = 4096;

/// Key for the visited-set used by cycle detection.
///
/// Prefers OS file identity (volume serial + file ID / inode) which is
/// immune to path aliasing (junctions, `\\?\` prefix, UNC equivalents).
/// Falls back to canonicalized (then raw) path when identity is unavailable.
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub enum VisitedKey {
    /// File identity from the OS — volume serial number and file ID (or dev + ino).
    FileId { volume: u64, file_id: u64 },
    /// Fallback: canonical or raw path.
    Path(PathBuf),
}

/// Build a visited-set key for cycle detection.
///
/// Uses [`crate::platform::get_file_id_follow`] (which resolves symlinks /
/// reparse points) to obtain an identity that is stable across path aliases.
/// On failure falls back to `canonicalize`, then to the raw path.
pub fn make_visited_key(path: &Path) -> VisitedKey {
    if let Some(info) = crate::platform::get_file_id_follow(path) {
        VisitedKey::FileId {
            volume: info.volume_serial,
            file_id: info.file_id,
        }
    } else {
        VisitedKey::Path(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
    }
}

/// Result of filtering a directory entry.
pub enum FilterResult {
    /// Entry passes all filters; `is_dir` indicates directory status.
    Include { is_dir: bool },
    /// Entry excluded by filter rules (`-a`, `-d`, `-I`, `-P`, `--prune`).
    Exclude,
    /// Entry is a Windows reserved device name — caller may report a warning.
    Reserved,
}

/// Tracks .gitignore and .rtignore rules during traversal.
///
/// Maintains a stack of `Gitignore` instances, one for each directory
/// containing an ignore file. Cloning is cheap and safe for parallel traversal.
pub struct IgnoreMatcher {
    enabled: bool,
    stack: Vec<(PathBuf, Gitignore)>, // (root, gitignore)
}

impl IgnoreMatcher {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            stack: Vec::new(),
        }
    }

    /// Create a clone for a child directory (parallel-safe).
    pub fn clone_for_child(&self) -> Self {
        Self {
            enabled: self.enabled,
            stack: self.stack.clone(),
        }
    }

    /// Push ignore rules for the given directory.
    pub fn push_dir(&mut self, dir: &Path) {
        if !self.enabled {
            return;
        }
        let mut builder = GitignoreBuilder::new(dir);
        let _ = builder.add(dir.join(".gitignore"));
        let _ = builder.add(dir.join(".rtignore"));

        if let Ok(gi) = builder.build() {
            self.stack.push((dir.to_path_buf(), gi));
        }
    }

    /// Seed the matcher with `.gitignore` / `.rtignore` rules found in every
    /// ancestor of `root` (not including `root` itself — the caller is
    /// expected to `push_dir(root)` separately when traversal begins).
    ///
    /// Real `git` resolves root-anchored patterns (e.g. `/dir/ignore/`)
    /// relative to the directory containing the `.gitignore` file, not
    /// relative to whatever path was passed on the command line. Without
    /// this seeding step, running `rt <subdir>` silently drops any ignore
    /// rules defined in a `.gitignore` above `<subdir>` — e.g. a rule
    /// `/dir/ignore/` in the project root has no effect when `dir` itself
    /// is passed as the traversal root, because that `.gitignore` is never
    /// discovered.
    pub fn seed_from_ancestors(&mut self, root: &Path) {
        if !self.enabled {
            return;
        }

        // Ancestor discovery needs an absolute path — a relative root's
        // `.ancestors()` would stop after one or two components.
        let absolute = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::fs::canonicalize(root).unwrap_or_else(|_| {
                std::env::current_dir()
                    .map(|cwd| cwd.join(root))
                    .unwrap_or_else(|_| root.to_path_buf())
            })
        };

        // Collect ancestors above `root`, outermost first, so the stack
        // ends up ordered the same way a top-down traversal starting at
        // the real project root would have built it.
        let mut ancestors: Vec<PathBuf> = absolute
            .ancestors()
            .skip(1)
            .map(Path::to_path_buf)
            .collect();
        ancestors.reverse();

        for dir in ancestors {
            self.push_dir(&dir);
        }
    }

    /// Check if a path is ignored by any rules in the stack.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        if !self.enabled {
            return false;
        }
        for (root, gi) in &self.stack {
            let relative_path = path.strip_prefix(root).unwrap_or(path);
            if gi.matched(relative_path, is_dir).is_ignore() {
                return true;
            }
        }
        false
    }
}

/// Check if configuration requires file ID lookups (inode, device, one-fs).
pub fn needs_file_id(config: &Config) -> bool {
    config.one_fs || config.show_inodes || config.show_device
}

/// Compute root device serial for `--one-fs` boundary checking.
///
/// Returns `None` if `--one-fs` is not enabled.
pub fn compute_root_device(config: &Config, root: &Path) -> Option<u64> {
    if config.one_fs {
        crate::platform::get_file_id(root).map(|info| info.volume_serial)
    } else {
        None
    }
}

/// Unified entry filter: hidden, dirs_only, reserved, -I, -P, prune-symlinks.
///
/// Evaluates all per-entry filter rules from the configuration.
/// Returns `FilterResult` so the caller can decide how to handle
/// reserved names (e.g., push a warning vs silently skip).
///
/// On `DirEntry::file_type()` failure, returns `Exclude`.
pub fn filter_entry(
    config: &Config,
    de: &DirEntry,
    parent_matched: bool,
    ignore: &IgnoreMatcher,
) -> FilterResult {
    // Hidden files
    if !config.show_all && (de.file_name().as_encoded_bytes().first() == Some(&b'.')) {
        return FilterResult::Exclude;
    }

    let ft = match de.file_type() {
        Ok(ft) => ft,
        Err(_) => return FilterResult::Exclude,
    };

    let is_dir = ft.is_dir() || (config.follow_symlinks && ft.is_symlink() && de.path().is_dir());

    // dirs_only: include directories and symlinks to directories
    if config.dirs_only {
        let is_symlink_to_dir = ft.is_symlink() && de.path().is_dir();
        if !is_dir && !is_symlink_to_dir {
            return FilterResult::Exclude;
        }
    }

    // .gitignore / .rtignore
    if ignore.is_ignored(&de.path(), is_dir) {
        return FilterResult::Exclude;
    }

    // prune: symlinks to directories are "empty" when not followed — skip them
    if config.prune && !config.follow_symlinks && ft.is_symlink() && de.path().is_dir() {
        return FilterResult::Exclude;
    }

    let name_os = de.file_name();
    let name = name_os.to_string_lossy();

    // Skip Windows reserved device names (CON, NUL, PRN, …).
    if crate::platform::should_skip_reserved_name(&name) {
        return FilterResult::Reserved;
    }

    // -I always excludes matching entries
    if config.filter.excluded(&name) {
        return FilterResult::Exclude;
    }

    // -P include pattern: files only, unless parent dir matched via --matchdirs
    if !is_dir && !parent_matched && !config.filter.matches(&name, false) {
        return FilterResult::Exclude;
    }

    FilterResult::Include { is_dir }
}

// ──────────────────────────────────────────────
// Helpers extracted from engine / streaming
// ──────────────────────────────────────────────

/// Create a leaf tree node (no children).
///
/// Shorthand for the ubiquitous `Tree { entry, children: Vec::new() }`.
pub fn leaf_node(entry: Entry) -> Tree {
    Tree {
        entry,
        children: Vec::new(),
    }
}

/// Resolve root path with `--long-paths` support.
///
/// When `long_paths` is enabled and the root is relative, canonicalizes it
/// first (\\?\ prefix requires absolute paths and no `.`/`..` components).
/// Then applies `platform::to_long_path`.
pub fn resolve_long_root(root: &Path, long_paths: bool) -> PathBuf {
    let effective = if long_paths && !root.is_absolute() {
        std::fs::canonicalize(root).unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|cwd| cwd.join(root))
                .unwrap_or_else(|_| root.to_path_buf())
        })
    } else {
        root.to_path_buf()
    };
    crate::platform::to_long_path(&effective, long_paths)
}

/// Result of `--one-fs` boundary check.
pub enum OnefsCheck {
    /// Same device (or `--one-fs` not enabled) — descend normally.
    Proceed,
    /// Different device — do not descend.
    DifferentDevice,
    /// Cannot determine device — caller should emit error and not descend.
    Unknown,
}

/// Check whether `path` resides on the same filesystem as the root.
///
/// Returns `Proceed` when `root_device` is `None` (i.e. `--one-fs` disabled).
pub fn check_one_fs(root_device: Option<u64>, path: &Path) -> OnefsCheck {
    match root_device {
        None => OnefsCheck::Proceed,
        Some(root_dev) => match crate::platform::get_file_id(path) {
            Some(info) if info.volume_serial == root_dev => OnefsCheck::Proceed,
            Some(_) => OnefsCheck::DifferentDevice,
            None => OnefsCheck::Unknown,
        },
    }
}

/// Collect directory entries with optional `--filelimit` early exit.
///
/// When `file_limit` is `Some(limit)`, reads at most `limit + 1` entries
/// into memory.  If more entries exist, counts the remainder via the
/// iterator (O(1) memory) and returns `Err(total_count)`.
/// When `file_limit` is `None`, collects everything (existing behaviour).
pub fn collect_with_filelimit(
    read_dir: ReadDir,
    file_limit: Option<usize>,
) -> Result<Vec<DirEntry>, usize> {
    if let Some(limit) = file_limit {
        let mut iter = read_dir.filter_map(|e| e.ok());
        let check_count = limit.saturating_add(1);
        let entries: Vec<_> = iter.by_ref().take(check_count).collect();
        if entries.len() > limit {
            // Count remaining without storing — each DirEntry created & dropped immediately.
            let total = entries.len() + iter.count();
            return Err(total);
        }
        Ok(entries)
    } else {
        Ok(read_dir.filter_map(|e| e.ok()).collect())
    }
}

/// Determine whether an empty directory should be pruned.
///
/// Returns `true` when `--prune` is active, the directory has no children,
/// is not the root (`depth > 0`), and does not match a `--matchdirs` pattern.
pub fn should_prune(config: &Config, path: &Path, depth: usize, children_empty: bool) -> bool {
    if !config.prune || !children_empty || depth == 0 {
        return false;
    }
    let dir_name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    !config.filter.dir_matches_include(dir_name.as_ref())
}

// ──────────────────────────────────────────────
// Directory traversal shared helpers
// ──────────────────────────────────────────────

/// Result of checking whether to descend into a directory.
pub enum DescendCheck {
    /// Entry should be returned as a leaf node.
    Leaf,
    /// Same as Leaf, but also carries an error for the caller to report.
    LeafWithError(TreeError),
    /// All checks passed — caller should descend.
    Proceed,
}

/// Check junction, max_depth, and `--one-fs` conditions.
///
/// Call after creating the entry, before inserting into the visited set.
pub fn check_descend(
    entry: &Entry,
    path: &Path,
    depth: usize,
    config: &Config,
    root_device: Option<u64>,
) -> DescendCheck {
    if matches!(
        entry.entry_type,
        crate::core::entry::EntryType::Junction { .. }
    ) && !config.show_junctions
    {
        return DescendCheck::Leaf;
    }

    if let Some(max) = config.max_depth {
        if depth >= max {
            return DescendCheck::Leaf;
        }
    }

    match check_one_fs(root_device, path) {
        OnefsCheck::DifferentDevice => DescendCheck::Leaf,
        OnefsCheck::Unknown => DescendCheck::LeafWithError(TreeError::Io(
            path.to_path_buf(),
            std::io::Error::other("cannot determine volume for --one-fs"),
        )),
        OnefsCheck::Proceed => DescendCheck::Proceed,
    }
}

/// Result of reading a directory's children.
pub enum ReadDirResult {
    /// Successfully read, filtered for I/O errors, and sorted.
    Entries(Vec<DirEntry>),
    /// `read_dir` failed.
    ReadError(std::io::Error),
    /// `--filelimit` exceeded (carries total entry count).
    FilelimitExceeded(usize),
}

/// Read directory, apply `--filelimit`, and sort entries.
///
/// Combines `to_long_path` → `read_dir` → `collect_with_filelimit` → `sort_entries`.
pub fn read_sorted_children(path: &Path, config: &Config) -> ReadDirResult {
    let long_path = crate::platform::to_long_path(path, config.long_paths);
    match std::fs::read_dir(&long_path) {
        Err(e) => ReadDirResult::ReadError(e),
        Ok(rd) => match collect_with_filelimit(rd, config.file_limit) {
            Err(total) => ReadDirResult::FilelimitExceeded(total),
            Ok(mut entries) => {
                sort_entries(&mut entries, &config.sort_config);
                ReadDirResult::Entries(entries)
            }
        },
    }
}

/// Enumerate NTFS Alternate Data Streams for `path` and return them as
/// child tree nodes at the given `depth`.
///
/// On non-Windows platforms `crate::platform::get_alternate_streams`
/// returns an empty `Vec` — zero runtime cost, no `#[cfg]` needed here.
pub fn collect_ads_children(path: &Path, depth: usize) -> Vec<Tree> {
    crate::platform::get_alternate_streams(path)
        .into_iter()
        .map(|stream| Tree {
            entry: Entry::from_ads(path, stream.name, stream.size, depth),
            children: Vec::new(),
        })
        .collect()
}

/// Create a tree node for a file entry, with optional ADS children.
pub fn make_file_node(
    dir_entry: &DirEntry,
    depth: usize,
    needs_file_id: bool,
    show_permissions: bool,
    show_streams: bool,
) -> Result<Tree, TreeError> {
    let entry = Entry::from_dir_entry(dir_entry, depth, needs_file_id, show_permissions)?;
    let stream_children = if show_streams {
        collect_ads_children(&dir_entry.path(), depth + 1)
    } else {
        Vec::new()
    };
    Ok(Tree {
        entry,
        children: stream_children,
    })
}

/// Create a leaf node marked as a recursive symlink.
pub fn make_recursive_link_node(
    dir_entry: &DirEntry,
    depth: usize,
    needs_file_id: bool,
    show_permissions: bool,
) -> Result<Tree, TreeError> {
    let mut entry = Entry::from_dir_entry(dir_entry, depth, needs_file_id, show_permissions)?;
    entry.recursive_link = true;
    Ok(leaf_node(entry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ══════════════════════════════════════════════
    // VisitedKey — equality, hashing
    // ══════════════════════════════════════════════

    #[test]
    fn visited_key_file_id_equal() {
        let a = VisitedKey::FileId {
            volume: 1,
            file_id: 100,
        };
        let b = VisitedKey::FileId {
            volume: 1,
            file_id: 100,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn visited_key_file_id_diff_volume() {
        let a = VisitedKey::FileId {
            volume: 1,
            file_id: 100,
        };
        let b = VisitedKey::FileId {
            volume: 2,
            file_id: 100,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn visited_key_file_id_diff_id() {
        let a = VisitedKey::FileId {
            volume: 1,
            file_id: 100,
        };
        let b = VisitedKey::FileId {
            volume: 1,
            file_id: 200,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn visited_key_path_equal() {
        let a = VisitedKey::Path(PathBuf::from("/tmp/a"));
        let b = VisitedKey::Path(PathBuf::from("/tmp/a"));
        assert_eq!(a, b);
    }

    #[test]
    fn visited_key_path_diff() {
        let a = VisitedKey::Path(PathBuf::from("/tmp/a"));
        let b = VisitedKey::Path(PathBuf::from("/tmp/b"));
        assert_ne!(a, b);
    }

    #[test]
    fn visited_key_variants_never_equal() {
        let a = VisitedKey::FileId {
            volume: 0,
            file_id: 0,
        };
        let b = VisitedKey::Path(PathBuf::new());
        assert_ne!(a, b);
    }

    #[test]
    fn visited_key_hashset_dedup() {
        let mut set = HashSet::new();
        let k = VisitedKey::FileId {
            volume: 1,
            file_id: 42,
        };
        assert!(set.insert(k.clone()));
        assert!(!set.insert(k), "duplicate must not be inserted");
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn visited_key_hashset_distinct() {
        let mut set = HashSet::new();
        set.insert(VisitedKey::FileId {
            volume: 1,
            file_id: 1,
        });
        set.insert(VisitedKey::FileId {
            volume: 1,
            file_id: 2,
        });
        set.insert(VisitedKey::Path(PathBuf::from("/x")));
        assert_eq!(set.len(), 3);
    }

    // ══════════════════════════════════════════════
    // make_visited_key (real filesystem)
    // ══════════════════════════════════════════════

    #[test]
    fn make_visited_key_real_dir() {
        let dir = tempfile::tempdir().unwrap();
        let key = make_visited_key(dir.path());
        match &key {
            VisitedKey::FileId { .. } => { /* valid on Windows */ }
            VisitedKey::Path(p) => assert!(p.exists()),
        }
    }

    #[test]
    fn make_visited_key_stable() {
        let dir = tempfile::tempdir().unwrap();
        let k1 = make_visited_key(dir.path());
        let k2 = make_visited_key(dir.path());
        assert_eq!(k1, k2);
    }

    #[test]
    fn make_visited_key_nonexistent_returns_path() {
        let key = make_visited_key(Path::new("/no/such/path/99999"));
        assert!(matches!(key, VisitedKey::Path(_)));
    }

    #[test]
    fn make_visited_key_different_dirs_differ() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let k1 = make_visited_key(d1.path());
        let k2 = make_visited_key(d2.path());
        assert_ne!(k1, k2);
    }

    // ══════════════════════════════════════════════
    // leaf_node
    // ══════════════════════════════════════════════

    #[test]
    fn leaf_node_empty_children() {
        let entry = crate::core::entry::Entry {
            path: PathBuf::from("test"),
            name: std::ffi::OsString::from("test"),
            entry_type: crate::core::entry::EntryType::File,
            metadata: None,
            depth: 0,
            is_last: false,
            ancestors_last: vec![],
            filelimit_exceeded: None,
            recursive_link: false,
        };
        let node = leaf_node(entry);
        assert!(node.children.is_empty());
        assert_eq!(node.entry.name.to_string_lossy(), "test");
    }

    // ══════════════════════════════════════════════
    // check_one_fs
    // ══════════════════════════════════════════════

    #[test]
    fn check_one_fs_none_always_proceed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            check_one_fs(None, dir.path()),
            OnefsCheck::Proceed
        ));
    }

    #[test]
    fn check_one_fs_same_device() {
        let dir = tempfile::tempdir().unwrap();
        if let Some(info) = crate::platform::get_file_id(dir.path()) {
            assert!(matches!(
                check_one_fs(Some(info.volume_serial), dir.path()),
                OnefsCheck::Proceed
            ));
        }
    }

    #[test]
    fn check_one_fs_different_device() {
        let dir = tempfile::tempdir().unwrap();
        if let Some(info) = crate::platform::get_file_id(dir.path()) {
            let fake = info.volume_serial.wrapping_add(999);
            assert!(matches!(
                check_one_fs(Some(fake), dir.path()),
                OnefsCheck::DifferentDevice
            ));
        }
    }

    #[test]
    fn check_one_fs_nonexistent() {
        let result = check_one_fs(Some(1), Path::new("/no/such/path/42"));
        assert!(!matches!(result, OnefsCheck::Proceed));
    }

    // ══════════════════════════════════════════════
    // collect_with_filelimit
    // ══════════════════════════════════════════════

    #[test]
    fn filelimit_none_collects_all() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "").unwrap();
        }
        let rd = std::fs::read_dir(dir.path()).unwrap();
        let result = collect_with_filelimit(rd, None);
        assert_eq!(result.unwrap().len(), 5);
    }

    #[test]
    fn filelimit_within_limit() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "").unwrap();
        }
        let rd = std::fs::read_dir(dir.path()).unwrap();
        let result = collect_with_filelimit(rd, Some(10));
        assert_eq!(result.unwrap().len(), 3);
    }

    #[test]
    fn filelimit_exact_boundary() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "").unwrap();
        }
        let rd = std::fs::read_dir(dir.path()).unwrap();
        assert_eq!(collect_with_filelimit(rd, Some(5)).unwrap().len(), 5);
    }

    #[test]
    fn filelimit_exceeded_returns_total() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "").unwrap();
        }
        let rd = std::fs::read_dir(dir.path()).unwrap();
        assert_eq!(collect_with_filelimit(rd, Some(5)).unwrap_err(), 10);
    }

    #[test]
    fn filelimit_exceeded_by_one() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..6 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "").unwrap();
        }
        let rd = std::fs::read_dir(dir.path()).unwrap();
        assert_eq!(collect_with_filelimit(rd, Some(5)).unwrap_err(), 6);
    }

    #[test]
    fn filelimit_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let rd = std::fs::read_dir(dir.path()).unwrap();
        assert!(collect_with_filelimit(rd, Some(5)).unwrap().is_empty());
    }

    #[test]
    fn filelimit_one_single_file_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("only.txt"), "").unwrap();
        let rd = std::fs::read_dir(dir.path()).unwrap();
        assert_eq!(collect_with_filelimit(rd, Some(1)).unwrap().len(), 1);
    }

    #[test]
    fn filelimit_one_two_files_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        let rd = std::fs::read_dir(dir.path()).unwrap();
        assert_eq!(collect_with_filelimit(rd, Some(1)).unwrap_err(), 2);
    }

    // ══════════════════════════════════════════════
    // resolve_long_root
    // ══════════════════════════════════════════════

    #[test]
    fn resolve_long_root_disabled_passthrough() {
        let p = PathBuf::from("/tmp/test");
        assert_eq!(resolve_long_root(&p, false), p);
    }

    #[test]
    fn resolve_long_root_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_long_root(dir.path(), true);
        if cfg!(windows) {
            assert!(resolved.to_string_lossy().contains(r"\\?\"));
        } else {
            assert_eq!(resolved.as_path(), dir.path());
        }
    }

    #[test]
    fn resolve_long_root_relative_disabled() {
        let p = PathBuf::from("relative/path");
        assert_eq!(resolve_long_root(&p, false), p);
    }

    // ══════════════════════════════════════════════
    // collect_ads_children
    // ══════════════════════════════════════════════

    #[test]
    fn collect_ads_children_regular_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("plain.txt");
        std::fs::write(&f, "data").unwrap();

        let ads = collect_ads_children(&f, 1);
        // On non-Windows or NTFS without ADS, returns empty
        if !cfg!(windows) {
            assert!(ads.is_empty());
        }
    }

    // ══════════════════════════════════════════════
    // make_file_node (real filesystem)
    // ══════════════════════════════════════════════

    #[test]
    fn make_file_node_basic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "content").unwrap();

        let de = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();

        let node = make_file_node(&de, 1, false, false, false).unwrap();
        assert_eq!(node.entry.name.to_string_lossy(), "file.txt");
        assert!(node.children.is_empty()); // no ADS
        assert!(matches!(
            node.entry.entry_type,
            crate::core::entry::EntryType::File
        ));
    }

    // ══════════════════════════════════════════════
    // make_recursive_link_node (real filesystem)
    // ══════════════════════════════════════════════

    #[test]
    fn make_recursive_link_node_sets_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("link_target.txt"), "x").unwrap();

        let de = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();

        let node = make_recursive_link_node(&de, 2, false, false).unwrap();
        assert!(node.entry.recursive_link);
        assert!(node.children.is_empty());
    }

    // ══════════════════════════════════════════════
    // IgnoreMatcher
    // ══════════════════════════════════════════════

    #[test]
    fn ignore_matcher_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored_file.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored_file.txt"), "").unwrap();
        std::fs::write(dir.path().join("normal_file.txt"), "").unwrap();

        let mut ignore = IgnoreMatcher::new(true);
        ignore.push_dir(dir.path());

        assert!(ignore.is_ignored(&dir.path().join("ignored_file.txt"), false));
        assert!(!ignore.is_ignored(&dir.path().join("normal_file.txt"), false));
    }

    #[test]
    fn ignore_matcher_respects_rtignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".rtignore"), "rt_ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("rt_ignored.txt"), "").unwrap();

        let mut ignore = IgnoreMatcher::new(true);
        ignore.push_dir(dir.path());

        assert!(ignore.is_ignored(&dir.path().join("rt_ignored.txt"), false));
    }

    #[test]
    fn ignore_matcher_disabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored_file.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored_file.txt"), "").unwrap();

        let mut ignore = IgnoreMatcher::new(false);
        ignore.push_dir(dir.path());

        assert!(!ignore.is_ignored(&dir.path().join("ignored_file.txt"), false));
    }

    // Regression test for: root-anchored gitignore patterns defined above
    // the traversal root were silently ignored when a subdirectory was
    // passed directly to `rt`, e.g.:
    //   project/.gitignore   ->  "/dir/ignore/"
    //   project/dir/ignore/  ->  should be ignored even when running
    //                             `rt project/dir` directly.
    #[test]
    fn seed_from_ancestors_applies_parent_gitignore_when_root_is_subdir() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(".gitignore"), "/dir/ignore/\n").unwrap();

        let sub_root = project.path().join("dir");
        std::fs::create_dir_all(sub_root.join("ignore")).unwrap();
        std::fs::write(sub_root.join("ignore").join("file.txt"), "").unwrap();
        std::fs::write(sub_root.join("keep.txt"), "").unwrap();

        // Before the fix: only `push_dir(sub_root)` was called, so the
        // `.gitignore` in `project/` (the parent of the traversal root)
        // was never discovered and `dir/ignore` leaked into the output.
        let mut ignore = IgnoreMatcher::new(true);
        ignore.seed_from_ancestors(&sub_root);
        ignore.push_dir(&sub_root);

        assert!(ignore.is_ignored(&sub_root.join("ignore"), true));
        assert!(!ignore.is_ignored(&sub_root.join("keep.txt"), false));
    }

    #[test]
    fn seed_from_ancestors_noop_when_disabled() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(".gitignore"), "/dir/ignore/\n").unwrap();
        let sub_root = project.path().join("dir");
        std::fs::create_dir_all(sub_root.join("ignore")).unwrap();

        let mut ignore = IgnoreMatcher::new(false);
        ignore.seed_from_ancestors(&sub_root);
        ignore.push_dir(&sub_root);

        assert!(!ignore.is_ignored(&sub_root.join("ignore"), true));
    }
}
