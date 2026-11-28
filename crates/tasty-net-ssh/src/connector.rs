//! The entry point of SSH connections: it chooses the backend of each attempt and opens port
//! discovery and tunnels over it (docs/design/ssh-backends.md "진입점", "선택 규칙").

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::backend::{EffectiveTarget, SelectedBackend, SshBackendError};
use crate::discovery::{PortDiscoveryConfig, PortDiscoveryError, PortMode};
use crate::inspect::{InspectedSetting, ResolverState, SshInspection, SshPurpose};
use crate::settings::{BackendChoice, SshSettings};
use crate::tunnel::{SshTunnel, TunnelError};
#[cfg(feature = "system-ssh")]
use crate::{BackendReason, SshLauncher, find_program};
use crate::{SshBackendKind, SshTarget};

/// What a port discovery read, and the remote command that read it.
#[derive(Clone, Debug)]
pub struct Discovered<T> {
    /// The decoded record.
    pub value: T,
    /// The remote command (as argument words) that gave the record.
    pub(crate) remote_argv: Vec<String>,
}

/// Chooses the SSH backend of each connection attempt and opens port discovery and tunnels
/// over it. Cloning shares the session numbering.
#[derive(Clone)]
pub struct SshConnector {
    settings: SshSettings,
    #[cfg(feature = "system-ssh")]
    launcher: SshLauncher,
    /// The next session number (from 1): one per `ssh` child of a tunnel, one per embedded
    /// SSH session.
    next_session: Arc<AtomicU64>,
}

impl fmt::Debug for SshConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("SshConnector");
        d.field("backend", &self.settings.backend());
        #[cfg(feature = "system-ssh")]
        d.field("launcher", &self.launcher);
        d.finish_non_exhaustive()
    }
}

impl SshConnector {
    /// A connector with `settings`. The system backend runs the default program
    /// ([`crate::resolve_ssh_path`]) until [`Self::with_launcher`] gives another.
    pub fn new(settings: SshSettings) -> Self {
        Self {
            #[cfg(feature = "system-ssh")]
            launcher: SshLauncher::system().with_settings(settings.clone()),
            settings,
            next_session: Arc::new(AtomicU64::new(1)),
        }
    }

    /// The system backend's program, child creation hook and host key checking. The
    /// connector's settings win over the launcher's.
    #[cfg(feature = "system-ssh")]
    pub fn with_launcher(mut self, launcher: SshLauncher) -> Self {
        self.launcher = launcher.with_settings(self.settings.clone());
        self
    }

    /// The system backend's launcher (with the connector's settings).
    #[cfg(feature = "system-ssh")]
    pub fn launcher(&self) -> &SshLauncher {
        &self.launcher
    }

    /// The settings in effect.
    pub fn settings(&self) -> &SshSettings {
        &self.settings
    }

    /// The backend an attempt for `target` would use now, and why. It looks at files only: no
    /// connection, no other program. Each attempt chooses once; its port discovery and its
    /// tunnel use that choice.
    pub fn select(&self, _target: &SshTarget) -> Result<SelectedBackend, SshBackendError> {
        let choice = self.settings.backend();
        match choice {
            BackendChoice::Auto | BackendChoice::System => self.select_system(choice),
            // `SshSettings::set_text` refuses a backend this build does not have.
            BackendChoice::Embedded => Err(SshBackendError::NotFound {
                searched: Vec::new(),
            }),
        }
    }

    #[cfg(feature = "system-ssh")]
    fn select_system(&self, choice: BackendChoice) -> Result<SelectedBackend, SshBackendError> {
        let path = std::env::var_os("PATH");
        match find_program(self.launcher.program(), path.as_deref(), cfg!(windows)) {
            Ok(found) => {
                let reason = match choice {
                    BackendChoice::Auto => BackendReason::Found {
                        path: found.clone(),
                    },
                    _ => BackendReason::Configured,
                };
                Ok(SelectedBackend::system(reason, found))
            }
            Err(searched) => Err(SshBackendError::NotFound { searched }),
        }
    }

