//! The tunnel handle both backends give: a loopback port forwarded to a remote port
//! (docs/design/ssh-backends.md "터널·exec·SFTP"), and why one did not come up.

use std::error::Error;
use std::fmt;
use std::io;
use std::time::Duration;

use crate::backend::{EffectiveTarget, SelectedBackend, SshBackendError};
use crate::listener::ListenerVerdict;
// The child's exit status, from the system module (this module builds without a process).
#[cfg(feature = "system-ssh")]
use crate::system::ExitStatus;
#[cfg(feature = "system-ssh")]
use crate::{SshLauncher, SshTarget};

/// Why a tunnel did not come up.
#[derive(Debug)]
#[non_exhaustive]
pub enum TunnelError {
    /// No local port could be reserved for the forward.
    LocalPort(io::Error),
    /// The ssh child could not be created.
    #[cfg(feature = "system-ssh")]
    Spawn(io::Error),
    /// ssh exited before the local end was ready (authentication or forward failure, for
    /// example). Its stderr was passed on before this error returned.
    #[cfg(feature = "system-ssh")]
    ExitedBeforeReady { status: ExitStatus },
    /// The local end did not accept connections in time.
    ReadyTimeout { local_port: u16, waited: Duration },
    /// The state of the ssh child could not be read.
    #[cfg(feature = "system-ssh")]
    Wait(io::Error),
    /// No backend could open it, or the tunnel's effective target could not be read.
    Backend(SshBackendError),
    /// The tunnel would go to another effective target than the port discovery did
    /// (`Match command`, `Match sessiontype`). Nothing was started.
    TargetMismatch {
        /// The port discovery's target.
        discovery: Box<EffectiveTarget>,
        /// The tunnel's target.
        tunnel: Box<EffectiveTarget>,
    },
}

impl fmt::Display for TunnelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalPort(_) => f.write_str("Could not reserve a local port for the SSH tunnel."),
            #[cfg(feature = "system-ssh")]
            Self::Spawn(_) => f.write_str("Could not start ssh for the tunnel."),
            #[cfg(feature = "system-ssh")]
            Self::ExitedBeforeReady { status } => write!(
                f,
                "ssh exited before the tunnel was ready ({status}); authentication or the port forward may have failed."
            ),
            Self::ReadyTimeout { local_port, waited } => write!(
                f,
                "The SSH tunnel on local port {local_port} was not ready within {waited:?}."
            ),
            #[cfg(feature = "system-ssh")]
            Self::Wait(_) => f.write_str("Could not read the state of the ssh tunnel process."),
            Self::Backend(e) => write!(f, "{e}"),
            Self::TargetMismatch { discovery, tunnel } => write!(
                f,
                "The SSH tunnel would reach {}@{}:{} but the port discovery reached {}@{}:{}.",
                tunnel.user,
                tunnel.hostname,
                tunnel.port,
                discovery.user,
                discovery.hostname,
                discovery.port
            ),
        }
    }
}

impl Error for TunnelError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LocalPort(e) => Some(e),
            #[cfg(feature = "system-ssh")]
            Self::Spawn(e) | Self::Wait(e) => Some(e),
            Self::Backend(e) => Some(e),
            #[cfg(feature = "system-ssh")]
            Self::ExitedBeforeReady { .. } => None,
            Self::ReadyTimeout { .. } | Self::TargetMismatch { .. } => None,
        }
    }
}

/// A local loopback port forwarded to a port on the remote host, over the backend chosen for
/// its attempt. Dropping it closes the forward (the system backend kills and reaps its `ssh`
/// child).
///
/// The remote process the tunnel forwards to runs on its own. All methods take `&self`, so
/// several channels can share one tunnel through an `Arc`; the last reference closes it.
pub struct SshTunnel {
    inner: Inner,
    backend: SelectedBackend,
    target: Option<EffectiveTarget>,
    session_id: Option<u64>,
}

enum Inner {
    #[cfg(feature = "system-ssh")]
    System(crate::system::tunnel::SshTunnel),
}

impl SshTunnel {
    /// Starts `ssh -N -L 127.0.0.1:<local>:127.0.0.1:<remote_port>` for `target` with
    /// `launcher`'s program as given, and waits until the local end accepts connections (the
    /// system backend; [`crate::SshConnector::tunnel`] chooses the backend).
    #[cfg(feature = "system-ssh")]
    pub fn establish(
        launcher: &SshLauncher,
        target: &SshTarget,
        remote_port: u16,
    ) -> Result<Self, TunnelError> {
        let tunnel = crate::system::tunnel::SshTunnel::establish(launcher, target, remote_port)?;
        Ok(Self::system(
            tunnel,
            SelectedBackend::system(
                crate::backend::BackendReason::Configured,
                launcher.program().to_path_buf(),
            ),
            None,
            None,
        ))
    }

    #[cfg(feature = "system-ssh")]
    pub(crate) fn system(
        tunnel: crate::system::tunnel::SshTunnel,
        backend: SelectedBackend,
        target: Option<EffectiveTarget>,
        session_id: Option<u64>,
    ) -> Self {
        Self {
            inner: Inner::System(tunnel),
            backend,
            target,
            session_id,
        }
    }

    /// Local end: connect to `127.0.0.1:local_port`.
    pub fn local_port(&self) -> u16 {
        match &self.inner {
            #[cfg(feature = "system-ssh")]
            Inner::System(t) => t.local_port(),
        }
    }

    /// The remote port the tunnel forwards to.
    pub fn remote_port(&self) -> u16 {
        match &self.inner {
            #[cfg(feature = "system-ssh")]
            Inner::System(t) => t.remote_port(),
        }
    }

    /// Closes this tunnel now (the system backend kills and reaps its child). A host calls it
    /// on its own retirement worker; dropping the tunnel does the same synchronously.
    pub fn terminate_and_reap(&self) -> io::Result<()> {
        match &self.inner {
            #[cfg(feature = "system-ssh")]
            Inner::System(t) => t.terminate_and_reap().map(drop),
        }
    }

    /// Whether the forward still runs (the system backend: its `ssh` child).
    pub fn is_alive(&self) -> bool {
        match &self.inner {
            #[cfg(feature = "system-ssh")]
            Inner::System(t) => t.is_alive(),
        }
    }

    /// The absent-listener verdict for the next channel through this tunnel. Take it right
    /// before opening that channel; it counts only refusals after this call
    /// ([`ListenerVerdict`]). It stays usable after the tunnel moved elsewhere or dropped.
    pub fn listener_verdict(&self) -> ListenerVerdict {
        match &self.inner {
            #[cfg(feature = "system-ssh")]
            Inner::System(t) => t.listener_verdict(),
        }
    }

    /// The backend of this tunnel and why it was chosen.
    pub fn backend(&self) -> &SelectedBackend {
        &self.backend
    }

    /// The effective target the tunnel goes to, when it was resolved before the tunnel opened
    /// ([`crate::SshConnector::tunnel`]).
    pub fn effective_target(&self) -> Option<&EffectiveTarget> {
        self.target.as_ref()
    }

    /// The number of the SSH session the tunnel runs on, within its connector (the system
    /// backend: its `ssh` child). `None` for a tunnel not opened through a connector.
    pub fn session_id(&self) -> Option<u64> {
        self.session_id
    }
}

impl fmt::Debug for SshTunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshTunnel")
            .field("backend", &self.backend.backend)
            .field("local_port", &self.local_port())
            .field("remote_port", &self.remote_port())
            .finish_non_exhaustive()
    }
}
