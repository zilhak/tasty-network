//! The algorithms the embedded backend proposes: the OpenSSH lists of the resolution (the
//! defaults of `myproposal.h` with the directives' `+`/`-`/`^`) cut to what russh implements
//! (docs/design/ssh-backends.md "알고리즘").

use std::borrow::Cow;

use russh::keys::Algorithm;
use russh::keys::key::ALL_KEY_TYPES;
use russh::{Preferred, cipher, compression, kex, mac};
use tasty_net_ssh_config::Resolution;

use crate::inspect::{ConfigNote, NoteClass, RefusalNote};

use super::resolve::backend_note;

/// The algorithms of one connection, or the keyword whose list russh cannot serve at all.
pub(crate) struct Proposal {
    pub(crate) preferred: Preferred,
    /// The accepted signature algorithms of user keys (`PubkeyAcceptedAlgorithms`).
    pub(crate) pubkey_accepted: Vec<String>,
}

/// The host key algorithms of the resolution as `ssh -G` prints them (the port assembles
/// them only for that output).
fn dumped(res: &Resolution, keyword: &str) -> Option<String> {
    let dump = res.dump_client_config().ok()?;
    dump.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == keyword).then(|| v.to_string())
    })
}

fn names(list: Option<&str>) -> Vec<String> {
    list.unwrap_or_default()
        .split(',')
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

/// The names of `wanted` that `known` has, as `known`'s values, and the names left out.
fn intersect<N: Copy + AsRef<str>>(wanted: &[String], known: &[&N]) -> (Vec<N>, Vec<String>) {
    let mut kept = Vec::new();
    let mut left = Vec::new();
    for w in wanted {
        let found = known.iter().find(|k| {
            let name: &str = (***k).as_ref();
            name == w && w != "none"
        });
        match found {
            Some(k) => kept.push(**k),
            None => left.push(w.clone()),
        }
    }
    (kept, left)
}

/// A host key algorithm russh checks (no DSA, no certificates in this stage).
fn host_key_algorithm(name: &str) -> Option<Algorithm> {
    if name.contains("-cert-") {
        return None;
    }
    let alg = Algorithm::new(name).ok()?;
    (alg != Algorithm::Dsa && ALL_KEY_TYPES.contains(&alg)).then_some(alg)
}

/// Builds the proposal of `res`. `known_types` are the key types the known_hosts files hold
/// for the host, which `order_hostkeyalgs` moves to the front (`sshconnect2.c:121`).
pub(crate) fn proposal(
    res: &Resolution,
    known_types: &[String],
    notes: &mut Vec<ConfigNote>,
) -> Result<Proposal, RefusalNote> {
    let mut note_left = |keyword: &str, left: &[String]| {
        let origin = res.source_of(keyword);
        if !left.is_empty() && origin != tasty_net_ssh_config::Origin::Default {
            let note = backend_note(keyword, &origin, NoteClass::AlgorithmNotImplemented);
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
    };
    let empty = |keyword: &str| RefusalNote {
        kind: "security_not_implemented",
        keyword: Some(keyword.to_string()),
        origin: Some(res.source_of(keyword).to_string()),
        message: format!("none of the {keyword} the configuration allows is implemented"),
    };

    let (mut kex_names, left) = intersect(
        &names(res.options.kex_algorithms.as_deref()),
        kex::ALL_KEX_ALGORITHMS,
    );
    note_left("kexalgorithms", &left);
    if kex_names.is_empty() {
        return Err(empty("kexalgorithms"));
    }
    // Always offered, whatever the list (OpenSSH does the same).
    kex_names.push(kex::EXTENSION_SUPPORT_AS_CLIENT);
    kex_names.push(kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT);

    let (ciphers, left) = intersect(&names(res.options.ciphers.as_deref()), cipher::ALL_CIPHERS);
    note_left("ciphers", &left);
    if ciphers.is_empty() {
        return Err(empty("ciphers"));
    }
    let (macs, left) = intersect(&names(res.options.macs.as_deref()), mac::ALL_MAC_ALGORITHMS);
    note_left("macs", &left);
    if macs.is_empty() {
        return Err(empty("macs"));
    }

    let mut host_algs = Vec::new();
    let mut left = Vec::new();
    for name in names(dumped(res, "hostkeyalgorithms").as_deref()) {
        match host_key_algorithm(&name) {
            Some(alg) => host_algs.push(alg),
            // Certificate types are left out on purpose in this stage, not reported.
            None if name.contains("-cert-") => {}
            None => left.push(name),
        }
    }
    note_left("hostkeyalgorithms", &left);
    if host_algs.is_empty() {
        return Err(empty("hostkeyalgorithms"));
    }
    // Known types first, keeping their order (order_hostkeyalgs).
    let known = |alg: &Algorithm| {
        known_types.iter().any(|t| match Algorithm::new(t) {
            Ok(Algorithm::Rsa { .. }) => matches!(alg, Algorithm::Rsa { .. }),
            Ok(k) => k == *alg,
            Err(_) => false,
        })
    };
    host_algs.sort_by_key(|alg| !known(alg));

    let pubkey_accepted = names(res.options.pubkey_accepted_algos.as_deref());

    Ok(Proposal {
        preferred: Preferred {
            kex: Cow::Owned(kex_names),
            key: Cow::Owned(host_algs),
            host_key_certificates: Cow::Borrowed(&[]),
            cipher: Cow::Owned(ciphers),
            mac: Cow::Owned(macs),
            // `Compression yes` is shown and not applied (no flate2).
            compression: Cow::Borrowed(&[compression::NONE]),
        },
        pubkey_accepted,
    })
}
