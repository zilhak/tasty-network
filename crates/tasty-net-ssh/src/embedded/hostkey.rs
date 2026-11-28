//! The host key check of the embedded backend (docs/design/ssh-backends.md "호스트 키 판정").
//!
//! The verdict is the OpenSSH port's `KnownHosts::check` (`check_host_key` order: a changed
//! key and `@revoked` refuse, `@cert-authority` lines are not trusted). This module adds what
//! `ssh` does around it: `RequiredRSASize`, `RevokedHostKeys` files, `CheckHostIP`, the
//! `StrictHostKeyChecking` decision, and the `accept-new` record (`add_host_to_hostfile`,
//! with `HashKnownHosts`).

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use russh::keys::{HashAlg, PublicKey, PublicKeyOrCertificate};
use tasty_net_ssh_config::options::{Flag, StrictHostKey};
use tasty_net_ssh_config::{HostKeyQuery, HostKeyVerdict, KnownHosts, Resolution};

use crate::backend::{HostKeyReason, HostKeyRefusal};
use crate::cancel::recover_mutex;
use crate::inspect::{ConfigNote, NoteClass, RefusalNote};
use crate::prompt::{HostKeyQuestion, SshPrompt};

use super::resolve::{RSA_MINIMUM_BITS, SSH_PORT, backend_note};
use super::{LOG_TARGET, Paused, ask};

/// The start of a binary key revocation list (`KRL_MAGIC`, `krl.h`).
const KRL_MAGIC: &[u8] = b"SSHKRL\n\0";
/// The salt length of a hashed known_hosts name (`SHA_DIGEST_LENGTH`, `hostfile.c:116`).
const HASH_SALT_LEN: usize = 20;

/// A `RevokedHostKeys` file, read before the connection.
enum RevokedList {
    Keys { file: PathBuf, blobs: Vec<Vec<u8>> },
    Unreadable(PathBuf),
}

/// Everything the check of one host needs, taken from its resolution before connecting.
pub(crate) struct HostCheck {
    query: HostKeyQuery,
    known: KnownHosts,
    mode: StrictHostKey,
    check_ip: bool,
    port: u16,
    revoked: Vec<RevokedList>,
    required_rsa: u32,
    hash: bool,
    record_file: Option<PathBuf>,
    /// The known_hosts files, read again under the record lock before a new key is written.
    known_files: Vec<PathBuf>,
    prompt: Option<Arc<dyn SshPrompt>>,
    prompt_timeout: Duration,
    paused: Arc<Paused>,
    notes: Arc<Mutex<Vec<ConfigNote>>>,
    notes_poison_reported: AtomicBool,
}

impl HostCheck {
    /// Reads what the check of `res` needs. A `RevokedHostKeys` file in the binary KRL format
    /// refuses the connection before it starts ("보안 관련 미구현").
    pub(crate) fn prepare(
        res: &Resolution,
        prompt: Option<Arc<dyn SshPrompt>>,
        prompt_timeout: Duration,
        paused: Arc<Paused>,
        notes: Arc<Mutex<Vec<ConfigNote>>>,
    ) -> Result<Self, RefusalNote> {
        let o = &res.options;
        let mut revoked = Vec::new();
        for file in o
            .revoked_host_keys
            .iter()
            .filter(|f| !f.eq_ignore_ascii_case("none"))
        {
            let file = PathBuf::from(file);
            match std::fs::read(&file) {
                Ok(bytes) if bytes.starts_with(KRL_MAGIC) => {
                    return Err(RefusalNote {
                        kind: "security_not_implemented",
                        keyword: Some("revokedhostkeys".into()),
                        origin: Some(res.source_of("revokedhostkeys").to_string()),
                        message: format!(
                            "{} is a binary key revocation list, which the embedded backend does not read",
                            file.display()
                        ),
                    });
                }
                Ok(bytes) => revoked.push(RevokedList::Keys {
                    blobs: key_list(&String::from_utf8_lossy(&bytes)),
                    file,
                }),
                Err(_) => revoked.push(RevokedList::Unreadable(file)),
            }
        }
        let system: Vec<PathBuf> = o.system_hostfiles.iter().map(PathBuf::from).collect();
        let record_file = (!o.user_hostfiles.is_empty())
            .then(|| {
                res.known_hosts_files
                    .iter()
                    .find(|f| !system.contains(f))
                    .cloned()
            })
            .flatten();
        Ok(Self {
            query: res.host_key_query(),
            known: KnownHosts::load(&res.known_hosts_files),
            mode: res.host_key_checking,
            check_ip: o.check_host_ip.is_some_and(Flag::is_yes),
            port: o.port.unwrap_or(SSH_PORT),
            revoked,
            required_rsa: u32::try_from(o.required_rsa_size.unwrap_or(RSA_MINIMUM_BITS))
                .unwrap_or_default(),
            hash: o.hash_known_hosts.is_some_and(Flag::is_yes),
            record_file,
            known_files: res.known_hosts_files.clone(),
            prompt: if o.batch_mode.is_some_and(Flag::is_yes) {
                None
            } else {
                prompt
            },
            prompt_timeout,
            paused,
            notes,
            notes_poison_reported: AtomicBool::new(false),
        })
    }

