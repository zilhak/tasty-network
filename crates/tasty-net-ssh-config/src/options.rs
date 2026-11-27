// This file is derived from OpenSSH, modified: a Rust port of parts of readconf.c, readconf.h, pathnames.h, log.c, digest-openssl.c
// of OpenSSH portable 10.6p2 (commit d449e662b6b8b8cb20e852d8084c269af1c5e8f1). The
// notices of those files follow as written there; LICENSE-OpenSSH has the OpenSSH licence.
//
// readconf.c:
// /*
//  * Author: Tatu Ylonen <ylo@cs.hut.fi>
//  * Copyright (c) 1995 Tatu Ylonen <ylo@cs.hut.fi>, Espoo, Finland
//  *                    All rights reserved
//  * Functions for reading the configuration files.
//  *
//  * As far as I am concerned, the code I have written for this software
//  * can be used freely for any purpose.  Any derived versions of this
//  * software must be clearly marked as such, and if the derived work is
//  * incompatible with the protocol description in the RFC file, it must be
//  * called by a name other than "ssh" or "Secure Shell".
//  */
//
// readconf.h:
// /*
//  * Author: Tatu Ylonen <ylo@cs.hut.fi>
//  * Copyright (c) 1995 Tatu Ylonen <ylo@cs.hut.fi>, Espoo, Finland
//  *                    All rights reserved
//  * Functions for reading the configuration file.
//  *
//  * As far as I am concerned, the code I have written for this software
//  * can be used freely for any purpose.  Any derived versions of this
//  * software must be clearly marked as such, and if the derived work is
//  * incompatible with the protocol description in the RFC file, it must be
//  * called by a name other than "ssh" or "Secure Shell".
//  */
//
// pathnames.h:
// /*
//  * Author: Tatu Ylonen <ylo@cs.hut.fi>
//  * Copyright (c) 1995 Tatu Ylonen <ylo@cs.hut.fi>, Espoo, Finland
//  *                    All rights reserved
//  *
//  * As far as I am concerned, the code I have written for this software
//  * can be used freely for any purpose.  Any derived versions of this
//  * software must be clearly marked as such, and if the derived work is
//  * incompatible with the protocol description in the RFC file, it must be
//  * called by a name other than "ssh" or "Secure Shell".
//  */
//
// log.c:
// /*
//  * Author: Tatu Ylonen <ylo@cs.hut.fi>
//  * Copyright (c) 1995 Tatu Ylonen <ylo@cs.hut.fi>, Espoo, Finland
//  *                    All rights reserved
//  *
//  * As far as I am concerned, the code I have written for this software
//  * can be used freely for any purpose.  Any derived versions of this
//  * software must be clearly marked as such, and if the derived work is
//  * incompatible with the protocol description in the RFC file, it must be
//  * called by a name other than "ssh" or "Secure Shell".
//  */
// /*
//  * Copyright (c) 2000 Markus Friedl.  All rights reserved.
//  *
//  * Redistribution and use in source and binary forms, with or without
//  * modification, are permitted provided that the following conditions
//  * are met:
//  * 1. Redistributions of source code must retain the above copyright
//  *    notice, this list of conditions and the following disclaimer.
//  * 2. Redistributions in binary form must reproduce the above copyright
//  *    notice, this list of conditions and the following disclaimer in the
//  *    documentation and/or other materials provided with the distribution.
//  *
//  * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
//  * IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//  * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
//  * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT,
//  * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
//  * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//  * DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//  * THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//  * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
//  * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//  */
//
// digest-openssl.c:
// /*
//  * Copyright (c) 2013 Damien Miller <djm@mindrot.org>
//  *
//  * Permission to use, copy, modify, and distribute this software for any
//  * purpose with or without fee is hereby granted, provided that the above
//  * copyright notice and this permission notice appear in all copies.
//  *
//  * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//  * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//  * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//  * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//  * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//  * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//  * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//  */
//! The client options of OpenSSH `readconf.h` (`Options`), the multistate value tables of
//! `readconf.c`, `fill_default_options` (`:2860`) and `dump_client_config` (`:3739`, the
//! `ssh -G` output).
//!
//! A C field that starts as `-1` or `NULL` and is set by the first value is an `Option` here.

use crate::algorithms::{AlgList, kex_assemble_names};
use crate::keywords::{Op, keyword_name};
use crate::misc::{IPTOS_DSCP_CS0, IPTOS_DSCP_EF, TUNID_ANY, iptos2str};

/// A value of a multistate keyword: the table is `multistate_*` of `readconf.c`. Parsing is
/// case-insensitive and the printed name is the first name of the value.
pub trait Multistate: Copy + PartialEq + 'static {
    /// The names and values in the order of the original table.
    const TABLE: &'static [(&'static str, Self)];

    /// `parse_multistate_value`.
    fn parse(arg: &str) -> Option<Self> {
        Self::TABLE
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(arg))
            .map(|(_, v)| *v)
    }

    /// `fmt_multistate_int`.
    fn name(self) -> &'static str {
        Self::TABLE
            .iter()
            .find(|(_, v)| *v == self)
            .map(|(k, _)| *k)
            .unwrap_or("UNKNOWN")
    }
}

macro_rules! multistate {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $var:ident),* $(,)? } = [$(($k:literal, $v:ident)),* $(,)?]) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name { $($(#[$vm])* $var),* }
        impl Multistate for $name {
            const TABLE: &'static [(&'static str, Self)] = &[$(($k, $name::$v)),*];
        }
    };
}

