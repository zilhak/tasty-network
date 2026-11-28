//! The embedded backend's tunnel: a loopback listener whose connections each open a
//! `direct-tcpip` channel to the remote port (docs/design/ssh-backends.md "내장 백엔드: 세션
//! 하나에 채널 여럿", "리스너 판정").

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use russh::ChannelOpenFailure;
use tokio::task::{AbortHandle, JoinSet};

use crate::backend::SshBackendError;
use crate::listener::{ListenerVerdict, StderrMarks, VerdictTiming};
use crate::settings::SshSettings;

use super::session::{ORIGINATOR, Session};
use super::{LOG_TARGET, run_blocking};

/// A local loopback port forwarded over an embedded session.
pub(crate) struct Tunnel {
    local_port: u16,
    remote_port: u16,
    session: Arc<Session>,
    accept: AbortHandle,
    marks: Arc<StderrMarks>,
    timing: VerdictTiming,
}

impl Tunnel {
    /// Listens on `127.0.0.1:0` and forwards each connection to `127.0.0.1:<remote_port>` of
    /// the session's host. The listener accepts before this returns.
    pub(crate) fn open(
        session: Arc<Session>,
        remote_port: u16,
        settings: &SshSettings,
    ) -> Result<Self, std::io::Error> {
        let marks = Arc::new(StderrMarks::default());
        let handle = Arc::clone(&session.conn.handle);
        let open_timeout = settings.channel_open_timeout();
        let task_marks = Arc::clone(&marks);
        let opened = run_blocking(&session.runtime, async move {
            let listener =
                tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, u16::MIN)))
                    .await
                    .map_err(|e| SshBackendError::Session {
                        detail: e.to_string(),
                    })?;
            let local_port = listener
                .local_addr()
                .map_err(|e| SshBackendError::Session {
                    detail: e.to_string(),
                })?
                .port();
            let accept = tokio::spawn(accept_loop(
                listener,
                handle,
                remote_port,
                open_timeout,
                task_marks,
            ));
            Ok((local_port, accept.abort_handle()))
        });
        let (local_port, accept) = opened.map_err(std::io::Error::other)?;
        Ok(Self {
            local_port,
            remote_port,
            session,
            accept,
            marks,
            timing: VerdictTiming::new(settings),
        })
    }

    pub(crate) fn local_port(&self) -> u16 {
        self.local_port
    }

    pub(crate) fn remote_port(&self) -> u16 {
        self.remote_port
    }

    pub(crate) fn session_id(&self) -> u64 {
        self.session.id
    }

    pub(crate) fn notes(&self) -> &[crate::inspect::ConfigNote] {
        &self.session.notes
    }

    /// Stops accepting and ends the forwarded connections. The session ends when its last
    /// handle drops.
    pub(crate) fn terminate(&self) {
        self.accept.abort();
    }

    pub(crate) fn is_alive(&self) -> bool {
        !self.accept.is_finished() && !self.session.conn.is_closed()
    }

    pub(crate) fn listener_verdict(&self) -> ListenerVerdict {
        ListenerVerdict::new(self.remote_port, Arc::clone(&self.marks), self.timing)
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// Accepts local connections until aborted; aborting it ends every forwarded connection.
async fn accept_loop(
    listener: tokio::net::TcpListener,
    handle: Arc<russh::client::Handle<super::hostkey::Client>>,
    remote_port: u16,
    open_timeout: Duration,
    marks: Arc<StderrMarks>,
) {
    let mut forwards = JoinSet::new();
    loop {
        let accepted = listener.accept().await;
        // Reap the forwards that ended so the set does not grow.
        while let Some(done) = forwards.try_join_next() {
            if let Err(e) = done {
                tracing::debug!(target: LOG_TARGET, "a forwarded connection ended: {e}");
            }
        }
        let (stream, peer) = match accepted {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(target: LOG_TARGET, "accept failed: {e}");
                continue;
            }
        };
        if handle.is_closed() {
            break;
        }
        forwards.spawn(forward(
            stream,
            peer,
            Arc::clone(&handle),
            remote_port,
            open_timeout,
            Arc::clone(&marks),
        ));
    }
}

/// One forwarded connection. A refused channel closes the local connection; only reason 2
/// (`SSH_OPEN_CONNECT_FAILED`) marks an absent remote listener.
async fn forward(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    handle: Arc<russh::client::Handle<super::hostkey::Client>>,
    remote_port: u16,
    open_timeout: Duration,
    marks: Arc<StderrMarks>,
) {
    let opened = tokio::time::timeout(
        open_timeout,
        handle.channel_open_direct_tcpip(
            ORIGINATOR,
            u32::from(remote_port),
            ORIGINATOR,
            u32::from(peer.port()),
        ),
    )
    .await;
    let channel = match opened {
        Ok(Ok(channel)) => channel,
        Ok(Err(russh::Error::ChannelOpenFailure(ChannelOpenFailure::ConnectFailed))) => {
            marks.record_connect_failure();
            return;
        }
        Ok(Err(e)) => {
            tracing::debug!(target: LOG_TARGET, "the tunnel channel did not open: {e}");
            return;
        }
        Err(_) => {
            tracing::debug!(target: LOG_TARGET, "the tunnel channel did not open in {open_timeout:?}");
            return;
        }
    };
    let mut remote = channel.into_stream();
    if let Err(e) = tokio::io::copy_bidirectional(&mut stream, &mut remote).await {
        tracing::debug!(target: LOG_TARGET, "a forwarded connection ended: {e}");
    }
}