    /// Reads the remote record over the chosen backend and decodes it with `decode`
    /// ([`crate::discover_remote_port`] for the rules). A [`crate::SshCancel`] installed on the
    /// calling thread stops it.
    pub fn discover_remote_port<T, E>(
        &self,
        selected: &SelectedBackend,
        target: &SshTarget,
        mode: PortMode,
        config: &PortDiscoveryConfig,
        decode: impl Fn(&str) -> Result<T, E>,
    ) -> Result<Discovered<T>, PortDiscoveryError> {
        match (selected.backend, selected.program()) {
            #[cfg(feature = "system-ssh")]
            (Some(SshBackendKind::System), Some(program)) => {
                let launcher = self.launcher.with_program(program);
                let runner = crate::system::discovery::SystemRun {
                    launcher: &launcher,
                    target,
                };
                let recording = crate::discovery::Recording {
                    inner: &runner,
                    last: std::sync::Mutex::new(None),
                };
                let probe =
                    crate::discovery::Probe::new(&recording, &self.settings, target, config);
                let value = crate::discovery::discover(&probe, mode, &decode)?;
                let remote_argv = recording
                    .last
                    .into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .unwrap_or_default();
                Ok(Discovered { value, remote_argv })
            }
            _ => Err(no_backend_for_discovery()),
        }
    }

    /// The mode that reaches the remote side over the chosen backend
    /// ([`crate::detect_port_mode`] for the rules).
    pub fn detect_port_mode<T, E>(
        &self,
        selected: &SelectedBackend,
        target: &SshTarget,
        config: &PortDiscoveryConfig,
        decode: impl Fn(&str) -> Result<T, E>,
    ) -> Result<PortMode, PortDiscoveryError> {
        match (selected.backend, selected.program()) {
            #[cfg(feature = "system-ssh")]
            (Some(SshBackendKind::System), Some(program)) => {
                let launcher = self.launcher.with_program(program);
                let runner = crate::system::discovery::SystemRun {
                    launcher: &launcher,
                    target,
                };
                let probe = crate::discovery::Probe::new(&runner, &self.settings, target, config);
                crate::discovery::detect(&probe, &decode)
            }
            _ => Err(no_backend_for_discovery()),
        }
    }