multistate!(
    /// `multistate_flag`.
    Flag { No, Yes } = [("true", Yes), ("false", No), ("yes", Yes), ("no", No)]
);
multistate!(
    /// `multistate_yesnoask` (`VerifyHostKeyDNS`, `UpdateHostKeys`).
    YesNoAsk { No, Yes, Ask } =
        [("true", Yes), ("false", No), ("yes", Yes), ("no", No), ("ask", Ask)]
);
multistate!(
    /// `multistate_strict_hostkey` (`SSH_STRICT_HOSTKEY_*`).
    StrictHostKey { Off, New, Yes, Ask } = [
        ("true", Yes), ("false", Off), ("yes", Yes), ("no", Off), ("ask", Ask), ("off", Off),
        ("accept-new", New),
    ]
);
multistate!(
    /// `multistate_yesnoaskconfirm` (`AddKeysToAgent`).
    AddKeys { No, Yes, Ask, Confirm } = [
        ("true", Yes), ("false", No), ("yes", Yes), ("no", No), ("ask", Ask),
        ("confirm", Confirm),
    ]
);
multistate!(
    /// `multistate_addressfamily`.
    AddressFamily { Inet, Inet6, Any } = [("inet", Inet), ("inet6", Inet6), ("any", Any)]
);
multistate!(
    /// `multistate_controlmaster` (`SSHCTL_MASTER_*`).
    ControlMaster { No, Yes, Ask, Auto, AutoAsk } = [
        ("true", Yes), ("yes", Yes), ("false", No), ("no", No), ("auto", Auto), ("ask", Ask),
        ("autoask", AutoAsk),
    ]
);
multistate!(
    /// `multistate_tunnel` (`SSH_TUNMODE_*`; `yes` is point-to-point).
    Tunnel { No, PointToPoint, Ethernet } = [
        ("ethernet", Ethernet), ("point-to-point", PointToPoint), ("true", PointToPoint),
        ("yes", PointToPoint), ("false", No), ("no", No),
    ]
);
multistate!(
    /// `multistate_requesttty` (`REQUEST_TTY_*`).
    RequestTty { Auto, No, Yes, Force } = [
        ("true", Yes), ("yes", Yes), ("false", No), ("no", No), ("force", Force),
        ("auto", Auto),
    ]
);
multistate!(
    /// `multistate_sessiontype` (`SESSION_TYPE_*`).
    SessionType { None, Subsystem, Default } =
        [("none", None), ("subsystem", Subsystem), ("default", Default)]
);
multistate!(
    /// `multistate_canonicalizehostname` (`SSH_CANONICALISE_*`).
    Canonicalize { No, Yes, Always } =
        [("true", Yes), ("false", No), ("yes", Yes), ("no", No), ("always", Always)]
);
multistate!(
    /// `multistate_pubkey_auth` (`SSH_PUBKEY_AUTH_*`).
    PubkeyAuth { No, Unbound, HostBound, All } = [
        ("true", All), ("false", No), ("yes", All), ("no", No), ("unbound", Unbound),
        ("host-bound", HostBound),
    ]
);
multistate!(
    /// `multistate_compression` of a build with zlib.
    Compression { No, Delayed } = [("yes", Delayed), ("no", No)]
);
multistate!(
    /// `multistate_keepalives` (`SSH_KEEPALIVES_*`).
    KeepAlives { Off, Transport, All } = [
        ("true", Transport), ("false", Off), ("yes", Transport), ("no", Off),
        ("transport", Transport), ("all", All),
    ]
);
multistate!(
    /// `multistate_warnweakcrypto` (`no-pq-kex` is `no`).
    WarnWeakCrypto { No, Yes } =
        [("true", Yes), ("false", No), ("yes", Yes), ("no", No), ("no-pq-kex", No)]
);
multistate!(
    /// `log_levels[]` of `log.c` (`DEBUG` is `DEBUG1`). The order is the C order.
    LogLevel { Quiet, Fatal, Error, Info, Verbose, Debug1, Debug2, Debug3 } = [
        ("QUIET", Quiet), ("FATAL", Fatal), ("ERROR", Error), ("INFO", Info),
        ("VERBOSE", Verbose), ("DEBUG", Debug1), ("DEBUG1", Debug1), ("DEBUG2", Debug2),
        ("DEBUG3", Debug3),
    ]
);
multistate!(
    /// `log_facilities[]` of `log.c`.
    LogFacility {
        Daemon, User, Auth, AuthPriv, Local0, Local1, Local2, Local3, Local4, Local5, Local6,
        Local7
    } = [
        ("DAEMON", Daemon), ("USER", User), ("AUTH", Auth), ("AUTHPRIV", AuthPriv),
        ("LOCAL0", Local0), ("LOCAL1", Local1), ("LOCAL2", Local2), ("LOCAL3", Local3),
        ("LOCAL4", Local4), ("LOCAL5", Local5), ("LOCAL6", Local6), ("LOCAL7", Local7),
    ]
);
multistate!(
    /// `digests[]` of `digest-openssl.c` (`FingerprintHash`).
    Digest { Md5, Sha1, Sha256, Sha384, Sha512 } = [
        ("MD5", Md5), ("SHA1", Sha1), ("SHA256", Sha256), ("SHA384", Sha384),
        ("SHA512", Sha512),
    ]
);

impl Flag {
    /// The flag as a `bool`.
    pub fn is_yes(self) -> bool {
        self == Flag::Yes
    }
}

/// `EscapeChar`: `none` or one byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeChar {
    /// `SSH_ESCAPECHAR_NONE`.
    None,
    /// The byte.
    Byte(u8),
}

/// One side of a forwarding (`struct Forward`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardEnd {
    /// A Unix socket path (`PORT_STREAMLOCAL`).
    Path(String),
    /// A host and port. The port is `a2port`'s value (`-1` invalid, `0` allowed for remote
    /// listens).
    Port {
        /// The host, if given.
        host: Option<String>,
        /// The port.
        port: i32,
    },
}

/// A `LocalForward`, `RemoteForward` or `DynamicForward` (`struct Forward`). A dynamic
/// forward connects to the host `socks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    /// The listening side.
    pub listen: ForwardEnd,
    /// The connecting side.
    pub connect: ForwardEnd,
}

