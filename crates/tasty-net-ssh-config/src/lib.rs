//! OpenSSH client configuration (`ssh_config`) parsing, ported from OpenSSH portable 10.6p2
//! (docs/design/ssh-backends.md "설정 해석").
//!
//! [`ConfigSet::load`] reads the top-level files and [`ConfigSet::resolve`] applies them to a
//! destination the way `ssh` does up to `ssh -G`: `Host` and `Match` blocks, the first value
//! wins, `Include` with its depth limit, `%` tokens and `${NAME}` environment values,
//! `IgnoreUnknown`, then the configuration stage of `ssh.c`. The [`Resolution`] carries the
//! options, where each value came from, notes about keywords this crate does not apply, and a
//! refusal when the resolution cannot be used.
//!
//! The crate never runs a program and does no name service lookup: what needs one
//! (`Match exec`, `Match localnetwork`, `CanonicalizeHostname` with a name that is not an
//! address) is refused as [`RefusalKind::Undeterminable`] or
//! [`RefusalKind::SecurityNotImplemented`]. Numeric host names are read by the rule of the C
//! library of the OS (glibc, bionic, Darwin), as `ssh` reads them there; on an OS without a
//! rule, a name the libraries read differently is refused as
//! [`RefusalKind::Undeterminable`].
//!
//! The base version and the ported files, functions and line ranges are in the crate README.
//! Files derived from OpenSSH keep the original notices and are under `LICENSE-OpenSSH`.

#![forbid(unsafe_code)]

mod algorithms;
mod glob;
mod hostfile;
pub mod keywords;
mod matching;
mod misc;
pub mod options;
mod readconf;
mod resolve;
mod result;
mod scaled;
pub mod settings;

pub use hostfile::{HostKeyEntry, HostKeyQuery, HostKeyVerdict, KnownHosts, LineKey, Marker};
pub use resolve::{
    CommandLine, CommandLineValue, ConfigError, ConfigFiles, ConfigInputs, ConfigSet, Resolution,
    ResolveContext,
};
pub use result::{ConfigNote, NoteAction, NoteClass, Origin, Refusal, RefusalKind, ValueSource};
