// This file is derived from OpenSSH, modified: a Rust port of parts of cipher.c, mac.c, kex-names.c, sshkey.c, myproposal.h
// of OpenSSH portable 10.6p2 (commit d449e662b6b8b8cb20e852d8084c269af1c5e8f1). The
// notices of those files follow as written there; LICENSE-OpenSSH has the OpenSSH licence.
//
// cipher.c:
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
//  *
//  *
//  * Copyright (c) 1999 Niels Provos.  All rights reserved.
//  * Copyright (c) 1999, 2000 Markus Friedl.  All rights reserved.
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
// mac.c:
// /*
//  * Copyright (c) 2001 Markus Friedl.  All rights reserved.
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
// kex-names.c:
// /*
//  * Copyright (c) 2000, 2001 Markus Friedl.  All rights reserved.
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
// sshkey.c:
// /*
//  * Copyright (c) 2000, 2001 Markus Friedl.  All rights reserved.
//  * Copyright (c) 2008 Alexander von Gernler.  All rights reserved.
//  * Copyright (c) 2010,2011 Damien Miller.  All rights reserved.
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
// myproposal.h:
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
//! Algorithm names and lists: the name tables of OpenSSH `cipher.c`, `mac.c`,
//! `kex-names.c` and `sshkey.c` for a build with OpenSSL, security keys and the post-quantum
//! algorithms, the default lists of `myproposal.h`, and `kex_assemble_names`.

use crate::matching::{ListMatch, match_filter_allowlist, match_filter_denylist, match_list};

/// `ciphers[]` of `cipher.c` without the internal `none`.
pub(crate) const CIPHERS: &[&str] = &[
    "3des-cbc",
    "aes128-cbc",
    "aes192-cbc",
    "aes256-cbc",
    "aes128-ctr",
    "aes192-ctr",
    "aes256-ctr",
    "aes128-gcm@openssh.com",
    "aes256-gcm@openssh.com",
    "chacha20-poly1305@openssh.com",
];

/// `macs[]` of `mac.c`.
pub(crate) const MACS: &[&str] = &[
    "hmac-sha1",
    "hmac-sha1-96",
    "hmac-sha2-256",
    "hmac-sha2-512",
    "hmac-md5",
    "hmac-md5-96",
    "umac-64@openssh.com",
    "umac-128@openssh.com",
    "hmac-sha1-etm@openssh.com",
    "hmac-sha1-96-etm@openssh.com",
    "hmac-sha2-256-etm@openssh.com",
    "hmac-sha2-512-etm@openssh.com",
    "hmac-md5-etm@openssh.com",
    "hmac-md5-96-etm@openssh.com",
    "umac-64-etm@openssh.com",
    "umac-128-etm@openssh.com",
];

/// `kexalgs[]` of `kex-names.c`.
pub(crate) const KEX: &[&str] = &[
    "diffie-hellman-group1-sha1",
    "diffie-hellman-group14-sha1",
    "diffie-hellman-group14-sha256",
    "diffie-hellman-group16-sha512",
    "diffie-hellman-group18-sha512",
    "diffie-hellman-group-exchange-sha1",
    "diffie-hellman-group-exchange-sha256",
    "ecdh-sha2-nistp256",
    "ecdh-sha2-nistp384",
    "ecdh-sha2-nistp521",
    "mlkem768nistp256-sha256",
    "curve25519-sha256",
    "curve25519-sha256@libssh.org",
    "sntrup761x25519-sha512",
    "sntrup761x25519-sha512@openssh.com",
    "mlkem768x25519-sha256",
];

/// One entry of `keyimpls[]` of `sshkey.c`.
/// `sigonly` is left out: every list this crate builds includes the signature-only names
/// (`sshkey_alg_list(.., include_sigonly = 1, ..)` in `fill_default_options`).
struct KeyImpl {
    name: &'static str,
    cert: bool,
}

const fn key(name: &'static str, cert: bool) -> KeyImpl {
    KeyImpl { name, cert }
}