/// `ProxyJump` (`jump_user`, `jump_host`, `jump_port`, `jump_extra`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Jump {
    /// `ProxyJump none`.
    None,
    /// The first hop and the rest of the list.
    Hosts {
        /// The user of the first hop.
        user: Option<String>,
        /// The host of the first hop.
        host: String,
        /// The port of the first hop.
        port: Option<u16>,
        /// The hops before the first hop in the written list (`jump_extra`).
        extra: Option<String>,
    },
}

/// The client options after resolution (`Options` of `readconf.h`). The values are those
/// OpenSSH would use, before this crate's treatment (see [`crate::Resolution`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Options {
    pub host_arg: String,
    pub forward_agent: Option<Flag>,
    pub forward_agent_sock_path: Option<String>,
    pub forward_x11: Option<Flag>,
    pub forward_x11_trusted: Option<Flag>,
    pub forward_x11_timeout: Option<i32>,
    pub exit_on_forward_failure: Option<Flag>,
    pub xauth_location: Option<String>,
    pub gateway_ports: Option<Flag>,
    pub streamlocal_bind_mask: Option<u32>,
    pub streamlocal_bind_unlink: Option<Flag>,
    pub pubkey_authentication: Option<PubkeyAuth>,
    pub hostbased_authentication: Option<Flag>,
    pub gss_authentication: Option<Flag>,
    pub gss_deleg_creds: Option<Flag>,
    pub password_authentication: Option<Flag>,
    pub kbd_interactive_authentication: Option<Flag>,
    pub kbd_interactive_devices: Option<String>,
    pub batch_mode: Option<Flag>,
    pub check_host_ip: Option<Flag>,
    pub strict_host_key_checking: Option<StrictHostKey>,
    pub compression: Option<Compression>,
    pub tcp_keep_alive: Option<KeepAlives>,
    pub ip_qos: Option<(i32, i32)>,
    pub escape_char: Option<EscapeChar>,
    pub port: Option<u16>,
    pub address_family: Option<AddressFamily>,
    pub connection_attempts: Option<i32>,
    pub connection_timeout: Option<i32>,
    pub number_of_password_prompts: Option<i32>,
    pub ciphers: Option<String>,
    pub macs: Option<String>,
    pub hostkeyalgorithms: Option<String>,
    pub kex_algorithms: Option<String>,
    pub ca_sign_algorithms: Option<String>,
    pub hostname: Option<String>,
    pub host_key_alias: Option<String>,
    pub proxy_command: Option<String>,
    pub user: Option<String>,
    pub system_hostfiles: Vec<String>,
    pub user_hostfiles: Vec<String>,
    pub preferred_authentications: Option<String>,
    pub bind_address: Option<String>,
    pub bind_interface: Option<String>,
    pub pkcs11_provider: Option<String>,
    pub sk_provider: Option<String>,
    pub verify_host_key_dns: Option<YesNoAsk>,
    /// Paths and who gave them (`identity_file_userprovided`).
    pub identity_files: Vec<(String, ProvidedBy)>,
    /// Paths and who gave them (`certificate_file_userprovided`).
    pub certificate_files: Vec<(String, ProvidedBy)>,
    /// The mode and the lifespan in seconds (`0` unlimited).
    pub add_keys_to_agent: Option<(AddKeys, i32)>,
    pub identity_agent: Option<String>,
    pub local_forwards: Vec<Forward>,
    pub remote_forwards: Vec<Forward>,
    pub clear_forwardings: Option<Flag>,
    pub permitted_remote_opens: Vec<String>,
    pub enable_ssh_keysign: Option<Flag>,
    pub rekey_limit: Option<i64>,
    pub rekey_interval: Option<i32>,
    pub no_host_authentication_for_localhost: Option<Flag>,
    pub identities_only: Option<Flag>,
    pub server_alive_interval: Option<i32>,
    pub server_alive_count_max: Option<i32>,
    pub send_env: Vec<String>,
    pub setenv: Vec<String>,
    pub control_path: Option<String>,
    pub control_master: Option<ControlMaster>,
    /// Whether it persists and the timeout in seconds (`0` without one).
    pub control_persist: Option<(bool, i32)>,
    pub hash_known_hosts: Option<Flag>,
    pub tun_open: Option<Tunnel>,
    pub tun_device: Option<(i32, i32)>,
    pub local_command: Option<String>,
    pub permit_local_command: Option<Flag>,
    pub remote_command: Option<String>,
    pub visual_host_key: Option<Flag>,
    pub request_tty: Option<RequestTty>,
    pub session_type: Option<SessionType>,
    pub stdin_null: Option<Flag>,
    pub fork_after_authentication: Option<Flag>,
    pub proxy_use_fdpass: Option<Flag>,
    pub ignored_unknown: Option<String>,
    pub canonical_domains: Vec<String>,
    pub canonicalize_hostname: Option<Canonicalize>,
    pub canonicalize_max_dots: Option<i32>,
    pub canonicalize_fallback_local: Option<Flag>,
    /// Source and target patterns (`none` is the source `none` and an empty target).
    pub permitted_cnames: Vec<(String, String)>,
    pub revoked_host_keys: Vec<String>,
    pub fingerprint_hash: Option<Digest>,
    pub update_hostkeys: Option<YesNoAsk>,
    pub hostbased_accepted_algos: Option<String>,
    pub pubkey_accepted_algos: Option<String>,
    pub jump: Option<Jump>,
    pub known_hosts_command: Option<String>,
    pub required_rsa_size: Option<i32>,
    pub enable_escape_commandline: Option<Flag>,
    pub obscure_keystroke_timing_interval: Option<i32>,
    pub channel_timeouts: Vec<String>,
    pub version_addendum: Option<String>,
    pub warn_weak_crypto: Option<WarnWeakCrypto>,
    pub log_level: Option<LogLevel>,
    pub log_facility: Option<LogFacility>,
    pub log_verbose: Vec<String>,
    pub tag: Option<String>,
}