    /// The key types the known_hosts files hold for this host (for the algorithm order).
    pub(crate) fn known_types(&self) -> Vec<String> {
        self.known
            .entries(&self.query)
            .into_iter()
            .filter_map(|e| e.key_type)
            .collect()
    }

    fn note(&self, note: ConfigNote) {
        let mut notes = recover_mutex(
            self.notes.lock(),
            "embedded notes",
            &self.notes_poison_reported,
        );
        if !notes.contains(&note) {
            notes.push(note);
        }
    }

    /// Judges the server's key. `address` is the server's IP address when the connection is a
    /// direct TCP connection (`CheckHostIP` needs it; through a jump host there is none).
    pub(crate) async fn judge(
        &self,
        offered: &PublicKeyOrCertificate,
        address: Option<&str>,
    ) -> Result<(), HostKeyRefusal> {
        let (blob, plain, keys_of_record): (Vec<u8>, PublicKey, Vec<Vec<u8>>) = match offered {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                let blob = key.to_bytes().unwrap_or_default();
                (blob.clone(), key.clone(), vec![blob])
            }
            PublicKeyOrCertificate::Certificate(cert) => {
                let plain = PublicKey::new(cert.public_key().clone(), "");
                let ca = PublicKey::new(cert.signature_key().clone(), "");
                (
                    cert.to_bytes().unwrap_or_default(),
                    plain.clone(),
                    vec![
                        plain.to_bytes().unwrap_or_default(),
                        ca.to_bytes().unwrap_or_default(),
                    ],
                )
            }
        };
        let key_type = plain.key_data().algorithm().as_str().to_string();
        let refuse = |reason| HostKeyRefusal {
            host: self.query.name.clone(),
            key_type: key_type.clone(),
            reason,
        };
        if let Some(rsa) = plain.key_data().rsa()
            && rsa.key_size() < self.required_rsa
        {
            return Err(refuse(HostKeyReason::RsaTooSmall {
                bits: rsa.key_size(),
                required: self.required_rsa,
            }));
        }
        for list in &self.revoked {
            match list {
                RevokedList::Unreadable(file) => {
                    return Err(refuse(HostKeyReason::RevokedListUnreadable {
                        file: file.clone(),
                    }));
                }
                RevokedList::Keys { file, blobs } => {
                    if keys_of_record.iter().any(|k| blobs.contains(k)) {
                        return Err(refuse(HostKeyReason::Revoked {
                            file: file.clone(),
                            line: None,
                        }));
                    }
                }
            }
        }
        let names = Names {
            query: self.query.clone(),
            port: self.port,
            address: address.filter(|_| self.check_ip).map(str::to_string),
            plain_blob: plain.to_bytes().unwrap_or_default(),
            blob,
        };
        // The first judgement, from the files read before connecting. A judgement that writes
        // is made again by the same function under the record lock on the files as they are
        // then, and that judgement is followed: a refusal refuses, a new host goes to the
        // checking mode (once), and what is written is what it chose.
        let mut decision = names.decide(&self.known);
        let mut accepted = false;
        let stands = loop {
            match &decision {
                Decision::Refuse(_) | Decision::Pass => break decision,
                Decision::NewHost { .. } if !accepted => {
                    if !self.accept_unknown(&plain).await {
                        break decision;
                    }
                    accepted = true;
                }
                Decision::NewHost { .. } | Decision::Record(_) => {}
            }
            match self.record(&names, accepted, &plain, decision).await {
                Recorded::Done(latest) => break latest,
                Recorded::Again(latest) => decision = latest,
            }
        };
        // The notes and the result are those of the judgement that stands (the one under the
        // lock when there was one), as a new connection to those files gives them.
        if let Decision::NewHost { verdict, .. } = &stands {
            self.note_distrusted(verdict);
        }
        match stands {
            Decision::Refuse(reason) => Err(refuse(reason)),
            Decision::NewHost { .. } if !accepted => Err(refuse(HostKeyReason::Unknown)),
            Decision::Pass | Decision::Record(_) | Decision::NewHost { .. } => Ok(()),
        }
    }

    /// Notes a `@cert-authority` line that would have vouched for the host, which is not
    /// trusted in this stage.
    fn note_distrusted(&self, verdict: &HostKeyVerdict) {
        if let HostKeyVerdict::DistrustedCaLine { file, line } = verdict {
            self.note(backend_note(
                "@cert-authority",
                &tasty_net_ssh_config::Origin::File {
                    path: file.clone(),
                    line: *line,
                },
                NoteClass::CertAuthorityDistrusted,
            ));
        }
    }

    /// Whether an unknown host key `plain` is accepted by the `StrictHostKeyChecking` mode:
    /// `accept-new` accepts it, `ask` asks the prompt (no prompt or no answer refuses), `yes`
    /// refuses.
    async fn accept_unknown(&self, plain: &PublicKey) -> bool {
        match self.mode {
            StrictHostKey::New => true,
            StrictHostKey::Ask => match &self.prompt {
                Some(prompt) => {
                    let question = HostKeyQuestion {
                        host: self.query.name.clone(),
                        key_type: plain.key_data().algorithm().as_str().to_string(),
                        fingerprint: plain.fingerprint(HashAlg::Sha256).to_string(),
                    };
                    ask(
                        Arc::clone(prompt),
                        self.prompt_timeout,
                        &self.paused,
                        move |p| p.confirm_host_key(&question),
                    )
                    .await
                    .unwrap_or(false)
                }
                None => false,
            },
            // `Off` is never the checking mode of a resolution; refuse like `yes`.
            StrictHostKey::Yes | StrictHostKey::Off => false,
        }
    }

    /// Judges `names` again under the record lock with [`Names::decide`] on the known_hosts
    /// files read again, and writes `key` for the names that judgement chose to the first
    /// `UserKnownHostsFile` (`add_host_to_hostfile`): the new `CheckHostIP` address of a
    /// known host, or with `accepted` (the checking mode took the key as a new host) the host
    /// and its new address. A key another connection recorded meanwhile is the name's key now,
    /// so the same key is not written twice; another judgement (a refusal, or a new host the
    /// mode has not decided) is returned to [`Self::judge`] to follow. As in `ssh`, a failure
    /// to write is logged and the connection goes on. With no file to write to, nothing is
    /// written and `decision` (the judgement before it) stands. [`Recorded::Done`] carries the
    /// judgement that stands, whose notes [`Self::judge`] keeps.
    async fn record(
        &self,
        names: &Names,
        accepted: bool,
        key: &PublicKey,
        decision: Decision,
    ) -> Recorded {
        let Some(file) = self.record_file.clone() else {
            return Recorded::Done(decision);
        };
        let files = self.known_files.clone();
        let (names, key, hash) = (names.clone(), key.clone(), self.hash);
        // What stands when the record task itself fails: the judgement before it.
        let stand_in = decision;
        let written = tokio::task::spawn_blocking(move || {
            let _guard = RECORD_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let latest = names.decide(&KnownHosts::load(&files));
            let to_write = match &latest {
                Decision::Pass => return (Recorded::Done(latest), None),
                Decision::Record(to_write) => to_write,
                Decision::NewHost {
                    names: to_write, ..
                } if accepted => to_write,
                Decision::NewHost { .. } | Decision::Refuse(_) => {
                    return (Recorded::Again(latest), None);
                }
            };
            let failed = match host_lines(to_write, &key, hash) {
                Ok(lines) => append_locked(&file, &lines).err(),
                Err(e) => Some(std::io::Error::other(e)),
            };
            (Recorded::Done(latest), failed)
        })
        .await;
        match written {
            Ok((recorded, failed)) => {
                if let Some(e) = failed {
                    tracing::warn!(target: LOG_TARGET, "could not record the host key: {e}");
                }
                recorded
            }
            Err(e) => {
                tracing::warn!(target: LOG_TARGET, "could not record the host key: {e}");
                Recorded::Done(stand_in)
            }
        }
    }
}

