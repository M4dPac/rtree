//! Unified tree engine (sequential + parallel)
//! Deterministic structure, no duplicates, identical output in both modes.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Condvar, Mutex};

use rayon::prelude::*;

use super::common;
use crate::config::Config;
use crate::core::entry::Entry;
use crate::error::TreeError;

use crate::core::tree::Tree;

//
// ==============================
// Backpressure: DirReadLimiter
// ==============================
//

/// Limits the number of concurrent `read_dir` + collect operations
/// in parallel mode.  Provides backpressure to prevent excessive
/// file-descriptor usage and memory spikes on wide trees.
///
/// **Deadlock-freedom:** the guard is held only during `read_sorted_children`
/// (block scope) and released before `par_iter` spawns child work.  Since
/// child directories acquire their own permits *after* the parent releases,
/// all rayon workers always make progress.  `max` is clamped to ≥ 1.
struct DirReadLimiter {
    state: Mutex<usize>,
    cvar: Condvar,
    max: usize,
}

/// RAII guard — releases one permit on drop.
struct DirReadGuard<'a>(&'a DirReadLimiter);

impl DirReadLimiter {
    fn new(max: usize) -> Self {
        Self {
            state: Mutex::new(0),
            cvar: Condvar::new(),
            max: max.max(1), // at least 1 to prevent deadlock
        }
    }

    fn acquire(&self) -> DirReadGuard<'_> {
        let mut active = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while *active >= self.max {
            active = self.cvar.wait(active).unwrap_or_else(|p| p.into_inner());
        }
        *active += 1;
        DirReadGuard(self)
    }
}

impl Drop for DirReadGuard<'_> {
    fn drop(&mut self) {
        let mut active = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        *active -= 1;
        self.0.cvar.notify_one();
    }
}

//
// ==============================
// Mutex-poison-safe helpers
// ==============================
//

/// Push an error into a poisonable Mutex<Vec<TreeError>>,
/// recovering from poison instead of losing the error.
fn push_error(errors: &Mutex<Vec<TreeError>>, error: TreeError) {
    match errors.lock() {
        Ok(mut errs) => errs.push(error),
        Err(poisoned) => poisoned.into_inner().push(error),
    }
}

/// Atomically check-and-insert into the visited set.
/// Returns `true` if the path was **already** present (= cycle).
/// On poison: recovers and still inserts.
fn check_visited(visited: &Mutex<HashSet<common::VisitedKey>>, key: common::VisitedKey) -> bool {
    match visited.lock() {
        Ok(mut v) => !v.insert(key),
        Err(poisoned) => !poisoned.into_inner().insert(key),
    }
}

//
// ==============================
// Parallel traversal context
// ==============================
//

/// Shared state for parallel directory traversal.
/// Bundles references that would otherwise require 8+ function arguments.
struct ParallelCtx<'a> {
    config: &'a Config,
    errors: &'a Mutex<Vec<TreeError>>,
    visited: &'a Mutex<HashSet<common::VisitedKey>>,
    dir_limiter: &'a DirReadLimiter,
    root_device: Option<u64>,
}

//
// ==============================
// Result
// ==============================
//

/// Result of tree traversal: entries + any errors encountered
#[derive(Default)]
pub struct TraversalResult {
    pub errors: Vec<TreeError>,
    pub truncated: bool,
    /// Hierarchical tree for rendering
    pub tree: Option<Tree>,
}

//
// ==============================
// Public Engine
// ==============================
//

pub struct OrderedEngine {
    pool: Option<rayon::ThreadPool>,
}

impl OrderedEngine {
    pub fn new(config: &Config) -> Self {
        let pool = if config.parallel {
            let mut builder = rayon::ThreadPoolBuilder::new().stack_size(8 * 1024 * 1024); // 8 MiB — match main thread stack
            if let Some(n) = config.threads {
                builder = builder.num_threads(n);
            }
            match builder.build() {
                Ok(pool) => Some(pool),
                Err(e) => {
                    crate::error::diag_warn(format_args!(
                        "--parallel: thread pool creation failed, falling back to sequential: {}",
                        e
                    ));
                    None
                }
            }
        } else {
            None
        };

        Self { pool }
    }