/// `fill_default_options` values (`readconf.c:2860`–`:3034`).
/// Who gave an `IdentityFile` or `CertificateFile`: the `userprovided` value of
/// `add_identity_file` and `add_certificate_file`. A duplicate is dropped only when this is
/// the same too (`readconf.c:494`-`:500`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProvidedBy {
    /// `0`: the system configuration or the default files.
    NotUser,
    /// `1`: `ssh -i`.
    IdentityOption,
    /// `2` (`SSHCONF_USERCONF`): a user configuration file or `-o`.
    UserConfig,
}

/// `SSH_MAX_IDENTITY_FILES` (`ssh.h`).
pub(crate) const MAX_IDENTITY_FILES: usize = 100;
/// `SSH_MAX_CERTIFICATE_FILES` (`ssh.h`).
pub(crate) const MAX_CERTIFICATE_FILES: usize = 100;

/// `add_identity_file` and `add_certificate_file` reached their limit (OpenSSH stops with
/// `fatal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TooManyFiles {
    pub(crate) max: usize,
}

fn add_file(
    list: &mut Vec<(String, ProvidedBy)>,
    path: String,
    userprovided: ProvidedBy,
    max: usize,
) -> Result<(), TooManyFiles> {
    if list.len() >= max {
        return Err(TooManyFiles { max });
    }
    if !list.iter().any(|(p, u)| *u == userprovided && *p == path) {
        list.push((path, userprovided));
    }
    Ok(())
}

const FORWARD_X11_TIMEOUT: i32 = 1200;
const STREAMLOCAL_BIND_MASK: u32 = 0o177;
const CONNECTION_ATTEMPTS: i32 = 1;
const NUMBER_OF_PASSWORD_PROMPTS: i32 = 3;
const SERVER_ALIVE_COUNT_MAX: i32 = 3;
const CANONICALIZE_MAX_DOTS: i32 = 1;
/// `SSH_RSA_MINIMUM_MODULUS_SIZE` (`sshkey.h`).
const REQUIRED_RSA_SIZE: i32 = 1024;
/// `SSH_KEYSTROKE_DEFAULT_INTERVAL_MS` (`readconf.h`).
pub(crate) const KEYSTROKE_DEFAULT_INTERVAL_MS: i32 = 20;
/// `PATH_XAUTH` of a build configured with `--with-xauth=/usr/bin/xauth`, which is what
/// distributions use.
const XAUTH_PATH: &str = "/usr/bin/xauth";
/// `_PATH_SSH_CLIENT_ID_*` (`pathnames.h`) under `~/`, in the order of `fill_default_options`.
const DEFAULT_IDENTITY_FILES: &[&str] = &[
    "~/.ssh/id_rsa",
    "~/.ssh/id_ecdsa",
    "~/.ssh/id_ecdsa_sk",
    "~/.ssh/id_ed25519",
    "~/.ssh/id_ed25519_sk",
    "~/.ssh/id_mldsa44_ed25519",
];
/// `_PATH_SSH_SYSTEM_HOSTFILE`, `_PATH_SSH_SYSTEM_HOSTFILE2`.
const SYSTEM_HOSTFILES: &[&str] = &["/etc/ssh/ssh_known_hosts", "/etc/ssh/ssh_known_hosts2"];
/// `_PATH_SSH_USER_HOSTFILE`, `_PATH_SSH_USER_HOSTFILE2`.
pub(crate) const USER_HOSTFILE: &str = "~/.ssh/known_hosts";
const USER_HOSTFILE2: &str = "~/.ssh/known_hosts2";
/// The `SecurityKeyProvider` default of a build without the built-in provider.
const SK_PROVIDER_DEFAULT: &str = "$SSH_SK_PROVIDER";

fn or<T>(slot: &mut Option<T>, v: T) {
    if slot.is_none() {
        *slot = Some(v);
    }
}

/// `option_clear_or_none`.
pub(crate) fn clear_or_none(o: Option<&str>) -> bool {
    o.is_none_or(|o| o.eq_ignore_ascii_case("none"))
}

/// Why `fill_default_options` failed: an algorithm list that `kex_assemble_names` rejects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AssembleError(pub(crate) Op);

impl Options {
    /// `clear_forwardings`.
    fn clear_forwardings(&mut self) {
        self.local_forwards.clear();
        self.remote_forwards.clear();
        self.tun_open = Some(Tunnel::No);
    }

    /// `fill_default_options_for_canonicalization`.
    pub(crate) fn fill_for_canonicalization(&mut self) {
        or(&mut self.canonicalize_max_dots, CANONICALIZE_MAX_DOTS);
        or(&mut self.canonicalize_fallback_local, Flag::Yes);
        or(&mut self.canonicalize_hostname, Canonicalize::No);
    }