/// The names one connection's host key is judged under, and the key: the host's query (with
/// the `CheckHostIP` address for the re-lookup of a name with a port) with the offered blob,
/// and the address's own query (with the port) with the plain key.
#[derive(Clone)]
struct Names {
    query: HostKeyQuery,
    port: u16,
    address: Option<String>,
    blob: Vec<u8>,
    plain_blob: Vec<u8>,
}

impl Names {
    /// The judgement of these names in `known` (`check_host_key`), the same before connecting
    /// and under the record lock. The address is judged first: its changed or revoked key
    /// refuses before anything is asked. Then the host: a match passes and writes only a new
    /// address (`ssh` adds it without asking, `sshconnect.c:1153`–`:1167`), a changed or
    /// revoked key refuses, and anything else is a new host for the checking mode, with the
    /// host and its new address to write.
    fn decide(&self, known: &KnownHosts) -> Decision {
        let mut new_address = None;
        if let Some(address) = &self.address {
            let query = HostKeyQuery::for_address(address, self.port);
            match known.check(&query, &self.plain_blob) {
                HostKeyVerdict::Mismatch { file, line } => {
                    return Decision::Refuse(HostKeyReason::Changed { file, line });
                }
                HostKeyVerdict::Revoked { file, line } => {
                    return Decision::Refuse(HostKeyReason::Revoked {
                        file,
                        line: Some(line),
                    });
                }
                HostKeyVerdict::Match { .. } => {}
                _ => new_address = Some(query.name),
            }
        }
        match known.check_with_address(&self.query, self.address.as_deref(), &self.blob) {
            HostKeyVerdict::Match { .. } => match new_address {
                Some(address) => Decision::Record(vec![address]),
                None => Decision::Pass,
            },
            HostKeyVerdict::Mismatch { file, line } => {
                Decision::Refuse(HostKeyReason::Changed { file, line })
            }
            HostKeyVerdict::Revoked { file, line } => Decision::Refuse(HostKeyReason::Revoked {
                file,
                line: Some(line),
            }),
            verdict => Decision::NewHost {
                names: std::iter::once(self.query.name.clone())
                    .chain(new_address)
                    .collect(),
                verdict,
            },
        }
    }
}