    pub fn traverse<P: AsRef<Path>>(&self, root: P, config: &Config) -> TraversalResult {
        let mut errors = Vec::new();
        let visited = HashSet::new();

        let long_root_buf = common::resolve_long_root(root.as_ref(), config.long_paths);
        let root_path = long_root_buf.as_path();

        let root_device = common::compute_root_device(config, root_path);

        if config.one_fs && root_device.is_none() {
            errors.push(TreeError::Io(
                root_path.to_path_buf(),
                std::io::Error::other(
                    "--one-fs: cannot determine root volume; cross-device check skipped",
                ),
            ));
        }

        let dir_limiter = DirReadLimiter::new(config.queue_cap.unwrap_or(64));
        let mut ignore = common::IgnoreMatcher::new(!config.no_ignore);
        // Load .gitignore/.rtignore rules from directories *above* root_path
        // so root-anchored patterns (e.g. `/dir/ignore/`) defined higher up
        // still apply when a subdirectory is passed as the traversal root.
        ignore.seed_from_ancestors(root_path);

        // Dispatch: pool present → parallel, otherwise → sequential.
        // Sequential path is written once (was duplicated before).
        let root_node = match &self.pool {
            Some(pool) => {
                let errors_mutex = Mutex::new(Vec::new());
                let visited_mutex = Mutex::new(visited);

                let result = {
                    let ctx = ParallelCtx {
                        config,
                        errors: &errors_mutex,
                        visited: &visited_mutex,
                        dir_limiter: &dir_limiter,
                        root_device,
                    };
                    pool.install(|| {
                        build_node_parallel_inner(
                            root_path,
                            0,
                            &ctx,
                            false,
                            ignore.clone_for_child(),
                        )
                    })
                };

                match errors_mutex.into_inner() {
                    Ok(errs) => errors.extend(errs),
                    Err(poisoned) => errors.extend(poisoned.into_inner()),
                }
                result
            }
            None => {
                let mut visited = visited;
                build_node_sequential(
                    root_path,
                    0,
                    config,
                    &mut errors,
                    &mut visited,
                    false,
                    root_device,
                    ignore,
                )
            }
        };

        let root_node = match root_node {
            Some(node) => node,
            None => {
                return TraversalResult {
                    errors,
                    truncated: false,
                    tree: None,
                };
            }
        };

        // In ordered mode the full tree is always built; max_entries only
        // sets a flag for renderers.  Streaming mode stops traversal early.
        let truncated = config
            .max_entries
            .is_some_and(|max| root_node.count_nodes().saturating_sub(1) > max);

        TraversalResult {
            errors,
            truncated,
            tree: Some(root_node),
        }
    }
}

//
// ==============================
// Sequential Builder
// ==============================
//