    /// `fill_default_options`. `ssh -G` has no `-W`, so the stdio forward defaults are off.
    pub(crate) fn fill_defaults(&mut self) -> Result<(), AssembleError> {
        or(&mut self.forward_agent, Flag::No);
        or(&mut self.forward_x11, Flag::No);
        or(&mut self.forward_x11_trusted, Flag::No);
        or(&mut self.forward_x11_timeout, FORWARD_X11_TIMEOUT);
        or(&mut self.exit_on_forward_failure, Flag::No);
        or(&mut self.clear_forwardings, Flag::No);
        if self.clear_forwardings == Some(Flag::Yes) {
            self.clear_forwardings();
        }
        or(&mut self.xauth_location, XAUTH_PATH.to_string());
        or(&mut self.gateway_ports, Flag::No);
        or(&mut self.streamlocal_bind_mask, STREAMLOCAL_BIND_MASK);
        or(&mut self.streamlocal_bind_unlink, Flag::No);
        or(&mut self.pubkey_authentication, PubkeyAuth::All);
        or(&mut self.gss_authentication, Flag::No);
        or(&mut self.gss_deleg_creds, Flag::No);
        or(&mut self.password_authentication, Flag::Yes);
        or(&mut self.kbd_interactive_authentication, Flag::Yes);
        or(&mut self.hostbased_authentication, Flag::No);
        or(&mut self.batch_mode, Flag::No);
        or(&mut self.check_host_ip, Flag::No);
        or(&mut self.strict_host_key_checking, StrictHostKey::Ask);
        or(&mut self.compression, Compression::No);
        or(&mut self.tcp_keep_alive, KeepAlives::Transport);
        // `port` 0 is "filled in ssh_connect"; the resolver puts the default port in.
        or(&mut self.address_family, AddressFamily::Any);
        or(&mut self.connection_attempts, CONNECTION_ATTEMPTS);
        or(
            &mut self.number_of_password_prompts,
            NUMBER_OF_PASSWORD_PROMPTS,
        );
        or(
            &mut self.add_keys_to_agent,
            (AddKeys::No, Default::default()),
        );
        if self.identity_files.is_empty() {
            for f in DEFAULT_IDENTITY_FILES {
                // The list is empty here, so the limit is not reached.
                if self
                    .add_identity_file(f.to_string(), ProvidedBy::NotUser)
                    .is_err()
                {
                    break;
                }
            }
        }
        or(&mut self.escape_char, EscapeChar::Byte(b'~'));
        if self.system_hostfiles.is_empty() {
            self.system_hostfiles = SYSTEM_HOSTFILES.iter().map(|s| s.to_string()).collect();
        }
        if self.update_hostkeys.is_none() {
            let dns_off = self.verify_host_key_dns.is_none_or(|v| v == YesNoAsk::No);
            let default_files = match self.user_hostfiles.as_slice() {
                [] => true,
                [one] => one == USER_HOSTFILE,
                _ => false,
            };
            self.update_hostkeys = Some(if dns_off && default_files {
                YesNoAsk::Yes
            } else {
                YesNoAsk::No
            });
        }
        if self.user_hostfiles.is_empty() {
            self.user_hostfiles = vec![USER_HOSTFILE.to_string(), USER_HOSTFILE2.to_string()];
        }
        or(&mut self.log_level, LogLevel::Info);
        or(&mut self.log_facility, LogFacility::User);
        or(&mut self.no_host_authentication_for_localhost, Flag::No);
        or(&mut self.identities_only, Flag::No);
        or(&mut self.enable_ssh_keysign, Flag::No);
        or(&mut self.rekey_limit, Default::default());
        or(&mut self.rekey_interval, Default::default());
        or(&mut self.verify_host_key_dns, YesNoAsk::No);
        or(&mut self.server_alive_interval, Default::default());
        or(&mut self.server_alive_count_max, SERVER_ALIVE_COUNT_MAX);
        or(&mut self.control_master, ControlMaster::No);
        or(&mut self.control_persist, (false, Default::default()));
        or(&mut self.hash_known_hosts, Flag::No);
        or(&mut self.tun_open, Tunnel::No);
        or(&mut self.tun_device, (TUNID_ANY, TUNID_ANY));
        or(&mut self.permit_local_command, Flag::No);
        or(&mut self.visual_host_key, Flag::No);
        or(&mut self.ip_qos, (IPTOS_DSCP_EF, IPTOS_DSCP_CS0));
        or(&mut self.request_tty, RequestTty::Auto);
        or(&mut self.session_type, SessionType::Default);
        or(&mut self.stdin_null, Flag::No);
        or(&mut self.fork_after_authentication, Flag::No);
        or(&mut self.proxy_use_fdpass, Flag::No);
        self.fill_for_canonicalization();
        or(&mut self.fingerprint_hash, Digest::Sha256);
        or(&mut self.sk_provider, SK_PROVIDER_DEFAULT.to_string());
        or(&mut self.required_rsa_size, REQUIRED_RSA_SIZE);
        or(&mut self.warn_weak_crypto, WarnWeakCrypto::Yes);
        or(&mut self.enable_escape_commandline, Flag::No);
        or(
            &mut self.obscure_keystroke_timing_interval,
            KEYSTROKE_DEFAULT_INTERVAL_MS,
        );

        let assemble = |slot: &mut Option<String>, list: AlgList, op: Op| {
            let v = kex_assemble_names(slot.as_deref(), &list.default_list(), &list.all())
                .ok_or(AssembleError(op))?;
            *slot = Some(v);
            Ok(())
        };
        assemble(&mut self.ciphers, AlgList::Ciphers, Op::Ciphers)?;
        assemble(&mut self.macs, AlgList::Macs, Op::Macs)?;
        assemble(&mut self.kex_algorithms, AlgList::Kex, Op::KexAlgorithms)?;
        assemble(
            &mut self.hostbased_accepted_algos,
            AlgList::HostbasedAccepted,
            Op::HostbasedAcceptedAlgorithms,
        )?;
        assemble(
            &mut self.pubkey_accepted_algos,
            AlgList::PubkeyAccepted,
            Op::PubkeyAcceptedAlgorithms,
        )?;
        assemble(
            &mut self.ca_sign_algorithms,
            AlgList::CaSignature,
            Op::CaSignatureAlgorithms,
        )?;

        for slot in [
            &mut self.local_command,
            &mut self.remote_command,
            &mut self.proxy_command,
            &mut self.control_path,
            &mut self.pkcs11_provider,
            &mut self.sk_provider,
            &mut self.known_hosts_command,
        ] {
            if clear_or_none(slot.as_deref()) {
                *slot = None;
            }
        }
        for list in [&mut self.channel_timeouts, &mut self.revoked_host_keys] {
            if let [one] = list.as_slice()
                && one.eq_ignore_ascii_case("none")
            {
                list.clear();
            }
        }
        if self.jump == Some(Jump::None) {
            self.jump = None;
        }
        if let [(source, target)] = self.permitted_cnames.as_slice()
            && source.eq_ignore_ascii_case("none")
            && target.is_empty()
        {
            self.permitted_cnames.clear();
        }
        Ok(())
    }

