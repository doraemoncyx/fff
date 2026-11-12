use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use axum::Router;
use tokio::sync::watch;

use crate::config::Config;
use crate::registry::Registry;

const SUBSUME_INTERVAL: Duration = Duration::from_millis(500);

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
    let listener = TcpListener::bind(SocketAddr::new(config.bind, config.port))?;
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
                let registry = match Registry::new(config) {
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
    tokio::spawn(subsumption_tick(registry.clone(), shutdown.subscribe()));
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

    let ctrlc_sender = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("ctrl-c received, shutting down");
            let _ = ctrlc_sender.send(true);
        }
    });

    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(signal)
        .await
    {
        tracing::error!(?error, "server stopped with an error");
    }

    registry.shutdown();
}

/// Scans finish on their own schedule; this lets subsumption happen without
/// waiting for the next request to arrive.
async fn subsumption_tick(registry: Arc<Registry>, mut stop: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(SUBSUME_INTERVAL) => registry.subsume_finished(),
            _ = stop.changed() => return,
        }
    }
}
