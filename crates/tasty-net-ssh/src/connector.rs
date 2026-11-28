//! The entry point of SSH connections: it chooses the backend of each attempt and opens port
//! discovery and tunnels over it (docs/design/ssh-backends.md "진입점", "선택 규칙").

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
#[cfg(feature = "system-ssh")]
use std::sync::atomic::Ordering;

use crate::backend::{EffectiveTarget, SelectedBackend, SshBackendError};
use crate::discovery::{PortDiscoveryConfig, PortDiscoveryError, PortMode};
use crate::inspect::{
    ConfigNote, InspectedSetting, ResolvedTarget, ResolverState, SshInspection, SshPurpose,
};
use crate::prompt::SshPrompt;
use crate::settings::{BackendChoice, SshSettings};
use crate::tunnel::{SshTunnel, TunnelError};
use crate::{BackendReason, SshBackendKind, SshTarget};
#[cfg(feature = "system-ssh")]
use crate::{SshLauncher, find_program};

/// What a port discovery read, and the remote command that read it.
#[derive(Clone, Debug)]
pub struct Discovered<T> {
    /// The decoded record.
    pub value: T,
    /// The remote command (as argument words) that gave the record: the system backend reads
    /// the discovery's target with it (`ssh -G`, [`SshConnector::discovery_target`]).
    #[cfg_attr(not(feature = "system-ssh"), allow(dead_code))]
    pub(crate) remote_argv: Vec<String>,
    /// The effective target the run that gave the record reached, when the backend resolves
    /// it for the run (the embedded backend): the tunnel is compared with this one.
    #[cfg_attr(not(feature = "embedded-ssh"), allow(dead_code))]
    pub(crate) target: Option<EffectiveTarget>,
}

/// Chooses the SSH backend of each connection attempt and opens port discovery and tunnels
/// over it. Cloning shares the session numbering (and the embedded backend's runtime and open
/// sessions).
#[derive(Clone)]
pub struct SshConnector {
    settings: SshSettings,
    #[cfg(feature = "system-ssh")]
    launcher: SshLauncher,
    prompt: Option<Arc<dyn SshPrompt>>,
    /// Whether the embedded backend puts `StrictHostKeyChecking=accept-new` in the option
    /// order (the system backend's launcher keeps its own).
    #[cfg(feature = "embedded-ssh")]
    verify: bool,
    #[cfg(feature = "embedded-ssh")]
    hook: Option<crate::embedded::SessionHook>,
    #[cfg(feature = "embedded-ssh")]
    embedded: Arc<crate::embedded::Embedded>,
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
        d.field("prompt", &self.prompt.is_some());
        #[cfg(feature = "embedded-ssh")]
        d.field("embedded", &self.embedded);
        d.finish_non_exhaustive()
    }
}