    /// `add_identity_file` without a directory: the limit is checked first (even for a
    /// duplicate), then duplicates of the same origin are dropped.
    pub(crate) fn add_identity_file(
        &mut self,
        path: String,
        userprovided: ProvidedBy,
    ) -> Result<(), TooManyFiles> {
        add_file(
            &mut self.identity_files,
            path,
            userprovided,
            MAX_IDENTITY_FILES,
        )
    }

    /// `add_certificate_file`, the same way.
    pub(crate) fn add_certificate_file(
        &mut self,
        path: String,
        userprovided: ProvidedBy,
    ) -> Result<(), TooManyFiles> {
        add_file(
            &mut self.certificate_files,
            path,
            userprovided,
            MAX_CERTIFICATE_FILES,
        )
    }

    /// `dump_client_config`: the text `ssh -G` prints, for the final host name `host`.
    pub fn dump_client_config(&self, host: &str) -> Result<String, String> {
        let mut out = Dump(String::new());
        let hostkeyalgorithms = kex_assemble_names(
            self.hostkeyalgorithms.as_deref(),
            &AlgList::HostKey.default_list(),
            &AlgList::HostKey.all(),
        )
        .ok_or_else(|| "expand HostKeyAlgorithms".to_string())?;

        out.string(Op::Host, Some(&self.host_arg));
        out.string(Op::User, self.user.as_deref());
        out.string(Op::Hostname, Some(host));
        out.int(Op::Port, self.port.map(i32::from));

        out.ms(Op::AddressFamily, self.address_family);
        out.flag(Op::BatchMode, self.batch_mode);
        out.flag(
            Op::CanonicalizeFallbackLocal,
            self.canonicalize_fallback_local,
        );
        out.ms(Op::CanonicalizeHostname, self.canonicalize_hostname);
        out.flag(Op::CheckHostIP, self.check_host_ip);
        out.ms(Op::Compression, self.compression);
        out.ms(Op::ControlMaster, self.control_master);
        out.flag(Op::EnableSshKeysign, self.enable_ssh_keysign);
        out.flag(Op::ClearAllForwardings, self.clear_forwardings);
        out.flag(Op::ExitOnForwardFailure, self.exit_on_forward_failure);
        out.ms(Op::FingerprintHash, self.fingerprint_hash);
        out.flag(Op::ForwardX11, self.forward_x11);
        out.flag(Op::ForwardX11Trusted, self.forward_x11_trusted);
        out.flag(Op::GatewayPorts, self.gateway_ports);
        out.flag(Op::GssAuthentication, self.gss_authentication);
        out.flag(Op::GssDelegateCreds, self.gss_deleg_creds);
        out.flag(Op::HashKnownHosts, self.hash_known_hosts);
        out.flag(Op::HostbasedAuthentication, self.hostbased_authentication);
        out.flag(Op::IdentitiesOnly, self.identities_only);
        out.flag(
            Op::KbdInteractiveAuthentication,
            self.kbd_interactive_authentication,
        );
        out.flag(
            Op::NoHostAuthenticationForLocalhost,
            self.no_host_authentication_for_localhost,
        );
        out.flag(Op::PasswordAuthentication, self.password_authentication);
        out.flag(Op::PermitLocalCommand, self.permit_local_command);
        out.flag(Op::ProxyUseFdpass, self.proxy_use_fdpass);
        out.ms(Op::PubkeyAuthentication, self.pubkey_authentication);
        out.ms(Op::RequestTty, self.request_tty);
        out.ms(Op::SessionType, self.session_type);
        out.flag(Op::StdinNull, self.stdin_null);
        out.flag(Op::ForkAfterAuthentication, self.fork_after_authentication);
        out.flag(Op::StreamLocalBindUnlink, self.streamlocal_bind_unlink);
        out.ms(Op::StrictHostKeyChecking, self.strict_host_key_checking);
        out.ms(Op::TcpKeepAlive, self.tcp_keep_alive);
        out.ms(Op::Tunnel, self.tun_open);
        out.ms(Op::VerifyHostKeyDns, self.verify_host_key_dns);
        out.flag(Op::VisualHostKey, self.visual_host_key);
        out.ms(Op::UpdateHostkeys, self.update_hostkeys);
        out.flag(Op::EnableEscapeCommandline, self.enable_escape_commandline);
        // `fmt_intarg` has no table for WarnWeakCrypto: 0 is `no`, 1 is `yes`.
        out.flag(
            Op::WarnWeakCrypto,
            self.warn_weak_crypto.map(|w| match w {
                WarnWeakCrypto::No => Flag::No,
                WarnWeakCrypto::Yes => Flag::Yes,
            }),
        );

        out.int(Op::CanonicalizeMaxDots, self.canonicalize_max_dots);
        out.int(Op::ConnectionAttempts, self.connection_attempts);
        out.int(Op::ForwardX11Timeout, self.forward_x11_timeout);
        out.int(Op::NumberOfPasswordPrompts, self.number_of_password_prompts);
        out.int(Op::ServerAliveCountMax, self.server_alive_count_max);
        out.int(Op::ServerAliveInterval, self.server_alive_interval);
        out.int(Op::RequiredRsaSize, self.required_rsa_size);
        match self.obscure_keystroke_timing_interval {
            Some(0) => out.line(Op::ObscureKeystrokeTiming, "no"),
            Some(KEYSTROKE_DEFAULT_INTERVAL_MS) => out.line(Op::ObscureKeystrokeTiming, "yes"),
            v => out.int(Op::ObscureKeystrokeTiming, v),
        }

        out.string(Op::BindAddress, self.bind_address.as_deref());
        out.string(Op::BindInterface, self.bind_interface.as_deref());
        out.string(Op::Ciphers, self.ciphers.as_deref());
        out.string(Op::ControlPath, self.control_path.as_deref());
        out.string(Op::HostKeyAlgorithms, Some(&hostkeyalgorithms));
        out.string(Op::HostKeyAlias, self.host_key_alias.as_deref());
        out.string(
            Op::HostbasedAcceptedAlgorithms,
            self.hostbased_accepted_algos.as_deref(),
        );
        out.string(Op::IdentityAgent, self.identity_agent.as_deref());
        out.string(Op::IgnoreUnknown, self.ignored_unknown.as_deref());
        out.string(
            Op::KbdInteractiveDevices,
            self.kbd_interactive_devices.as_deref(),
        );
        out.string(Op::KexAlgorithms, self.kex_algorithms.as_deref());
        out.string(
            Op::CaSignatureAlgorithms,
            self.ca_sign_algorithms.as_deref(),
        );
        out.string(Op::LocalCommand, self.local_command.as_deref());
        out.string(Op::RemoteCommand, self.remote_command.as_deref());
        out.string(Op::LogLevel, self.log_level.map(Multistate::name));
        out.string(Op::Macs, self.macs.as_deref());
        out.string(Op::Pkcs11Provider, self.pkcs11_provider.as_deref());
        out.string(Op::SecurityKeyProvider, self.sk_provider.as_deref());
        out.string(
            Op::PreferredAuthentications,
            self.preferred_authentications.as_deref(),
        );
        out.string(
            Op::PubkeyAcceptedAlgorithms,
            self.pubkey_accepted_algos.as_deref(),
        );
        out.string(Op::XAuthLocation, self.xauth_location.as_deref());
        out.string(Op::KnownHostsCommand, self.known_hosts_command.as_deref());
        out.string(Op::Tag, self.tag.as_deref());
        out.string(Op::VersionAddendum, self.version_addendum.as_deref());

        out.forwards(Op::DynamicForward, &self.local_forwards);
        out.forwards(Op::LocalForward, &self.local_forwards);
        out.forwards(Op::RemoteForward, &self.remote_forwards);

        for (f, _) in &self.identity_files {
            out.line(Op::IdentityFile, f);
        }
        out.oneline(Op::CanonicalDomains, &self.canonical_domains);
        for (f, _) in &self.certificate_files {
            out.line(Op::CertificateFile, f);
        }
        out.oneline(Op::GlobalKnownHostsFile, &self.system_hostfiles);
        out.oneline(Op::UserKnownHostsFile, &self.user_hostfiles);
        out.oneline(Op::RevokedHostKeys, &self.revoked_host_keys);
        for e in &self.send_env {
            out.line(Op::SendEnv, e);
        }
        for e in &self.setenv {
            out.line(Op::SetEnv, e);
        }
        out.oneline(Op::LogVerbose, &self.log_verbose);
        out.oneline(Op::ChannelTimeout, &self.channel_timeouts);

        if self.permitted_remote_opens.is_empty() {
            out.line(Op::PermitRemoteOpen, "any");
        } else {
            out.oneline(Op::PermitRemoteOpen, &self.permitted_remote_opens);
        }
        match self.add_keys_to_agent {
            Some((mode, life)) if life > 0 => {
                let confirm = if mode == AddKeys::Confirm {
                    " confirm"
                } else {
                    ""
                };
                out.raw(&format!("addkeystoagent{confirm} {life}"));
            }
            v => out.ms(Op::AddKeysToAgent, v.map(|(m, _)| m)),
        }
        match &self.forward_agent_sock_path {
            None => out.flag(Op::ForwardAgent, self.forward_agent),
            Some(p) => out.line(Op::ForwardAgent, p),
        }
        match self.connection_timeout {
            None => out.raw("connecttimeout none"),
            v => out.int(Op::ConnectTimeout, v),
        }
        let tun = |t: i32| {
            if t == TUNID_ANY {
                "any".to_string()
            } else {
                t.to_string()
            }
        };
        let (local, remote) = self.tun_device.unwrap_or((TUNID_ANY, TUNID_ANY));
        out.raw(&format!("tunneldevice {}:{}", tun(local), tun(remote)));
        if self.permitted_cnames.is_empty() {
            out.raw("canonicalizepermittedcnames none");
        } else {
            let mut s = String::from("canonicalizepermittedcnames");
            for (a, b) in &self.permitted_cnames {
                s.push_str(&format!(" {a}:{b}"));
            }
            out.raw(&s);
        }
        match self.control_persist {
            Some((true, t)) if t != 0 => out.int(Op::ControlPersist, Some(t)),
            v => out.flag(
                Op::ControlPersist,
                v.map(|(on, _)| if on { Flag::Yes } else { Flag::No }),
            ),
        }
        match self.escape_char {
            Some(EscapeChar::None) => out.raw("escapechar none"),
            Some(EscapeChar::Byte(c)) => out.raw(&format!("escapechar {}", vis_white(c))),
            None => out.raw("escapechar unset"),
        }
        let (interactive, bulk) = self.ip_qos.unwrap_or((IPTOS_DSCP_EF, IPTOS_DSCP_CS0));
        out.raw(&format!(
            "ipqos {} {}",
            iptos2str(interactive),
            iptos2str(bulk)
        ));
        out.raw(&format!(
            "rekeylimit {} {}",
            // `%llu` of the C value.
            self.rekey_limit.unwrap_or_default().cast_unsigned(),
            self.rekey_interval.unwrap_or_default()
        ));
        out.raw(&format!(
            "streamlocalbindmask 0{:o}",
            self.streamlocal_bind_mask.unwrap_or_default()
        ));
        out.raw(&format!(
            "syslogfacility {}",
            self.log_facility.map(Multistate::name).unwrap_or("UNKNOWN")
        ));
        match &self.jump {
            None => out.string(Op::ProxyCommand, self.proxy_command.as_deref()),
            Some(Jump::None) => out.raw("proxyjump none"),
            Some(Jump::Hosts {
                user,
                host,
                port,
                extra,
            }) => {
                let numeric =
                    host.contains(':') || host.bytes().all(|c| c.is_ascii_digit() || c == b'.');
                let mut s = String::from("proxyjump ");
                if let Some(e) = extra {
                    s.push_str(&format!("{e},"));
                }
                if let Some(u) = user {
                    s.push_str(&format!("{u}@"));
                }
                if numeric {
                    s.push_str(&format!("[{host}]"));
                } else {
                    s.push_str(host);
                }
                if let Some(p) = port.filter(|p| *p > 0) {
                    s.push_str(&format!(":{p}"));
                }
                out.raw(&s);
            }
        }
        Ok(out.0)
    }
}

