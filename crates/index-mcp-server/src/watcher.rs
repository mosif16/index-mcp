use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::ingest::IngestError;
use crate::ingest::{ingest_codebase, IngestParams, DEFAULT_EXCLUDE_GLOBS, DEFAULT_INCLUDE_GLOBS};

#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
    #[error("failed to resolve watch root '{path}': {source}")]
    InvalidRoot {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("notify error: {0}")]
    Notify(#[from] notify::Error),
}

#[derive(Clone)]
pub struct WatcherOptions {
    pub root: PathBuf,
    pub database_name: String,
    pub debounce: Duration,
    pub run_initial: bool,
    pub quiet: bool,
}

pub struct WatcherHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
    watcher: Option<RecommendedWatcher>,
}

impl WatcherHandle {
    pub async fn stop(mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        if let Some(watcher) = self.watcher.take() {
            drop(watcher);
        }
        let _ = self.task.await;
    }
}

struct WatchContext {
    absolute_root: PathBuf,
    database_name: String,
    include_matcher: Option<GlobSet>,
    exclude_matcher: Option<GlobSet>,
    include_patterns: Vec<String>,
    exclude_patterns: Vec<String>,
    debounce: Duration,
    quiet: bool,
}

struct WatchState {
    changed_paths: HashSet<String>,
    removed_paths: HashSet<String>,
    ingest_in_progress: bool,
    rerun_requested: bool,
    timer_handle: Option<tokio::task::JoinHandle<()>>,
}

pub async fn start_ingest_watcher(options: WatcherOptions) -> Result<WatcherHandle, WatcherError> {
    let WatcherOptions {
        root,
        database_name,
        debounce,
        run_initial,
        quiet,
    } = options;

    let absolute_root = resolve_root(&root)?;

    let include_patterns: Vec<String> = DEFAULT_INCLUDE_GLOBS
        .iter()
        .map(|value| value.to_string())
        .collect();
    let mut exclude_patterns: Vec<String> = DEFAULT_EXCLUDE_GLOBS
        .iter()
        .map(|value| value.to_string())
        .collect();
    exclude_patterns.push(format!("**/{}", database_name));
    exclude_patterns.push(format!("**/{}-wal", database_name));
    exclude_patterns.push(format!("**/{}-shm", database_name));

    let include_matcher = compile_globs(&include_patterns)?;
    let exclude_matcher = compile_globs(&exclude_patterns)?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    let mut watcher = RecommendedWatcher::new(
        move |result| {
            if let Ok(event) = result {
                let _ = event_tx.send(event);
            }
        },
        Config::default(),
    )?;

    watcher.watch(&absolute_root, RecursiveMode::Recursive)?;

    let context = Arc::new(WatchContext {
        absolute_root: absolute_root.clone(),
        database_name: database_name.clone(),
        include_matcher,
        exclude_matcher,
        include_patterns,
        exclude_patterns,
        debounce,
        quiet,
    });

    let state = Arc::new(Mutex::new(WatchState {
        changed_paths: HashSet::new(),
        removed_paths: HashSet::new(),
        ingest_in_progress: false,
        rerun_requested: false,
        timer_handle: None,
    }));

    if run_initial {
        let mut guard = state.lock().await;
        schedule_ingest_locked(
            &mut guard,
            state.clone(),
            context.clone(),
            Duration::from_millis(0),
        );
    }

    let loop_state = state.clone();
    let loop_context = context.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    break;
                }
                Some(event) = event_rx.recv() => {
                    process_event(&loop_context, &loop_state, event).await;
                }
                else => break,
            }
        }

        if let Some(handle) = loop_state.lock().await.timer_handle.take() {
            handle.abort();
        }

        // wait for any ongoing ingest cycles to finish
        loop {
            if !loop_state.lock().await.ingest_in_progress {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    Ok(WatcherHandle {
        shutdown: Some(shutdown_tx),
        task,
        watcher: Some(watcher),
    })
}

async fn process_event(context: &Arc<WatchContext>, state: &Arc<Mutex<WatchState>>, event: Event) {
    let mut guard = state.lock().await;

    for path in event.paths {
        if let Some(relative) = normalize_relative_path(&context.absolute_root, &path) {
            let relative_path = Path::new(&relative);
            if !should_track(context, relative_path) {
                continue;
            }

            match event.kind {
                EventKind::Remove(_) => {
                    guard.changed_paths.remove(&relative);
                    guard.removed_paths.insert(relative);
                }
                _ => {
                    guard.removed_paths.remove(&relative);
                    guard.changed_paths.insert(relative);
                }
            }
        }
    }

    if !guard.changed_paths.is_empty() || !guard.removed_paths.is_empty() {
        schedule_ingest_locked(&mut guard, state.clone(), context.clone(), context.debounce);
    }
}

fn schedule_ingest_locked(
    guard: &mut WatchState,
    state: Arc<Mutex<WatchState>>,
    context: Arc<WatchContext>,
    delay: Duration,
) {
    if let Some(handle) = guard.timer_handle.take() {
        handle.abort();
    }

    guard.timer_handle = Some(tokio::spawn(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        execute_ingest(state, context).await;
    }));
}