/// The settings query's answer when no resolver can say more.
fn empty_inspection(
    settings: Vec<InspectedSetting>,
    backend: Result<SelectedBackend, SshBackendError>,
) -> SshInspection {
    SshInspection {
        settings,
        backend,
        resolver: ResolverState::NotBuilt,
        files: Vec::new(),
        file_notes: Vec::new(),
        target: None,
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
            prompt: None,
            #[cfg(feature = "embedded-ssh")]
            verify: false,
            #[cfg(feature = "embedded-ssh")]
            hook: None,
            #[cfg(feature = "embedded-ssh")]
            embedded: Arc::default(),
            next_session: Arc::new(AtomicU64::new(1)),
        }
    }

    /// The system backend's program, child creation hook and host key checking. The
    /// connector's settings win over the launcher's, and the launcher's `with_verify` becomes
    /// the connector's ([`Self::with_verify`]).
    #[cfg(feature = "system-ssh")]
    pub fn with_launcher(mut self, launcher: SshLauncher) -> Self {
        #[cfg(feature = "embedded-ssh")]
        {
            self.verify = launcher.verifies_host_keys();
        }
        self.launcher = launcher.with_settings(self.settings.clone());
        self
    }

    /// Puts `StrictHostKeyChecking=accept-new` in the option order of both backends (place 3
    /// of docs/design/ssh-backends.md "옵션 적용 순서").
    pub fn with_verify(mut self, verify: bool) -> Self {
        #[cfg(feature = "embedded-ssh")]
        {
            self.verify = verify;
        }
        #[cfg(feature = "system-ssh")]
        {
            self.launcher = self.launcher.with_verify(verify);
        }
        self
    }

    /// The system backend's launcher (with the connector's settings).
    #[cfg(feature = "system-ssh")]
    pub fn launcher(&self) -> &SshLauncher {
        &self.launcher
    }

    /// The callback the embedded backend asks for passphrases, passwords,
    /// keyboard-interactive answers and unknown host keys. Without one it asks nothing.
    pub fn with_prompt(mut self, prompt: Arc<dyn SshPrompt>) -> Self {
        self.prompt = Some(prompt);
        self
    }

    /// A hook that sees each new embedded session (tests end sessions with it, like the system
    /// backend's [`crate::SpawnHook`] sees each `ssh` child).
    #[cfg(feature = "embedded-ssh")]
    pub fn with_session_hook(mut self, hook: crate::embedded::SessionHook) -> Self {
        self.hook = Some(hook);
        self
    }

    /// The settings in effect.
    pub fn settings(&self) -> &SshSettings {
        &self.settings
    }

    /// The ssh options from settings as either backend uses them for `target`
    /// (`SshLauncher::applied_options` for the order): both pass them in the same place of
    /// the option order (docs/design/ssh-backends.md "옵션 적용 순서").
    pub fn applied_options(&self, target: &SshTarget) -> Vec<crate::AppliedOption> {
        crate::applied_options(&self.settings, target)
    }

    /// Whether the backends put `StrictHostKeyChecking=accept-new` in the option order.
    #[cfg(feature = "embedded-ssh")]
    fn verify(&self) -> bool {
        self.verify
    }

    #[cfg(feature = "embedded-ssh")]
    fn embedded_ctx(&self) -> crate::embedded::Ctx<'_> {
        crate::embedded::Ctx {
            shared: &self.embedded,
            settings: &self.settings,
            prompt: self.prompt.as_ref(),
            hook: self.hook.as_ref(),
            verify: self.verify(),
            next_session: &self.next_session,
        }
    }

    /// The backend an attempt for `target` would use now, and why. It looks at files only: no
    /// connection, no other program. Each attempt chooses once; its port discovery and its
    /// tunnel use that choice. `auto` falls back to the embedded backend only when the system
    /// program file is not found, and only in a build that has both.
    pub fn select(&self, _target: &SshTarget) -> Result<SelectedBackend, SshBackendError> {
        match self.settings.backend() {
            BackendChoice::Auto => self.select_auto(),
            BackendChoice::System => self.select_system(BackendReason::Configured),
            // `SshSettings::set_text` refuses a backend this build does not have.
            BackendChoice::Embedded => self.select_embedded(BackendReason::Configured),
        }
    }

    #[cfg(feature = "system-ssh")]
    fn select_auto(&self) -> Result<SelectedBackend, SshBackendError> {
        match self.select_system(BackendReason::Found {
            path: std::path::PathBuf::new(),
        }) {
            Err(SshBackendError::NotFound { searched }) if cfg!(feature = "embedded-ssh") => {
                self.select_embedded(BackendReason::NotFound { searched })
            }
            other => other,
        }
    }

    #[cfg(not(feature = "system-ssh"))]
    fn select_auto(&self) -> Result<SelectedBackend, SshBackendError> {
        self.select_embedded(BackendReason::SystemNotBuilt)
    }

    /// The system backend if its program file is found. `reason` is `Configured`, or `Found`
    /// (whose path is filled with the file).
    #[cfg(feature = "system-ssh")]
    fn select_system(&self, reason: BackendReason) -> Result<SelectedBackend, SshBackendError> {
        let path = std::env::var_os("PATH");
        match find_program(self.launcher.program(), path.as_deref(), cfg!(windows)) {
            Ok(found) => {
                let reason = match reason {
                    BackendReason::Found { .. } => BackendReason::Found {
                        path: found.clone(),
                    },
                    other => other,
                };
                Ok(SelectedBackend::system(reason, found))
            }
            Err(searched) => Err(SshBackendError::NotFound { searched }),
        }
    }

    #[cfg(not(feature = "system-ssh"))]
    fn select_system(&self, _reason: BackendReason) -> Result<SelectedBackend, SshBackendError> {
        Err(SshBackendError::NotFound {
            searched: Vec::new(),
        })
    }

    #[cfg(feature = "embedded-ssh")]
    fn select_embedded(&self, reason: BackendReason) -> Result<SelectedBackend, SshBackendError> {
        Ok(SelectedBackend::embedded(reason))
    }

    #[cfg(not(feature = "embedded-ssh"))]
    fn select_embedded(&self, reason: BackendReason) -> Result<SelectedBackend, SshBackendError> {
        match reason {
            BackendReason::NotFound { searched } => Err(SshBackendError::NotFound { searched }),
            _ => Err(SshBackendError::NotFound {
                searched: Vec::new(),
            }),
        }
    }

    /// The embedded backend's resolution of `target` for `purpose`, with its notes, before
    /// anything connects: `None` for the system backend. A refused resolution ends with
    /// [`SshBackendError::ConfigRefused`] and nothing reaches the network.
    pub fn resolve(
        &self,
        selected: &SelectedBackend,
        target: &SshTarget,
        purpose: &SshPurpose,
    ) -> Result<Option<(ResolvedTarget, Vec<ConfigNote>)>, SshBackendError> {
        #[cfg(feature = "embedded-ssh")]
        if selected.backend == Some(SshBackendKind::Embedded) {
            let ctx = self.embedded_ctx();
            let set = crate::embedded::resolve::load(ctx.settings)?;
            let chain = crate::embedded::resolve::resolve_chain(
                &set,
                ctx.settings,
                target,
                embedded_purpose(purpose),
                ctx.verify,
            )?;
            return Ok(Some((chain.resolved(), chain.notes)));
        }
        let _ = (selected, target, purpose);
        Ok(None)
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
        let run = |runner: &dyn crate::discovery::RemoteRun| {
            let recording = crate::discovery::Recording {
                inner: runner,
                last: std::sync::Mutex::new(None),
            };
            let probe = crate::discovery::Probe::new(&recording, &self.settings, target, config);
            let value = crate::discovery::discover(&probe, mode, &decode)?;
            let recorded = recording
                .last
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unwrap_or_default();
            Ok(Discovered {
                value,
                remote_argv: recorded.argv,
                target: recorded.target,
            })
        };
        match (selected.backend, selected.program()) {
            #[cfg(feature = "system-ssh")]
            (Some(SshBackendKind::System), Some(program)) => {
                let launcher = self.launcher.with_program(program);
                run(&crate::system::discovery::SystemRun {
                    launcher: &launcher,
                    target,
                })
            }
            #[cfg(feature = "embedded-ssh")]
            (Some(SshBackendKind::Embedded), _) => run(
                &crate::embedded::discovery::EmbeddedRun::new(self.embedded_ctx(), target),
            ),
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
        let run = |runner: &dyn crate::discovery::RemoteRun| {
            let probe = crate::discovery::Probe::new(runner, &self.settings, target, config);
            crate::discovery::detect(&probe, &decode)
        };
        match (selected.backend, selected.program()) {
            #[cfg(feature = "system-ssh")]
            (Some(SshBackendKind::System), Some(program)) => {
                let launcher = self.launcher.with_program(program);
                run(&crate::system::discovery::SystemRun {
                    launcher: &launcher,
                    target,
                })
            }
            #[cfg(feature = "embedded-ssh")]
            (Some(SshBackendKind::Embedded), _) => run(
                &crate::embedded::discovery::EmbeddedRun::new(self.embedded_ctx(), target),
            ),
            _ => Err(no_backend_for_discovery()),
        }
    }

    /// The effective target of the port discovery that gave `discovered`. The system backend
    /// runs `ssh -G` with the discovery child's arguments (a configuration's `Match exec` runs
    /// once more); the embedded backend gives the target the discovery's session reached (its
    /// host, port, user and each jump host's), kept with the record, without reading the
    /// configuration again. `None` when no backend is built for the choice.
    pub fn discovery_target<T>(
        &self,
        selected: &SelectedBackend,
        target: &SshTarget,
        discovered: &Discovered<T>,
    ) -> Result<Option<EffectiveTarget>, SshBackendError> {
        #[cfg(feature = "system-ssh")]
        if let (Some(SshBackendKind::System), Some(program)) =
            (selected.backend, selected.program())
        {
            let launcher = self.launcher.with_program(program);
            let argv: Vec<&str> = discovered.remote_argv.iter().map(String::as_str).collect();
            let cmd = launcher.resolve_command(
                target,
                &crate::system::discovery::DISCOVERY_FORCED_OPTIONS,
                &[],
                &argv,
            );
            return crate::system::resolve::effective_target(&launcher, target, cmd, "discovery")
                .map(Some);
        }
        // The target the discovery's session reached, kept with its record: the
        // configuration is not read again, so a change after the discovery is not compared.
        #[cfg(feature = "embedded-ssh")]
        if selected.backend == Some(SshBackendKind::Embedded) {
            return discovered
                .target
                .clone()
                .map(Some)
                .ok_or(SshBackendError::TargetUnresolved {
                    purpose: "discovery",
                    detail: "the port discovery kept no target".into(),
                });
        }
        let _ = (selected, target, discovered);
        Ok(None)
    }

    /// Opens a tunnel from a local loopback port to `remote_port` over the chosen backend. With
    /// `expected` (the port discovery's effective target) the backend reads the tunnel's
    /// effective target first (the system backend's `ssh -G`, the embedded backend's
    /// resolution) and a different one ends with [`TunnelError::TargetMismatch`] before
    /// anything starts.
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
            #[cfg(feature = "embedded-ssh")]
            (Some(SshBackendKind::Embedded), _) => {
                let ctx = self.embedded_ctx();
                let set =
                    crate::embedded::resolve::load(ctx.settings).map_err(TunnelError::Backend)?;
                let chain = crate::embedded::resolve::resolve_chain(
                    &set,
                    ctx.settings,
                    target,
                    crate::embedded::resolve::Purpose::Tunnel,
                    ctx.verify,
                )
                .map_err(TunnelError::Backend)?;
                if let Some(expected) = expected {
                    let tunnel_target = chain.effective();
                    if tunnel_target != *expected {
                        return Err(TunnelError::TargetMismatch {
                            discovery: Box::new(expected.clone()),
                            tunnel: Box::new(tunnel_target),
                        });
                    }
                }
                let session =
                    crate::embedded::session::open(ctx, &chain).map_err(TunnelError::Backend)?;
                let tunnel =
                    crate::embedded::tunnel::Tunnel::open(session, remote_port, ctx.settings)
                        .map_err(TunnelError::LocalPort)?;
                Ok(SshTunnel::embedded(
                    tunnel,
                    selected.clone(),
                    chain.effective(),
                    chain.resolved(),
                ))
            }
            _ => {
                let _ = (target, remote_port, expected);
                Err(TunnelError::Backend(SshBackendError::NotFound {
                    searched: Vec::new(),
                }))
            }
        }
    }

    /// The settings query: every setting with its value and source, the backend choice for
    /// `target` (or for the launcher's program), and the resolver's diagnostics for `purpose`.
    /// It does not connect and runs no other program.
    pub fn inspect(&self, target: Option<&SshTarget>, purpose: SshPurpose) -> SshInspection {
        let settings = inspected_settings(&self.settings);
        let backend = self.select(&target.cloned().unwrap_or_default());
        #[cfg(feature = "embedded-ssh")]
        {
            self.inspect_embedded(settings, backend, target, &purpose)
        }
        #[cfg(not(feature = "embedded-ssh"))]
        {
            let _ = (target, purpose);
            empty_inspection(settings, backend)
        }
    }

    #[cfg(feature = "embedded-ssh")]
    fn inspect_embedded(
        &self,
        settings: Vec<InspectedSetting>,
        backend: Result<SelectedBackend, SshBackendError>,
        target: Option<&SshTarget>,
        purpose: &SshPurpose,
    ) -> SshInspection {
        use crate::embedded::resolve;
        use crate::inspect::{NoteClass, TargetInspection};
        let resolver = match &backend {
            Ok(SelectedBackend {
                backend: Some(SshBackendKind::Embedded),
                ..
            }) => ResolverState::Embedded,
            _ => ResolverState::EmbeddedPreview,
        };
        let set = match resolve::load(&self.settings) {
            Ok(set) => set,
            Err(e) => {
                let mut inspection = empty_inspection(settings, backend);
                inspection.resolver = resolver;
                if let SshBackendError::ConfigRefused { refusal, notes } = e {
                    inspection.target = target.map(|_| TargetInspection {
                        resolved: None,
                        notes,
                        refusal: Some(*refusal),
                    });
                }
                return inspection;
            }
        };
        let Some(target) = target else {
            let mut inspection = empty_inspection(settings, backend);
            inspection.resolver = resolver;
            inspection.files = resolve::file_reports(&set, None);
            return inspection;
        };
        let (dest, chain) = resolve::inspect(
            &set,
            &self.settings,
            target,
            embedded_purpose(purpose),
            self.verify(),
        );
        let (resolved, notes, refusal) = match chain {
            Ok(chain) => (Some(chain.resolved()), chain.notes, None),
            Err(SshBackendError::ConfigRefused { refusal, notes }) => {
                (Some(resolve::resolved_target(&dest)), notes, Some(*refusal))
            }
            Err(e) => (
                Some(resolve::resolved_target(&dest)),
                dest.notes.iter().map(resolve::config_note).collect(),
                Some(crate::inspect::RefusalNote {
                    kind: "undeterminable",
                    keyword: None,
                    origin: None,
                    message: e.to_string(),
                }),
            ),
        };
        let file_level = |n: &ConfigNote| {
            matches!(
                n.class,
                NoteClass::UnknownKeyword
                    | NoteClass::IgnoredUnknownKeyword
                    | NoteClass::DroppedByOpenSsh
            )
        };
        let (file_notes, notes): (Vec<ConfigNote>, Vec<ConfigNote>) =
            notes.into_iter().partition(file_level);
        SshInspection {
            settings,
            backend,
            resolver,
            files: resolve::file_reports(&set, dest.refusal.as_ref()),
            file_notes,
            target: Some(TargetInspection {
                resolved,
                notes,
                refusal,
            }),
        }
    }
}

