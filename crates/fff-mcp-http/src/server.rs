use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use axum::Router;
use tokio::sync::watch;

use crate::config::Config;
use crate::memory::{MemoryProbe, OsMemoryProbe};
use crate::registry::Registry;

const SUBSUME_INTERVAL: Duration = Duration::from_millis(500);
// How often the idle watchdog compares the activity clock with the timeout.
const IDLE_TICK: Duration = Duration::from_millis(250);

/// Handle to a running server: bound address plus a shutdown switch.
pub struct RunningServer {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
}

impl RunningServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Block until the serving thread finishes.
    pub fn wait(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Bind the configured address and serve in a background thread.
pub fn spawn(config: Config) -> std::io::Result<RunningServer> {
    spawn_with_probe(config, Arc::new(OsMemoryProbe))
}

/// Same as [`spawn`] with an injected memory probe (the test seam).
pub fn spawn_with_probe(
    config: Config,
    probe: Arc<dyn MemoryProbe>,
) -> std::io::Result<RunningServer> {
    // An unprotected non-loopback bind is a misconfiguration, not a warning.
    config
        .check_access()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;

    // Fail before binding a port when the walker cannot honour skip rules.
    let ignore = config
        .ignore_options()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;

    // The binary table is process-wide and fixed before any Root is walked.
    let merged_extensions = config
        .apply_binary_extensions()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    tracing::info!(
        builtin = fff::binary_verdict::BINARY_EXTENSIONS.len(),
        merged = merged_extensions,
        configured = config.extra_binary_extensions.len(),
        "binary extension table"
    );

    let address = SocketAddr::new(config.bind, config.port);
    let listener = TcpListener::bind(address).map_err(|error| {
        std::io::Error::new(error.kind(), format!("cannot bind {address}: {error}"))
    })?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let (shutdown, _) = watch::channel(false);
    let thread_shutdown = shutdown.clone();

    let thread = std::thread::Builder::new()
        .name("fff-mcp-http".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(?error, "failed to start runtime");
                    return;
                }
            };

            runtime.block_on(async move {
                let registry = match Registry::with_probe_and_ignore(config, probe, ignore) {
                    Ok(registry) => Arc::new(registry),
                    Err(error) => {
                        tracing::error!(%error, "failed to start registry");
                        return;
                    }
                };
                run(listener, registry, thread_shutdown).await;
            });
        })?;

    Ok(RunningServer {
        addr,
        shutdown,
        thread: Some(thread),
    })
}

async fn run(listener: TcpListener, registry: Arc<Registry>, shutdown: watch::Sender<bool>) {
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(?error, "failed to adopt listener");
            return;
        }
    };

    let app: Router = crate::api::router(registry.clone());
    tokio::spawn(housekeeping_tick(registry.clone(), shutdown.subscribe()));
    tokio::spawn(eviction_tick(registry.clone(), shutdown.subscribe()));
    tokio::spawn(idle_watchdog(registry.clone(), shutdown.clone()));
    let signal = {
        let mut receiver = shutdown.subscribe();
        async move {
            while !*receiver.borrow_and_update() {
                if receiver.changed().await.is_err() {
                    break;
                }
            }
        }
    };

    tokio::spawn(shutdown_signals(shutdown.clone()));

    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(signal)
        .await
    {
        tracing::error!(?error, "server stopped with an error");
    }

    tracing::info!("stopping every index");
    registry.shutdown();
    tracing::info!("shutdown complete");
}

// Exit after `idle_timeout` without a request; `/healthz` must not reset it.
async fn idle_watchdog(registry: Arc<Registry>, stop: watch::Sender<bool>) {
    let timeout = registry.config().idle_timeout;
    if timeout.is_zero() {
        return;
    }
    let mut stopped = stop.subscribe();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(IDLE_TICK) => {}
            _ = stopped.changed() => return,
        }
        if registry.idle_for() >= timeout {
            tracing::info!(
                idle_secs = timeout.as_secs(),
                "exiting after the idle timeout"
            );
            let _ = stop.send(true);
            return;
        }
    }
}

#[cfg(unix)]
async fn shutdown_signals(stop: watch::Sender<bool>) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(signal) => Some(signal),
        Err(error) => {
            tracing::warn!(?error, "cannot listen for SIGTERM");
            None
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received, shutting down"),
        _ = async {
            match terminate.as_mut() {
                Some(signal) => {
                    signal.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        } => tracing::info!("SIGTERM received, shutting down"),
    }
    let _ = stop.send(true);
}

#[cfg(not(unix))]
async fn shutdown_signals(stop: watch::Sender<bool>) {
    if tokio::signal::ctrl_c().await.is_ok() {
        tracing::info!("ctrl-c received, shutting down");
    }
    let _ = stop.send(true);
}

// Scans finish on their own schedule; this lets subsumption and the
// post-scan eviction trigger happen without waiting for a request.
async fn housekeeping_tick(registry: Arc<Registry>, mut stop: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(SUBSUME_INTERVAL) => {}
            _ = stop.changed() => return,
        }
        let worker = registry.clone();
        let _ = tokio::task::spawn_blocking(move || {
            worker.subsume_finished();
            worker.evict_finished_scans();
            worker.start_deferred_scans();
        })
        .await;
    }
}

// Background eviction trigger: measure the real footprint every interval.
async fn eviction_tick(registry: Arc<Registry>, mut stop: watch::Receiver<bool>) {
    let interval = registry.config().evict_interval;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = stop.changed() => return,
        }
        let worker = registry.clone();
        let _ = tokio::task::spawn_blocking(move || {
            worker.evict_now();
        })
        .await;
    }
}