    /// The effective target of the port discovery that gave `discovered`. The system backend
    /// runs `ssh -G` with the discovery child's arguments (a configuration's `Match exec` runs
    /// once more); `None` when the backend does not report one.
    pub fn discovery_target<T>(
        &self,
        selected: &SelectedBackend,
        target: &SshTarget,
        discovered: &Discovered<T>,
    ) -> Result<Option<EffectiveTarget>, SshBackendError> {
        match (selected.backend, selected.program()) {
            #[cfg(feature = "system-ssh")]
            (Some(SshBackendKind::System), Some(program)) => {
                let launcher = self.launcher.with_program(program);
                let argv: Vec<&str> = discovered.remote_argv.iter().map(String::as_str).collect();
                let cmd = launcher.resolve_command(
                    target,
                    &crate::system::discovery::DISCOVERY_FORCED_OPTIONS,
                    &[],
                    &argv,
                );
                crate::system::resolve::effective_target(&launcher, target, cmd, "discovery")
                    .map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Opens a tunnel from a local loopback port to `remote_port` over the chosen backend. With
    /// `expected` (the port discovery's effective target) the tunnel's effective target is
    /// read first and a different one ends with [`TunnelError::TargetMismatch`] before anything
    /// starts.
    pub fn tunnel(
        &self,
        selected: &SelectedBackend,
        target: &SshTarget,
        remote_port: u16,
        expected: Option<&EffectiveTarget>,
    ) -> Result<SshTunnel, TunnelError> {
        match (selected.backend, selected.program()) {
            #[cfg(feature = "system-ssh")]
            (Some(SshBackendKind::System), Some(program)) => {
                let launcher = self.launcher.with_program(program);
                let effective = match expected {
                    Some(expected) => {
                        let cmd = crate::system::tunnel::tunnel_resolve_command(&launcher, target);
                        let tunnel_target = crate::system::resolve::effective_target(
                            &launcher, target, cmd, "tunnel",
                        )
                        .map_err(TunnelError::Backend)?;
                        if tunnel_target != *expected {
                            return Err(TunnelError::TargetMismatch {
                                discovery: Box::new(expected.clone()),
                                tunnel: Box::new(tunnel_target),
                            });
                        }
                        Some(tunnel_target)
                    }
                    None => None,
                };
                let tunnel =
                    crate::system::tunnel::SshTunnel::establish(&launcher, target, remote_port)?;
                let session = self.next_session.fetch_add(1, Ordering::Relaxed);
                Ok(SshTunnel::system(
                    tunnel,
                    selected.clone(),
                    effective,
                    Some(session),
                ))
            }
            _ => Err(TunnelError::Backend(SshBackendError::NotFound {
                searched: Vec::new(),
            })),
        }
    }

    /// The settings query: every setting with its value and source, the backend choice for
    /// `target` (or for the launcher's program), and the resolver's diagnostics for `purpose`.
    /// It does not connect and runs no other program.
    pub fn inspect(&self, target: Option<&SshTarget>, purpose: SshPurpose) -> SshInspection {
        let _ = purpose;
        let target = target.cloned().unwrap_or_default();
        SshInspection {
            settings: inspected_settings(&self.settings),
            backend: self.select(&target),
            resolver: ResolverState::NotBuilt,
            files: Vec::new(),
            file_notes: Vec::new(),
            target: None,
        }
    }
}

/// Every numeric and text setting with its value and source.
pub(crate) fn inspected_settings(settings: &SshSettings) -> Vec<InspectedSetting> {
    let numeric = settings.entries().into_iter().map(|e| InspectedSetting {
        key: e.key,
        value: e.value.to_string(),
        source: e.source,
    });
    let text = settings
        .text_entries()
        .into_iter()
        .map(|e| InspectedSetting {
            key: e.key,
            value: e.value.map_or_else(|| "-".to_owned(), |v| v.to_string()),
            source: e.source,
        });
    numeric.chain(text).collect()
}

fn no_backend_for_discovery() -> PortDiscoveryError {
    PortDiscoveryError::new(
        crate::PortDiscoveryFailureKind::SshConnectionFailed,
        "no SSH backend was chosen for this discovery",
    )
}

#[cfg(all(test, unix, feature = "system-ssh"))]
mod tests {
    use super::*;
    use crate::{BackendReason, PortDecodeError, ResolverState, decode_port};
    use std::path::{Path, PathBuf};

    /// A fake `ssh`: `-G` prints a target whose user depends on the session flag (`-N` for a
    /// tunnel), anything else appends its arguments to a log and prints a port.
    struct FakeSsh {
        dir: PathBuf,
        script: PathBuf,
        log: PathBuf,
    }

    impl FakeSsh {
        fn new(name: &str, tunnel_user: &str) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "tasty-net-ssh-connector-{}-{}-{name}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("dir");
            let script = dir.join("ssh");
            let log = dir.join("args.log");
            let body = format!(
                "#!/bin/sh\ncase \" $* \" in\n*\" -G \"*)\n  case \" $* \" in\n  *\" -N \"*) u='{tunnel_user}';;\n  *) u=disc;;\n  esac\n  printf 'hostname h.example\\nport 22\\nuser %s\\n' \"$u\";;\n*) echo \"$*\" >> '{log}'; echo 4242;;\nesac\n",
                log = log.display()
            );
            write_executable(&script, &body);
            Self { dir, script, log }
        }

        fn launcher(&self) -> SshLauncher {
            SshLauncher::new(&self.script)
        }

        fn logged(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    impl Drop for FakeSsh {
        fn drop(&mut self) {
            if let Err(e) = std::fs::remove_dir_all(&self.dir) {
                eprintln!("could not remove the test folder: {e}");
            }
        }
    }

    /// Writes through a child shell so no descriptor of the file stays open in this
    /// multi-threaded process when another test executes it.
    fn write_executable(path: &Path, body: &str) {
        let status = std::process::Command::new("sh")
            .args([
                "-c",
                "printf '%s' \"$1\" > \"$2\" && chmod 755 \"$2\"",
                "sh",
            ])
            .arg(body)
            .arg(path)
            .status()
            .expect("sh");
        assert!(status.success());
    }

    fn decode(text: &str) -> Result<u16, PortDecodeError> {
        decode_port(text)
    }

    #[test]
    fn auto_finds_the_launcher_program_and_names_the_file() {
        let fake = FakeSsh::new("found", "disc");
        let connector = SshConnector::new(SshSettings::new()).with_launcher(fake.launcher());
        let selected = connector.select(&SshTarget::parse("h")).expect("found");
        assert_eq!(selected.backend, Some(SshBackendKind::System));
        assert_eq!(
            selected.reason,
            BackendReason::Found {
                path: fake.script.clone()
            }
        );
        assert_eq!(selected.program(), Some(fake.script.as_path()));
        // `system` names the backend; it still runs the file it found.
        let mut settings = SshSettings::new();
        settings.set_text("ssh.backend", "system").expect("system");
        let forced = SshConnector::new(settings).with_launcher(fake.launcher());
        let selected = forced.select(&SshTarget::parse("h")).expect("found");
        assert_eq!(selected.reason, BackendReason::Configured);
        assert_eq!(selected.program(), Some(fake.script.as_path()));
    }

    #[test]
    fn a_missing_program_ends_with_not_found_and_no_fallback() {
        let missing = std::env::temp_dir().join("tasty-net-ssh-connector-no-such-ssh");
        for backend in ["auto", "system"] {
            let mut settings = SshSettings::new();
            settings.set_text("ssh.backend", backend).expect("backend");
            let connector = SshConnector::new(settings).with_launcher(SshLauncher::new(&missing));
            let error = connector.select(&SshTarget::parse("h")).unwrap_err();
            assert_eq!(
                error,
                SshBackendError::NotFound {
                    searched: vec![missing.clone()]
                },
                "{backend}"
            );
            let inspection = connector.inspect(None, SshPurpose::Tunnel);
            assert_eq!(inspection.backend, Err(error));
            assert_eq!(inspection.resolver, ResolverState::NotBuilt);
            assert!(inspection.files.is_empty() && inspection.target.is_none());
            assert!(
                inspection
                    .settings
                    .iter()
                    .any(|s| s.key == "ssh.backend" && s.value == backend),
                "{:?}",
                inspection.settings
            );
        }
    }

    #[test]
    fn discovery_runs_the_found_file_without_connection_sharing() {
        let fake = FakeSsh::new("discovery", "disc");
        let connector = SshConnector::new(SshSettings::new()).with_launcher(fake.launcher());
        let mut target = SshTarget::parse("h");
        target.extra_options = vec!["ControlMaster=auto".into()];
        let selected = connector.select(&target).expect("found");
        let found = connector
            .discover_remote_port(
                &selected,
                &target,
                PortMode::FileUnix,
                &PortDiscoveryConfig {
                    remote_program: "tasty".into(),
                    remote_args: Vec::new(),
                    data_dir: ".tasty".into(),
                    port_file: "port".into(),
                    explicit_port_file: None,
                },
                decode,
            )
            .expect("discovered");
        assert_eq!(found.value, 4242);
        let args = fake.logged();
        let at = |want: &str| args.find(want).unwrap_or_else(|| panic!("{want}: {args}"));
        // The forced options come first, so the target's ControlMaster loses.
        assert!(at("ControlMaster=no") < at("ControlMaster=auto"), "{args}");
        assert!(at("ControlPath=none") < at("ControlMaster=auto"), "{args}");
        let discovery = connector
            .discovery_target(&selected, &target, &found)
            .expect("ssh -G")
            .expect("the system backend reports it");
        assert_eq!(discovery.user, "disc");
        assert_eq!(
            (discovery.hostname.as_str(), discovery.port),
            ("h.example", 22)
        );
    }

    #[test]
    fn a_tunnel_to_another_target_is_refused_before_it_starts() {
        let fake = FakeSsh::new("mismatch", "other");
        let connector = SshConnector::new(SshSettings::new()).with_launcher(fake.launcher());
        let target = SshTarget::parse("h");
        let selected = connector.select(&target).expect("found");
        let discovery = EffectiveTarget {
            hostname: "h.example".into(),
            port: 22,
            user: "disc".into(),
            jump: String::new(),
        };
        let error = connector
            .tunnel(&selected, &target, 4242, Some(&discovery))
            .unwrap_err();
        match error {
            TunnelError::TargetMismatch {
                discovery: d,
                tunnel,
            } => {
                assert_eq!(*d, discovery);
                assert_eq!(tunnel.user, "other");
            }
            other => panic!("{other}"),
        }
        assert_eq!(fake.logged(), "", "no tunnel child was started");
    }
}