/// `keyimpls[]` of `sshkey.c`.
const KEYS: &[KeyImpl] = &[
    key("ssh-ed25519", false),
    key("ssh-ed25519-cert-v01@openssh.com", true),
    key("sk-ssh-ed25519@openssh.com", false),
    key("sk-ssh-ed25519-cert-v01@openssh.com", true),
    key("ssh-mldsa44-ed25519", false),
    key("ssh-mldsa44-ed25519-cert", true),
    key("ecdsa-sha2-nistp256", false),
    key("ecdsa-sha2-nistp256-cert-v01@openssh.com", true),
    key("ecdsa-sha2-nistp384", false),
    key("ecdsa-sha2-nistp384-cert-v01@openssh.com", true),
    key("ecdsa-sha2-nistp521", false),
    key("ecdsa-sha2-nistp521-cert-v01@openssh.com", true),
    key("sk-ecdsa-sha2-nistp256@openssh.com", false),
    key("sk-ecdsa-sha2-nistp256-cert-v01@openssh.com", true),
    key("webauthn-sk-ecdsa-sha2-nistp256@openssh.com", false),
    key("webauthn-sk-ecdsa-sha2-nistp256-cert-v01@openssh.com", true),
    key("ssh-rsa", false),
    key("ssh-rsa-cert-v01@openssh.com", true),
    key("rsa-sha2-256", false),
    key("rsa-sha2-256-cert-v01@openssh.com", true),
    key("rsa-sha2-512", false),
    key("rsa-sha2-512-cert-v01@openssh.com", true),
];

/// `KEX_CLIENT_KEX` of `myproposal.h`.
const DEFAULT_KEX: &str = "mlkem768x25519-sha256,sntrup761x25519-sha512,\
sntrup761x25519-sha512@openssh.com,curve25519-sha256,curve25519-sha256@libssh.org,\
ecdh-sha2-nistp256,ecdh-sha2-nistp384,ecdh-sha2-nistp521,\
diffie-hellman-group-exchange-sha256,diffie-hellman-group16-sha512,\
diffie-hellman-group18-sha512,diffie-hellman-group14-sha256";

/// `KEX_DEFAULT_PK_ALG` of `myproposal.h`.
const DEFAULT_PK_ALG: &str = "ssh-ed25519-cert-v01@openssh.com,\
ecdsa-sha2-nistp256-cert-v01@openssh.com,ecdsa-sha2-nistp384-cert-v01@openssh.com,\
ecdsa-sha2-nistp521-cert-v01@openssh.com,sk-ssh-ed25519-cert-v01@openssh.com,\
sk-ecdsa-sha2-nistp256-cert-v01@openssh.com,\
webauthn-sk-ecdsa-sha2-nistp256-cert-v01@openssh.com,rsa-sha2-512-cert-v01@openssh.com,\
rsa-sha2-256-cert-v01@openssh.com,ssh-mldsa44-ed25519-cert,ssh-ed25519,\
ecdsa-sha2-nistp256,ecdsa-sha2-nistp384,ecdsa-sha2-nistp521,sk-ssh-ed25519@openssh.com,\
sk-ecdsa-sha2-nistp256@openssh.com,webauthn-sk-ecdsa-sha2-nistp256@openssh.com,\
rsa-sha2-512,rsa-sha2-256,ssh-mldsa44-ed25519";

/// `KEX_CLIENT_ENCRYPT` of `myproposal.h`.
const DEFAULT_CIPHERS: &str = "chacha20-poly1305@openssh.com,aes128-gcm@openssh.com,\
aes256-gcm@openssh.com,aes128-ctr,aes192-ctr,aes256-ctr";

/// `KEX_CLIENT_MAC` of `myproposal.h`.
const DEFAULT_MACS: &str = "umac-64-etm@openssh.com,umac-128-etm@openssh.com,\
hmac-sha2-256-etm@openssh.com,hmac-sha2-512-etm@openssh.com,hmac-sha1-etm@openssh.com,\
umac-64@openssh.com,umac-128@openssh.com,hmac-sha2-256,hmac-sha2-512,hmac-sha1";