/// The text `vis(buf, c, VIS_WHITE, 0)` gives for one byte. Written for this crate from the
/// documented `vis(3)` output, not derived from `openbsd-compat/vis.c`.
fn vis_white(c: u8) -> String {
    const META: u8 = 0x80;
    const DEL: u8 = 0x7f;
    const CTRL_OFFSET: u8 = b'@';
    if c.is_ascii_graphic() {
        return if c == b'\\' {
            "\\\\".to_string()
        } else {
            char::from(c).to_string()
        };
    }
    if c & DEL == b' ' {
        return format!("\\{c:03o}");
    }
    let mut s = String::from("\\");
    let mut c = c;
    if c & META != 0 {
        c &= DEL;
        s.push('M');
    }
    if c.is_ascii_control() {
        s.push('^');
        s.push(if c == DEL {
            '?'
        } else {
            char::from(c + CTRL_OFFSET)
        });
    } else {
        s.push('-');
        s.push(char::from(c));
    }
    s
}

/// The `dump_cfg_*` printers.
struct Dump(String);

impl Dump {
    fn raw(&mut self, s: &str) {
        self.0.push_str(s);
        self.0.push('\n');
    }

    fn line(&mut self, op: Op, v: &str) {
        self.0.push_str(&format!("{} {v}\n", keyword_name(op)));
    }

