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
        let address = address.filter(|_| self.check_ip);
        let verdict = self.known.check_with_address(&self.query, address, &blob);
        let plain_blob = plain.to_bytes().unwrap_or_default();
        let address_unknown = match address {
            Some(a) => {
                let query = HostKeyQuery::for_address(a, self.port);
                match self.known.check(&query, &plain_blob) {
                    HostKeyVerdict::Mismatch { file, line } => {
                        return Err(refuse(HostKeyReason::Changed { file, line }));
                    }
                    HostKeyVerdict::Revoked { file, line } => {
                        return Err(refuse(HostKeyReason::Revoked {
                            file,
                            line: Some(line),
                        }));
                    }
                    HostKeyVerdict::Match { .. } => None,
                    _ => Some(query.name),
                }
            }
            None => None,
        };
        match verdict {
            HostKeyVerdict::Match { .. } => {
                if let Some(ip) = address_unknown {
                    self.record(&[ip], &plain).await;
                }
                Ok(())
            }
            HostKeyVerdict::Mismatch { file, line } => {
                Err(refuse(HostKeyReason::Changed { file, line }))
            }
            HostKeyVerdict::Revoked { file, line } => Err(refuse(HostKeyReason::Revoked {
                file,
                line: Some(line),
            })),
            other => {
                if let HostKeyVerdict::DistrustedCaLine { file, line } = &other {
                    self.note(backend_note(
                        "@cert-authority",
                        &tasty_net_ssh_config::Origin::File {
                            path: file.clone(),
                            line: *line,
                        },
                        NoteClass::CertAuthorityDistrusted,
                    ));
                }
                let accept = match self.mode {
                    StrictHostKey::New => true,
                    StrictHostKey::Ask => match &self.prompt {
                        Some(prompt) => {
                            let question = HostKeyQuestion {
                                host: self.query.name.clone(),
                                key_type: key_type.clone(),
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
                };
                if !accept {
                    return Err(refuse(HostKeyReason::Unknown));
                }
                let mut names = vec![self.query.name.clone()];
                names.extend(address_unknown);
                self.record(&names, &plain).await;
                Ok(())
            }
        }
    }

    /// Adds `key` for `names` to the first `UserKnownHostsFile` (`add_host_to_hostfile`). As
    /// in `ssh`, a failure is logged and the connection goes on.
    async fn record(&self, names: &[String], key: &PublicKey) {
        let Some(file) = self.record_file.clone() else {
            return;
        };
        let lines = match host_lines(names, key, self.hash) {
            Ok(lines) => lines,
            Err(e) => {
                tracing::warn!(target: LOG_TARGET, "could not record the host key: {e}");
                return;
            }
        };
        let written = tokio::task::spawn_blocking(move || append(&file, &lines)).await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(target: LOG_TARGET, "could not record the host key: {e}");
            }
            Err(e) => {
                tracing::warn!(target: LOG_TARGET, "could not record the host key: {e}");
            }
        }
    }
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

/// Appends `lines` to `file`, making its folder when it is missing (`ssh` makes `~/.ssh`).
/// The lines go out in one buffer and one `write_all`, so two records of this process (and a
/// record of another program that writes whole lines) do not mix inside a line.
fn append(file: &std::path::Path, lines: &[String]) -> std::io::Result<()> {
    let mut buffer = String::new();
    for line in lines {
        buffer.push_str(line);
        buffer.push('\n');
    }
    let _guard = RECORD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