/// The embedded resolution's purpose: exec with its command, SFTP as the `sftp` subsystem
/// (docs/design/ssh-backends.md "용도마다 해석하고, 같을 때만 세션을 나눈다").
#[cfg(feature = "embedded-ssh")]
fn embedded_purpose(purpose: &SshPurpose) -> crate::embedded::resolve::Purpose<'_> {
    use crate::embedded::resolve::Purpose;
    match purpose {
        SshPurpose::Tunnel => Purpose::Tunnel,
        SshPurpose::Discovery => Purpose::Discovery(""),
        SshPurpose::Exec { command } => Purpose::Exec(command),
        SshPurpose::Sftp => Purpose::Sftp,
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

#[cfg(all(test, feature = "embedded-ssh"))]
mod embedded_tests;

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
        // With the embedded backend built, `auto` falls back (embedded_tests).
        let backends: &[&str] = if cfg!(feature = "embedded-ssh") {
            &["system"]
        } else {
            &["auto", "system"]
        };
        for &backend in backends {
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
            // With the embedded backend built, its resolver previews the files
            // (embedded_tests).
            if cfg!(not(feature = "embedded-ssh")) {
                assert_eq!(inspection.resolver, ResolverState::NotBuilt);
                assert!(inspection.files.is_empty());
            }
            assert!(inspection.target.is_none());
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
            hops: Vec::new(),
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