#[allow(clippy::too_many_arguments)]
fn build_node_sequential(
    path: &Path,
    depth: usize,
    config: &Config,
    errors: &mut Vec<TreeError>,
    visited: &mut HashSet<common::VisitedKey>,
    parent_matched: bool,
    root_device: Option<u64>,
    mut ignore: common::IgnoreMatcher,
) -> Option<Tree> {
    if depth >= common::MAX_INTERNAL_DEPTH {
        errors.push(TreeError::MaxDepthExceeded(path.to_path_buf()));
        return None;
    }

    let needs_file_id = common::needs_file_id(config);
    let mut entry = match Entry::from_path(path, depth, needs_file_id, config.show_permissions) {
        Ok(e) => e,
        Err(e) => {
            errors.push(e);
            return None;
        }
    };

    match common::check_descend(&entry, path, depth, config, root_device) {
        common::DescendCheck::Leaf => return Some(common::leaf_node(entry)),
        common::DescendCheck::LeafWithError(e) => {
            errors.push(e);
            return Some(common::leaf_node(entry));
        }
        common::DescendCheck::Proceed => {}
    }

    // Track visited directories for cycle detection.
    let canon_key = common::make_visited_key(path);
    if !visited.insert(canon_key) {
        entry.recursive_link = true;
        return Some(common::leaf_node(entry));
    }

    let dir_entries = match common::read_sorted_children(path, config) {
        common::ReadDirResult::Entries(entries) => entries,
        common::ReadDirResult::ReadError(e) => {
            errors.push(TreeError::from_io(path.to_path_buf(), e));
            return Some(common::leaf_node(entry));
        }
        common::ReadDirResult::FilelimitExceeded(total) => {
            entry.filelimit_exceeded = Some(total);
            return Some(common::leaf_node(entry));
        }
    };

    ignore.push_dir(path);

    let mut children = Vec::with_capacity(dir_entries.len());

    for dir_entry in dir_entries {
        let is_dir = match common::filter_entry(config, &dir_entry, parent_matched, &ignore) {
            common::FilterResult::Include { is_dir } => is_dir,
            common::FilterResult::Reserved => {
                errors.push(TreeError::ReservedName(dir_entry.path()));
                continue;
            }
            common::FilterResult::Exclude => continue,
        };

        if is_dir {
            // Check for recursive symlink when following
            if config.follow_symlinks && dir_entry.file_type().is_ok_and(|ft| ft.is_symlink()) {
                let symlink_key = common::make_visited_key(&dir_entry.path());
                if visited.contains(&symlink_key) {
                    errors.push(TreeError::SymlinkLoop(dir_entry.path()));
                    match common::make_recursive_link_node(
                        &dir_entry,
                        depth + 1,
                        needs_file_id,
                        config.show_permissions,
                    ) {
                        Ok(node) => children.push(node),
                        Err(e) => errors.push(e),
                    }
                    continue;
                }
            }
            let child_name = dir_entry.file_name();
            let child_name_str = child_name.to_string_lossy();
            let child_parent_matched =
                parent_matched || config.filter.dir_matches_include(&child_name_str);
            let mut child_ignore = ignore.clone_for_child();
            child_ignore.push_dir(&dir_entry.path());

            if let Some(child) = build_node_sequential(
                &dir_entry.path(),
                depth + 1,
                config,
                errors,
                visited,
                child_parent_matched,
                root_device,
                child_ignore,
            ) {
                children.push(child);
            }
        } else {
            match common::make_file_node(
                &dir_entry,
                depth + 1,
                needs_file_id,
                config.show_permissions,
                config.show_streams,
            ) {
                Ok(node) => children.push(node),
                Err(e) => errors.push(e),
            }
        }
    }

    if common::should_prune(config, path, depth, children.is_empty()) {
        return None;
    }

    Some(Tree { entry, children })
}

//
// ==============================
// Parallel Builder
// ==============================
//