/// `SSH_ALLOWED_CA_SIGALGS` of `myproposal.h`.
const DEFAULT_CA_SIGALGS: &str = "ssh-ed25519,ecdsa-sha2-nistp256,ecdsa-sha2-nistp384,\
ecdsa-sha2-nistp521,sk-ssh-ed25519@openssh.com,sk-ecdsa-sha2-nistp256@openssh.com,\
webauthn-sk-ecdsa-sha2-nistp256@openssh.com,rsa-sha2-512,rsa-sha2-256,ssh-mldsa44-ed25519";

/// Which algorithm list a keyword sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlgList {
    Ciphers,
    Macs,
    Kex,
    /// `HostKeyAlgorithms` (assembled at connection time, not in `fill_default_options`).
    HostKey,
    HostbasedAccepted,
    PubkeyAccepted,
    CaSignature,
}

fn key_list(plain_only: bool) -> String {
    KEYS.iter()
        .filter(|k| !(plain_only && k.cert))
        .map(|k| k.name)
        .collect::<Vec<_>>()
        .join(",")
}

impl AlgList {
    /// The list of every supported name (`cipher_alg_list`, `mac_alg_list`,
    /// `kex_alg_list`, `sshkey_alg_list`).
    pub(crate) fn all(self) -> String {
        match self {
            AlgList::Ciphers => CIPHERS.join(","),
            AlgList::Macs => MACS.join(","),
            AlgList::Kex => KEX.join(","),
            AlgList::HostKey | AlgList::HostbasedAccepted | AlgList::PubkeyAccepted => {
                key_list(false)
            }
            AlgList::CaSignature => key_list(true),
        }
    }

    /// The default list with unsupported names removed (`fill_default_options`).
    pub(crate) fn default_list(self) -> String {
        let def = match self {
            AlgList::Ciphers => DEFAULT_CIPHERS,
            AlgList::Macs => DEFAULT_MACS,
            AlgList::Kex => DEFAULT_KEX,
            AlgList::HostKey | AlgList::HostbasedAccepted | AlgList::PubkeyAccepted => {
                DEFAULT_PK_ALG
            }
            AlgList::CaSignature => DEFAULT_CA_SIGALGS,
        };
        match_filter_allowlist(def, &self.all())
    }

    /// The validation a keyword applies to its value after `+`, `-` or `^` (`readconf.c`).
    /// A `-` list is not validated.
    pub(crate) fn valid(self, names: &str) -> bool {
        match self {
            AlgList::Ciphers => names_valid(names, |p| CIPHERS.contains(&p)),
            AlgList::Macs => names_valid(names, |p| MACS.contains(&p)),
            AlgList::Kex => kex_names_valid(names),
            AlgList::HostKey | AlgList::HostbasedAccepted | AlgList::PubkeyAccepted => {
                sshkey_names_valid2(names, false)
            }
            AlgList::CaSignature => sshkey_names_valid2(names, true),
        }
    }
}

/// `ciphers_valid`, `mac_valid`: every name before the first empty one is known, and there
/// is at least one.
fn names_valid(names: &str, known: impl Fn(&str) -> bool) -> bool {
    let mut found = false;
    for p in names.split(',').take_while(|p| !p.is_empty()) {
        if !known(p) {
            return false;
        }
        found = true;
    }
    found
}

/// `kex_names_valid`: not empty and every name before the first empty one is known.
fn kex_names_valid(names: &str) -> bool {
    !names.is_empty()
        && names
            .split(',')
            .take_while(|p| !p.is_empty())
            .all(|p| KEX.contains(&p))
}