/// What [`Names::decide`] judged.
enum Decision {
    /// Refused as changed or revoked.
    Refuse(HostKeyReason),
    /// Known, nothing to write.
    Pass,
    /// The host is known and these names (its new address) are written without asking.
    Record(Vec<String>),
    /// The host is new (`NotFound` or `DistrustedCaLine`): the checking mode decides, and
    /// `names` are written when it accepts.
    NewHost {
        names: Vec<String>,
        verdict: HostKeyVerdict,
    },
}

/// What the record under its lock did.
enum Recorded {
    /// Written as judged, or nothing to write: the judgement that stands.
    Done(Decision),
    /// The judgement under the lock is another one, to follow.
    Again(Decision),
}

/// The key blobs of a `RevokedHostKeys` file of public keys (`type base64 [comment]` lines).
fn key_list(text: &str) -> Vec<Vec<u8>> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            let _key_type = words.next()?;
            STANDARD.decode(words.next()?).ok()
        })
        .collect()
}

/// The known_hosts lines of `key` for `names` (`write_host_entry`): one line of the names
/// joined by `,`, or with `hash` one line per name with its `|1|salt|hash` form.
fn host_lines(names: &[String], key: &PublicKey, hash: bool) -> Result<Vec<String>, String> {
    let key_type = key.key_data().algorithm().as_str().to_string();
    let blob = STANDARD.encode(key.to_bytes().map_err(|e| e.to_string())?);
    if !hash {
        return Ok(vec![format!("{} {key_type} {blob}", names.join(","))]);
    }
    let rng = ring::rand::SystemRandom::new();
    names
        .iter()
        .map(|name| {
            let mut salt = [0u8; HASH_SALT_LEN];
            ring::rand::SecureRandom::fill(&rng, &mut salt)
                .map_err(|_| "no random salt".to_string())?;
            let tag = ring::hmac::sign(
                &ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt),
                name.as_bytes(),
            );
            Ok(format!(
                "|1|{}|{} {key_type} {blob}",
                STANDARD.encode(salt),
                STANDARD.encode(tag.as_ref())
            ))
        })
        .collect()
}

/// Serializes the known_hosts records of this process: each one is written as a whole while
/// no other record of the process writes.
static RECORD_LOCK: Mutex<()> = Mutex::new(());

/// Appends `lines` to `file` under the record lock ([`append_locked`]).
#[cfg(test)]
fn append(file: &std::path::Path, lines: &[String]) -> std::io::Result<()> {
    let _guard = RECORD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    append_locked(file, lines)
}

/// Appends `lines` to `file`, making its folder when it is missing (`ssh` makes `~/.ssh`).
/// The lines go out in one buffer and one `write_all`, so two records of this process (and a
/// record of another program that writes whole lines) do not mix inside a line. The caller
/// holds [`RECORD_LOCK`].
fn append_locked(file: &std::path::Path, lines: &[String]) -> std::io::Result<()> {
    let mut buffer = String::new();
    for line in lines {
        buffer.push_str(line);
        buffer.push('\n');
    }
    if let Some(dir) = file.parent()
        && !dir.as_os_str().is_empty()
        && !dir.exists()
    {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    f.write_all(buffer.as_bytes())?;
    f.flush()
}

/// The russh handler of one host: the host key check, and the refusal when it said no.
pub(crate) struct Client {
    pub(crate) check: Arc<HostCheck>,
    pub(crate) address: Option<String>,
    pub(crate) refusal: Arc<Mutex<Option<HostKeyRefusal>>>,
}

impl russh::client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        match self
            .check
            .judge(server_public_key, self.address.as_deref())
            .await
        {
            Ok(()) => Ok(true),
            Err(refusal) => {
                let mut slot = self
                    .refusal
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *slot = Some(refusal);
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests;