    fn string(&mut self, op: Op, v: Option<&str>) {
        if let Some(v) = v {
            self.line(op, v);
        }
    }

    fn int(&mut self, op: Op, v: Option<i32>) {
        match v {
            Some(v) => self.line(op, &v.to_string()),
            None => self.line(op, "-1"),
        }
    }

    fn flag(&mut self, op: Op, v: Option<Flag>) {
        self.line(
            op,
            match v {
                None => "unset",
                Some(Flag::Yes) => "yes",
                Some(Flag::No) => "no",
            },
        );
    }

    fn ms<T: Multistate>(&mut self, op: Op, v: Option<T>) {
        self.line(op, v.map(Multistate::name).unwrap_or("unset"));
    }

    fn oneline(&mut self, op: Op, vals: &[String]) {
        let mut s = keyword_name(op).to_string();
        if vals.is_empty() {
            s.push_str(" none");
        }
        for v in vals {
            s.push(' ');
            s.push_str(v);
        }
        self.raw(&s);
    }

    fn forwards(&mut self, op: Op, fwds: &[Forward]) {
        let end = |e: &ForwardEnd| match e {
            ForwardEnd::Path(p) => p.clone(),
            ForwardEnd::Port { host: None, port } => port.to_string(),
            ForwardEnd::Port {
                host: Some(h),
                port,
            } => format!("[{h}]:{port}"),
        };
        for f in fwds {
            let socks =
                matches!(&f.connect, ForwardEnd::Port { host: Some(h), .. } if h == "socks");
            let has_host = matches!(&f.connect, ForwardEnd::Port { host: Some(_), .. });
            if op == Op::DynamicForward && has_host && !socks {
                continue;
            }
            if op == Op::LocalForward && socks {
                continue;
            }
            let mut s = format!("{} {}", keyword_name(op), end(&f.listen));
            if op != Op::DynamicForward {
                s.push(' ');
                s.push_str(&end(&f.connect));
            }
            self.raw(&s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multistate_names_are_first_keys() {
        assert_eq!(ControlMaster::No.name(), "false");
        assert_eq!(StrictHostKey::Off.name(), "false");
        assert_eq!(StrictHostKey::New.name(), "accept-new");
        assert_eq!(Tunnel::PointToPoint.name(), "point-to-point");
        assert_eq!(LogLevel::Debug1.name(), "DEBUG");
        assert_eq!(Compression::Delayed.name(), "yes");
        assert_eq!(StrictHostKey::parse("ACCEPT-NEW"), Some(StrictHostKey::New));
        assert_eq!(Flag::parse("maybe"), None);
    }

    #[test]
    fn vis() {
        assert_eq!(vis_white(b'~'), "~");
        assert_eq!(vis_white(b'\\'), "\\\\");
        assert_eq!(vis_white(1), "\\^A");
        assert_eq!(vis_white(b' '), "\\040");
        assert_eq!(vis_white(0x7f), "\\^?");
        assert_eq!(vis_white(0xe9), "\\M-i");
    }

    #[test]
    fn defaults_dump() {
        let mut o = Options {
            host_arg: "h".to_string(),
            port: Some(22),
            user: Some("u".to_string()),
            ..Options::default()
        };
        o.fill_defaults().unwrap();
        let d = o.dump_client_config("h").unwrap();
        assert!(d.starts_with("host h\nuser u\nhostname h\nport 22\naddressfamily any\n"));
        assert!(d.contains("\nidentityfile ~/.ssh/id_mldsa44_ed25519\n"));
        assert!(d.contains("\nconnecttimeout none\n"));
        assert!(d.contains("\nipqos ef cs0\n"));
        assert!(d.contains("\nstreamlocalbindmask 0177\n"));
        assert!(d.contains("\ntunneldevice any:any\n"));
        assert!(d.contains("\nobscurekeystroketiming yes\n"));
        assert!(d.contains("\nupdatehostkeys true\n"));
        assert!(d.contains("\nsecuritykeyprovider $SSH_SK_PROVIDER\n"));
        assert!(d.ends_with("syslogfacility USER\n"));
    }
}