/// `sshkey_names_valid2(names, 1, plain_only)`: each name is a key type, or a pattern that
/// some key type matches (positively or negatively).
fn sshkey_names_valid2(names: &str, plain_only: bool) -> bool {
    if names.is_empty() {
        return false;
    }
    for p in names.split(',').take_while(|p| !p.is_empty()) {
        match KEYS.iter().find(|k| k.name == p) {
            Some(k) if plain_only && k.cert => return false,
            Some(_) => {}
            None => {
                let any = KEYS.iter().any(|k| {
                    crate::matching::match_pattern_list(k.name, p, false) != ListMatch::None
                });
                if !any {
                    return false;
                }
            }
        }
    }
    true
}

/// `kex_names_cat`: `a` followed by the names of `b` that are not in `a`.
fn kex_names_cat(a: &str, b: &str) -> String {
    if a.is_empty() {
        return b.to_string();
    }
    if b.is_empty() {
        return a.to_string();
    }
    let mut ret = a.to_string();
    for p in b.split(',').take_while(|p| !p.is_empty()) {
        if match_list(&ret, p).is_some() {
            continue;
        }
        ret.push(',');
        ret.push_str(p);
    }
    ret
}

/// `kex_assemble_names`: the list a keyword value makes from the default list. `None` is
/// an error (a `!` pattern or an empty result).
pub(crate) fn kex_assemble_names(list: Option<&str>, def: &str, all: &str) -> Option<String> {
    let list = match list {
        None | Some("") => return Some(def.to_string()),
        Some(l) => l,
    };
    let list = if let Some(rest) = list.strip_prefix('+') {
        kex_names_cat(def, rest)
    } else if let Some(rest) = list.strip_prefix('-') {
        return Some(match_filter_denylist(def, rest));
    } else if let Some(rest) = list.strip_prefix('^') {
        kex_names_cat(rest, def)
    } else {
        list.to_string()
    };
    let mut ret = String::new();
    for cp in list.split(',') {
        if cp.starts_with('!') {
            return None;
        }
        let matching = match_filter_allowlist(all, cp);
        ret = kex_names_cat(&ret, &matching);
    }
    (!ret.is_empty()).then_some(ret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_drop_nothing_in_this_build() {
        for l in [
            AlgList::Ciphers,
            AlgList::Macs,
            AlgList::Kex,
            AlgList::PubkeyAccepted,
            AlgList::CaSignature,
        ] {
            let def = l.default_list();
            assert!(!def.is_empty());
            assert!(l.valid(&def), "{l:?}");
        }
        assert_eq!(AlgList::Ciphers.default_list(), DEFAULT_CIPHERS);
    }

    #[test]
    fn assemble() {
        let all = "a,b,c,d";
        assert_eq!(kex_assemble_names(None, "a,b", all).as_deref(), Some("a,b"));
        assert_eq!(
            kex_assemble_names(Some("+c"), "a,b", all).as_deref(),
            Some("a,b,c")
        );
        assert_eq!(
            kex_assemble_names(Some("-a"), "a,b", all).as_deref(),
            Some("b")
        );
        assert_eq!(
            kex_assemble_names(Some("^c,a"), "a,b", all).as_deref(),
            Some("c,a,b")
        );
        assert_eq!(
            kex_assemble_names(Some("*"), "a,b", all).as_deref(),
            Some("a,b,c,d")
        );
        assert_eq!(kex_assemble_names(Some("x"), "a,b", all), None);
        assert_eq!(kex_assemble_names(Some("!a"), "a,b", all), None);
    }

    #[test]
    fn validation() {
        assert!(AlgList::Ciphers.valid("aes128-ctr,aes256-ctr"));
        assert!(!AlgList::Ciphers.valid("none"));
        assert!(!AlgList::Ciphers.valid(""));
        assert!(AlgList::PubkeyAccepted.valid("ssh-ed25519*"));
        assert!(AlgList::PubkeyAccepted.valid("ssh-*-cert-v01@openssh.com"));
        assert!(!AlgList::PubkeyAccepted.valid("nope*x"));
        assert!(!AlgList::CaSignature.valid("ssh-rsa-cert-v01@openssh.com"));
        assert!(AlgList::Kex.valid("curve25519-sha256,"));
    }
}
