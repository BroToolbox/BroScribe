//! Same-user, bounded-event local IPC server over the portable transport
//! (`broscribe_platform_native::ipc`: a private Unix socket or a current-user
//! named pipe; peers of another user never reach a connection). A slow
//! connection never owns the capture path or another subscriber's queue.
use crate::Daemon;
use broscribe_ipc::{AgentBuild, AgentPing, Call, IpcError, Request, Response};
use broscribe_platform_native::ipc::{self, Connection, Listener};
use std::io;

/// Synchronous process owner: no effect-bearing value escapes admission, and
/// Tokio's blocking tasks finish before the shared mode lease is released.
pub fn run_admitted<F, Fut>(data_dir: &std::path::Path, run: F) -> broscribe_core::Result<()>
where
    F: FnOnce(std::sync::Arc<broscribe_core::maintenance::ModeLease>) -> Fut,
    Fut: Future<Output = broscribe_core::Result<()>>,
{
    let lease = std::sync::Arc::new(broscribe_core::maintenance::ModeLease::acquire(
        data_dir,
        broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
    )?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run(std::sync::Arc::clone(&lease)));
    drop(runtime);
    drop(lease);
    result
}

pub async fn serve(daemon: Daemon, mut listener: Listener) -> io::Result<()> {
    daemon.start_background();
    let mut connections = tokio::task::JoinSet::new();
    let termination = termination_signal();
    tokio::pin!(termination);
    loop {
        tokio::select! {
            () = daemon.shared.shutdown.wait() => break,
            () = &mut termination => { daemon.trigger_shutdown(); break; }
            connection = accept_retry(&mut listener, async |l: &mut Listener| l.accept().await) => {
                let d = daemon.clone();
                connections.spawn(async move { let _ = serve_connection(d, connection).await; });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    daemon.finish().await;
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    drop(listener);
    Ok(())
}

/// The maintenance responder owns no normal daemon, library or subscriptions.
/// Its listener uses the same authentication, framing and accept retry as normal serving.
pub async fn serve_control(mut listener: Listener, build: Option<AgentBuild>) -> io::Result<()> {
    let build = std::sync::Arc::new(build);
    let shutdown = std::sync::Arc::new(crate::shutdown::Shutdown::new());
    let mut connections = tokio::task::JoinSet::new();
    let termination = termination_signal();
    tokio::pin!(termination);
    loop {
        tokio::select! {
            () = shutdown.wait() => break,
            () = &mut termination => break,
            connection = accept_retry(&mut listener, async |l: &mut Listener| l.accept().await) => {
                let shutdown = shutdown.clone();
                let build = build.clone();
                connections.spawn(async move {
                    let _ = serve_control_connection(connection, build, shutdown).await;
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    drop(listener);
    Ok(())
}

async fn termination_signal() {
    if ipc::termination().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Invalid requests that carry an ID receive the same protocol error in both modes.
fn parse_request(line: &str) -> Result<Request, Option<Response>> {
    Request::parse(line).map_err(|error| {
        error.id.map(|id| Response {
            id,
            outcome: Err(error.error),
        })
    })
}

async fn serve_control_connection(
    connection: Connection,
    build: std::sync::Arc<Option<AgentBuild>>,
    shutdown: std::sync::Arc<crate::shutdown::Shutdown>,
) -> io::Result<()> {
    let (mut read, mut write) = connection.into_split();
    while let Some(line) = read.read_line().await? {
        let request = match parse_request(&line) {
            Ok(request) => request,
            Err(response) => {
                if let Some(response) = response {
                    write
                        .write_line(response.to_line().map_err(io::Error::other)?)
                        .await?;
                }
                continue;
            }
        };
        let stopping = matches!(request.call, Call::Shutdown);
        let outcome = match request.call {
            Call::Ping => serde_json::to_value(AgentPing::maintenance(build.as_ref().as_ref()))
                .map_err(|error| IpcError::new("internal", error.to_string())),
            Call::Shutdown => Ok(serde_json::json!({ "stopping": true })),
            // The shared mode lease already fences capture and jobs for
            // this responder. Acknowledge the stop hold without an install fence.
            Call::ShutdownAcquire if !shutdown.is_triggered() => {
                Ok(serde_json::json!({ "held": true }))
            }
            _ => Err(IpcError::new(
                broscribe_core::maintenance::ERROR_CODE,
                "BroScribe Agent is in maintenance; stop clients and disable maintenance before relaunching",
            )),
        };
        let response = Response {
            id: request.id,
            outcome,
        };
        write
            .write_line(response.to_line().map_err(io::Error::other)?)
            .await?;
        if stopping {
            shutdown.trigger();
            return Ok(());
        }
    }
    Ok(())
}

/// Listener errors (transient, or descriptor exhaustion) must never
/// terminate capture or the helper: logged and retried, pausing briefly
/// when the system is out of descriptors or handles.
async fn accept_retry<S, T>(
    state: &mut S,
    mut accept: impl AsyncFnMut(&mut S) -> io::Result<T>,
) -> T {
    loop {
        match accept(state).await {
            Ok(connection) => return connection,
            Err(error) => {
                tracing::warn!(%error, "accepting daemon connection");
                if ipc::is_resource_exhaustion(&error) {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }
}

/// One connection: its requests are answered on its own [`ClientConnection`],
/// so an install admission it acquired and did not confirm ends when it
/// closes (read end, error or abort drop the connection handle).
async fn serve_connection(daemon: Daemon, connection: Connection) -> io::Result<()> {
    let (mut read, mut write) = connection.into_split();
    let client = daemon.connect();
    while let Some(line) = read.read_line().await? {
        let request = match parse_request(&line) {
            Ok(request) => request,
            Err(response) => {
                if let Some(response) = response {
                    write
                        .write_line(response.to_line().map_err(io::Error::other)?)
                        .await?;
                }
                continue;
            }
        };
        let subscribe = matches!(request.call, Call::Subscribe);
        let mut subscriber = subscribe.then(|| daemon.subscribe());
        let response = client.handle(request).await;
        write
            .write_line(response.to_line().map_err(io::Error::other)?)
            .await?;
        if let Some(sub) = subscriber.as_mut() {
            for event in daemon.subscription_snapshot() {
                write
                    .write_line(event.to_line().map_err(io::Error::other)?)
                    .await?;
            }
            while let Some(event) = sub.next().await {
                write
                    .write_line(event.to_line().map_err(io::Error::other)?)
                    .await?;
                // Events were dropped: the authoritative state, prompts and
                // the current reader incarnation follow at once, so the
                // client never keeps acting on what it missed.
                for event in daemon.resync_after(&event) {
                    write
                        .write_line(event.to_line().map_err(io::Error::other)?)
                        .await?;
                }
            }
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn maintenance_responder_acknowledges_a_stop_hold() {
        let temp = tempfile::tempdir().unwrap();
        let mut listener = Listener::bind(temp.path()).unwrap();
        let endpoint = ipc::Endpoint::new(temp.path());
        let shutdown = std::sync::Arc::new(crate::shutdown::Shutdown::new());
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(async move {
            let connection = listener.accept().await.unwrap();
            serve_control_connection(connection, std::sync::Arc::new(None), server_shutdown)
                .await
                .unwrap();
        });
        tokio::task::spawn_blocking(move || {
            let mut client =
                ipc::Client::connect_daemon(&endpoint, std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
            let answer = client.call(Call::ShutdownAcquire).unwrap();
            assert_eq!(answer, serde_json::json!({ "held": true }));
            assert!(
                matches!(client.call(Call::Status), Err(ipc::CallError::Remote(error))
                if error.code == broscribe_core::maintenance::ERROR_CODE)
            );
        })
        .await
        .unwrap();
        server.await.unwrap();
        assert!(!shutdown.is_triggered());
    }

    #[tokio::test]
    async fn record_accept_retries_listener_errors_until_a_connection() {
        let mut calls = 0;
        let result = accept_retry(&mut calls, async |calls: &mut i32| {
            *calls += 1;
            match *calls {
                1 => Err(io::Error::from(io::ErrorKind::ConnectionAborted)),
                2 => Err(io::Error::from(io::ErrorKind::ConnectionReset)),
                3 => Err(io::Error::other("synthetic listener fault")),
                _ => Ok(7),
            }
        })
        .await;
        assert_eq!(result, 7);
        assert_eq!(calls, 4);
    }
}
