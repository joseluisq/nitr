//! Dev-mode file watcher: a save triggers the pool rebuild immediately,
//! instead of being noticed by the next request's mtime check.
//!
//! Watches the handler script's directory tree (which covers `require`d
//! modules and `routes/`), the configuration script, and the templates
//! directory. Events are debounced — editors emit a burst per save — and
//! then feed the same reload channel `SIGHUP` uses, so a dev-mode save and
//! an operator reload are one code path. Static files need no watching:
//! they are read from disk per request.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::Watcher as _;

use crate::config::Config;

/// How long a burst of events must be quiet before one reload fires.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// How often the watcher thread checks whether the server stopped.
const STOP_POLL: Duration = Duration::from_millis(500);

/// Keeps the watcher thread alive for as long as the server serves;
/// dropping it disconnects the stop channel, which ends the thread.
pub(crate) struct WatchGuard {
    _stop: std::sync::mpsc::Sender<()>,
}

/// Whether an event is worth a rebuild: content changes only — reads and
/// metadata-only churn must not trigger reload storms.
fn relevant(event: &notify::Event) -> bool {
    use notify::EventKind;
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// The directories dev mode should react to.
fn watch_roots(cfg: &Config) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push = |path: Option<&Path>| {
        // A bare `app.lua` has the empty path as its parent: that is the
        // working directory.
        let path = match path {
            Some(p) if p.as_os_str().is_empty() => Some(Path::new(".")),
            other => other,
        };
        if let Some(path) = path
            && path.exists()
            && !roots.iter().any(|r| path.starts_with(r))
        {
            roots.push(path.to_path_buf());
        }
    };
    // The handler's whole directory: `require` is confined to it, so any
    // file in it can be part of the application.
    push(cfg.handler_script.parent());
    push(cfg.config_script.as_deref().and_then(Path::parent));
    push(cfg.templating.dir.as_deref());
    roots
}

/// Starts watching; changed files send on `reload` after the debounce.
/// Returns `None` when there is nothing to watch.
///
/// Everything — watcher creation, the recursive registration walk, and
/// the debounce loop — runs on its own thread: registering watches over a
/// large tree can be slow, and `serve()` must never wait on it (a CI
/// hang taught this the hard way). Dropping the returned guard stops the
/// thread and with it the watcher.
pub(crate) fn spawn(cfg: &Config, reload: tokio::sync::mpsc::Sender<()>) -> Option<WatchGuard> {
    let roots = watch_roots(cfg);
    if roots.is_empty() {
        return None;
    }

    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut watcher =
            match notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res
                    && relevant(&event)
                {
                    let _ = tx.send(());
                }
            }) {
                Ok(watcher) => watcher,
                Err(err) => {
                    tracing::warn!(
                        "dev-mode file watcher unavailable ({err}); reload via SIGHUP instead"
                    );
                    return;
                }
            };

        for root in &roots {
            if let Err(err) = watcher.watch(root, notify::RecursiveMode::Recursive) {
                tracing::warn!("cannot watch {} for changes: {err}", root.display());
            }
        }
        tracing::debug!(
            "watching {} for changes",
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );

        // The debounce loop: wait for a burst to go quiet, then request
        // one reload. Interleaved with the stop signal so the thread (and
        // the watcher it owns) dies promptly when the server stops.
        loop {
            match rx.recv_timeout(STOP_POLL) {
                Ok(()) => {
                    while rx.recv_timeout(DEBOUNCE).is_ok() {}
                    // A full channel means a reload is already queued.
                    let _ = reload.try_send(());
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
            match stop_rx.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                _ => return,
            }
        }
    });

    Some(WatchGuard { _stop: stop_tx })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TemplatingConfig;

    #[test]
    fn roots_deduplicate_and_skip_missing_paths() {
        let dir = std::env::temp_dir().join(format!("nitr-watch-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("templates")).expect("mkdir");
        std::fs::write(dir.join("app.lua"), "-- app").expect("write");
        std::fs::write(dir.join("config.lua"), "-- config").expect("write");

        let mut cfg = Config {
            handler_script: dir.join("app.lua"),
            config_script: Some(dir.join("config.lua")),
            templating: TemplatingConfig {
                dir: Some(dir.join("templates")),
            },
            ..Config::default()
        };
        // Config script shares the handler's directory; templates are
        // inside it too — one root covers everything.
        let roots = watch_roots(&cfg);
        assert_eq!(roots, vec![dir.clone()]);

        // A missing templates dir elsewhere is skipped rather than fatal.
        cfg.templating.dir = Some(PathBuf::from("/nonexistent/templates"));
        let roots = watch_roots(&cfg);
        assert_eq!(roots, vec![dir.clone()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_content_events_are_relevant() {
        use notify::{Event, EventKind};
        let content = Event::new(EventKind::Modify(notify::event::ModifyKind::Any));
        assert!(relevant(&content));
        let access = Event::new(EventKind::Access(notify::event::AccessKind::Any));
        assert!(!relevant(&access));
    }
}