async fn execute_ingest(state: Arc<Mutex<WatchState>>, context: Arc<WatchContext>) {
    let (paths, removed) = {
        let mut guard = state.lock().await;
        if guard.ingest_in_progress {
            guard.rerun_requested = true;
            return;
        }
        guard.ingest_in_progress = true;
        guard.rerun_requested = false;
        let paths = guard.changed_paths.drain().collect::<Vec<_>>();
        let removed = guard.removed_paths.drain().collect::<Vec<_>>();
        (paths, removed)
    };

    let mut target_paths: HashSet<String> = paths.into_iter().collect();
    target_paths.extend(removed.into_iter());
    let target_list: Vec<String> = target_paths.into_iter().collect();

    if let Err(error) = run_ingest(&context, &target_list).await {
        tracing::error!(?error, "Watcher ingest failed");
    }

    let mut guard = state.lock().await;
    guard.ingest_in_progress = false;
    if guard.rerun_requested {
        guard.rerun_requested = false;
        schedule_ingest_locked(&mut guard, state.clone(), context, Duration::from_millis(0));
    }
}

async fn run_ingest(context: &WatchContext, paths: &[String]) -> Result<(), IngestError> {
    let params = IngestParams {
        root: Some(context.absolute_root.to_string_lossy().to_string()),
        include: Some(context.include_patterns.clone()),
        exclude: Some(context.exclude_patterns.clone()),
        database_name: Some(context.database_name.clone()),
        max_file_size_bytes: None,
        store_file_content: None,
        paths: if paths.is_empty() {
            None
        } else {
            Some(paths.to_vec())
        },
        auto_evict: None,
        max_database_size_bytes: None,
        embedding: None,
    };

    if !context.quiet {
        tracing::info!(
            changed_paths = paths.len(),
            database = %context.database_name,
            "Watcher ingest scheduled"
        );
    }

    ingest_codebase(params).await.map(|result| {
        if !context.quiet {
            tracing::info!(
                ingested = result.ingested_file_count,
                deleted = result.deleted_paths.len(),
                skipped = result.skipped.len(),
                duration_ms = result.duration_ms,
                "Watcher ingest completed"
            );
        }
    })
}

fn should_track(context: &WatchContext, relative: &Path) -> bool {
    if let Some(include) = &context.include_matcher {
        if !include.is_match(relative) {
            return false;
        }
    }

    if let Some(exclude) = &context.exclude_matcher {
        if exclude.is_match(relative) {
            return false;
        }
    }

    true
}

fn normalize_relative_path(root: &Path, candidate: &Path) -> Option<String> {
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };

    let relative = absolute.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(relative.to_string_lossy().replace('\\', "/"))
}

fn compile_globs(patterns: &[String]) -> Result<Option<GlobSet>, WatcherError> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .map_err(|error| WatcherError::Notify(notify::Error::generic(&error.to_string())))?;
        builder.add(glob);
    }
    builder
        .build()
        .map(Some)
        .map_err(|error| WatcherError::Notify(notify::Error::generic(&error.to_string())))
}

fn resolve_root(root: &Path) -> Result<PathBuf, WatcherError> {
    let candidate = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| WatcherError::InvalidRoot {
                path: root.to_string_lossy().to_string(),
                source,
            })?
            .join(root)
    };

    let metadata = std::fs::metadata(&candidate).map_err(|source| WatcherError::InvalidRoot {
        path: candidate.to_string_lossy().to_string(),
        source,
    })?;

    if !metadata.is_dir() {
        return Err(WatcherError::InvalidRoot {
            path: candidate.to_string_lossy().to_string(),
            source: std::io::Error::new(std::io::ErrorKind::Other, "path is not a directory"),
        });
    }

    Ok(candidate)
}