fn build_node_parallel_inner(
    path: &Path,
    depth: usize,
    ctx: &ParallelCtx<'_>,
    parent_matched: bool,
    ignore: common::IgnoreMatcher,
) -> Option<Tree> {
    if depth >= common::MAX_INTERNAL_DEPTH {
        push_error(ctx.errors, TreeError::MaxDepthExceeded(path.to_path_buf()));
        return None;
    }

    let needs_file_id = common::needs_file_id(ctx.config);
    let mut entry = match Entry::from_path(path, depth, needs_file_id, ctx.config.show_permissions)
    {
        Ok(e) => e,
        Err(e) => {
            push_error(ctx.errors, e);
            return None;
        }
    };

    match common::check_descend(&entry, path, depth, ctx.config, ctx.root_device) {
        common::DescendCheck::Leaf => return Some(common::leaf_node(entry)),
        common::DescendCheck::LeafWithError(e) => {
            push_error(ctx.errors, e);
            return Some(common::leaf_node(entry));
        }
        common::DescendCheck::Proceed => {}
    }

    // Track visited directories for cycle detection.
    let canon_key = common::make_visited_key(path);
    if check_visited(ctx.visited, canon_key) {
        entry.recursive_link = true;
        return Some(common::leaf_node(entry));
    }

    // Backpressure: _guard released when block exits (or on early return).
    let dir_entries = {
        let _guard = ctx.dir_limiter.acquire();
        match common::read_sorted_children(path, ctx.config) {
            common::ReadDirResult::Entries(entries) => entries,
            common::ReadDirResult::ReadError(e) => {
                push_error(ctx.errors, TreeError::from_io(path.to_path_buf(), e));
                return Some(common::leaf_node(entry));
            }
            common::ReadDirResult::FilelimitExceeded(total) => {
                entry.filelimit_exceeded = Some(total);
                return Some(common::leaf_node(entry));
            }
        }
    };

    let mut current_ignore = ignore;
    current_ignore.push_dir(path);

    // rayon's par_iter preserves element order for indexed sources,
    // so children appear in the same sorted order as dir_entries.
    let children: Vec<Tree> = dir_entries
        .par_iter()
        .filter_map(|dir_entry| {
            let is_dir = match common::filter_entry(
                ctx.config,
                dir_entry,
                parent_matched,
                &current_ignore,
            ) {
                common::FilterResult::Include { is_dir } => is_dir,
                common::FilterResult::Reserved => {
                    push_error(ctx.errors, TreeError::ReservedName(dir_entry.path()));
                    return None;
                }
                common::FilterResult::Exclude => return None,
            };

            if is_dir {
                // Check for recursive symlink when following — atomic check
                if ctx.config.follow_symlinks
                    && dir_entry.file_type().is_ok_and(|ft| ft.is_symlink())
                {
                    let symlink_key = common::make_visited_key(&dir_entry.path());
                    let is_visited = match ctx.visited.lock() {
                        Ok(v) => v.contains(&symlink_key),
                        Err(poisoned) => poisoned.into_inner().contains(&symlink_key),
                    };
                    if is_visited {
                        push_error(ctx.errors, TreeError::SymlinkLoop(dir_entry.path()));
                        return match common::make_recursive_link_node(
                            dir_entry,
                            depth + 1,
                            needs_file_id,
                            ctx.config.show_permissions,
                        ) {
                            Ok(node) => Some(node),
                            Err(e) => {
                                push_error(ctx.errors, e);
                                None
                            }
                        };
                    }
                }
                let child_name = dir_entry.file_name();
                let child_name_str = child_name.to_string_lossy();
                let child_parent_matched =
                    parent_matched || ctx.config.filter.dir_matches_include(&child_name_str);
                let child_ignore = current_ignore.clone_for_child();

                build_node_parallel_inner(
                    &dir_entry.path(),
                    depth + 1,
                    ctx,
                    child_parent_matched,
                    child_ignore,
                )
            } else {
                match common::make_file_node(
                    dir_entry,
                    depth + 1,
                    needs_file_id,
                    ctx.config.show_permissions,
                    ctx.config.show_streams,
                ) {
                    Ok(node) => Some(node),
                    Err(e) => {
                        push_error(ctx.errors, e);
                        None
                    }
                }
            }
        })
        .collect();

    if common::should_prune(ctx.config, path, depth, children.is_empty()) {
        return None;
    }

    Some(Tree { entry, children })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ═══════════════════════════════════════
    // DirReadLimiter
    // ═══════════════════════════════════════

    #[test]
    fn limiter_zero_becomes_one() {
        let limiter = DirReadLimiter::new(0);
        assert_eq!(limiter.max, 1, "max=0 must clamp to 1 to prevent deadlock");
    }

    #[test]
    fn limiter_acquire_up_to_max() {
        let limiter = DirReadLimiter::new(3);
        let _g1 = limiter.acquire();
        let _g2 = limiter.acquire();
        let _g3 = limiter.acquire();
        // All 3 acquired without blocking
    }

    #[test]
    fn limiter_release_unblocks_waiter() {
        use std::sync::Arc;
        use std::thread;

        let limiter = Arc::new(DirReadLimiter::new(1));
        let g1 = limiter.acquire(); // holds the only permit

        let limiter2 = Arc::clone(&limiter);
        let handle = thread::spawn(move || {
            let _g2 = limiter2.acquire(); // blocks until g1 released
            42
        });

        drop(g1); // release → unblock thread
        assert_eq!(handle.join().unwrap(), 42);
    }

    #[test]
    fn limiter_guard_drops_on_scope_exit() {
        let limiter = DirReadLimiter::new(1);
        {
            let _g = limiter.acquire();
        } // guard dropped
        let _g2 = limiter.acquire(); // must not block
    }

    // ═══════════════════════════════════════
    // push_error
    // ═══════════════════════════════════════

    #[test]
    fn push_error_basic() {
        let errors = Mutex::new(Vec::new());
        push_error(&errors, TreeError::Generic("a".into()));
        push_error(&errors, TreeError::Generic("b".into()));
        assert_eq!(errors.lock().unwrap().len(), 2);
    }

    #[test]
    fn push_error_recovers_from_poison() {
        use std::sync::Arc;
        use std::thread;

        let errors = Arc::new(Mutex::new(Vec::new()));
        let errors2 = Arc::clone(&errors);

        // Poison the mutex
        let _ = thread::spawn(move || {
            let _guard = errors2.lock().unwrap();
            panic!("intentional poison");
        })
        .join();

        // push_error must not panic on poisoned mutex
        push_error(&errors, TreeError::Generic("recovered".into()));

        match errors.lock() {
            Err(poisoned) => assert_eq!(poisoned.into_inner().len(), 1),
            Ok(_) => panic!("mutex should be poisoned"),
        };
    }

    // ═══════════════════════════════════════
    // check_visited
    // ═══════════════════════════════════════

    #[test]
    fn check_visited_new_returns_false() {
        let visited = Mutex::new(HashSet::new());
        let key = common::VisitedKey::Path(std::path::PathBuf::from("/a"));
        assert!(!check_visited(&visited, key), "first insert → false");
    }

    #[test]
    fn check_visited_duplicate_returns_true() {
        let visited = Mutex::new(HashSet::new());
        let k1 = common::VisitedKey::Path(std::path::PathBuf::from("/a"));
        let k2 = common::VisitedKey::Path(std::path::PathBuf::from("/a"));
        assert!(!check_visited(&visited, k1));
        assert!(check_visited(&visited, k2), "duplicate → true");
    }

    #[test]
    fn check_visited_recovers_from_poison() {
        use std::sync::Arc;
        use std::thread;

        let visited = Arc::new(Mutex::new(HashSet::new()));
        let visited2 = Arc::clone(&visited);

        let _ = thread::spawn(move || {
            let _guard = visited2.lock().unwrap();
            panic!("intentional poison");
        })
        .join();

        let key = common::VisitedKey::Path(std::path::PathBuf::from("/x"));
        // Must not panic
        assert!(!check_visited(&visited, key));
    }

    // ═══════════════════════════════════════
    // OrderedEngine
    // ═══════════════════════════════════════

    #[test]
    fn engine_no_pool_without_parallel() {
        let config = crate::config::Config::default();
        let engine = OrderedEngine::new(&config);
        assert!(engine.pool.is_none());
    }

    #[test]
    fn engine_creates_pool_with_parallel() {
        let config = Config {
            parallel: true,
            ..Default::default()
        };
        let engine = OrderedEngine::new(&config);
        assert!(engine.pool.is_some());
    }

    #[test]
    fn engine_pool_respects_threads() {
        let config = Config {
            parallel: true,
            threads: Some(2),
            ..Default::default()
        };
        let engine = OrderedEngine::new(&config);
        assert!(engine.pool.is_some());
        assert_eq!(engine.pool.as_ref().unwrap().current_num_threads(), 2);
    }
}
