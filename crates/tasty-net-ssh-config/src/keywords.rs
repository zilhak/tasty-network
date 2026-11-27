// This file is derived from OpenSSH, modified: a Rust port of parts of readconf.c
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
//! The keyword table of OpenSSH `readconf.c` (`keywords[]`, `:178`–`:325`) for a build with
//! GSSAPI and PKCS#11, and the treatment of each keyword in this crate
//! (docs/design/ssh-backends.md "지시어 분류").

/// `OpCodes` of `readconf.c`, without `oBadOption` and `oIgnoredUnknownOption` (those are
/// lookup results, see [`Lookup`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Op {
    Host,
    Match,
    Include,
    Tag,
    ForwardAgent,
    ForwardX11,
    ForwardX11Trusted,
    ForwardX11Timeout,
    ExitOnForwardFailure,
    XAuthLocation,
    GatewayPorts,
    PasswordAuthentication,
    Hostname,
    IdentityFile,
    Port,
    LocalForward,
    RemoteForward,
    PermitRemoteOpen,
    CertificateFile,
    AddKeysToAgent,
    IdentityAgent,
    User,
    EscapeChar,
    ProxyCommand,
    GlobalKnownHostsFile,
    UserKnownHostsFile,
    ConnectionAttempts,
    BatchMode,
    CheckHostIP,
    StrictHostKeyChecking,
    Compression,
    TcpKeepAlive,
    NumberOfPasswordPrompts,
    LogFacility,
    LogLevel,
    LogVerbose,
    Ciphers,
    Macs,
    PubkeyAuthentication,
    KbdInteractiveAuthentication,
    KbdInteractiveDevices,
    HostKeyAlias,
    DynamicForward,
    PreferredAuthentications,
    HostbasedAuthentication,
    HostKeyAlgorithms,
    BindAddress,
    BindInterface,
    Pkcs11Provider,
    ClearAllForwardings,
    NoHostAuthenticationForLocalhost,
    EnableSshKeysign,
    RekeyLimit,
    VerifyHostKeyDns,
    ConnectTimeout,
    AddressFamily,
    GssAuthentication,
    GssDelegateCreds,
    ServerAliveInterval,
    ServerAliveCountMax,
    IdentitiesOnly,
    SendEnv,
    SetEnv,
    ControlPath,
    ControlMaster,
    ControlPersist,
    HashKnownHosts,
    Tunnel,
    TunnelDevice,
    LocalCommand,
    PermitLocalCommand,
    RemoteCommand,
    VisualHostKey,
    KexAlgorithms,
    IpQos,
    RequestTty,
    SessionType,
    StdinNull,
    ForkAfterAuthentication,
    IgnoreUnknown,
    ProxyUseFdpass,
    CanonicalDomains,
    CanonicalizeHostname,
    CanonicalizeMaxDots,
    CanonicalizeFallbackLocal,
    CanonicalizePermittedCnames,
    StreamLocalBindMask,
    StreamLocalBindUnlink,
    RevokedHostKeys,
    FingerprintHash,
    UpdateHostkeys,
    HostbasedAcceptedAlgorithms,
    PubkeyAcceptedAlgorithms,
    CaSignatureAlgorithms,
    ProxyJump,
    SecurityKeyProvider,
    KnownHostsCommand,
    RequiredRsaSize,
    EnableEscapeCommandline,
    ObscureKeystrokeTiming,
    ChannelTimeout,
    VersionAddendum,
    RefuseConnection,
    WarnWeakCrypto,
    /// `oIgnore`.
    Ignore,
    /// `oDeprecated`.
    Deprecated,
    /// `oUnsupported`.
    Unsupported,
}

/// `keywords[]` in the original order, with the GSSAPI and PKCS#11 branches enabled.
pub(crate) const KEYWORDS: &[(&str, Op)] = &[
    ("protocol", Op::Ignore),
    ("cipher", Op::Deprecated),
    ("fallbacktorsh", Op::Deprecated),
    ("globalknownhostsfile2", Op::Deprecated),
    ("rhostsauthentication", Op::Deprecated),
    ("userknownhostsfile2", Op::Deprecated),
    ("useroaming", Op::Deprecated),
    ("usersh", Op::Deprecated),
    ("useprivilegedport", Op::Deprecated),
    ("afstokenpassing", Op::Unsupported),
    ("kerberosauthentication", Op::Unsupported),
    ("kerberostgtpassing", Op::Unsupported),
    ("rsaauthentication", Op::Unsupported),
    ("rhostsrsaauthentication", Op::Unsupported),
    ("compressionlevel", Op::Unsupported),
    ("gssapiauthentication", Op::GssAuthentication),
    ("gssapidelegatecredentials", Op::GssDelegateCreds),
    ("pkcs11provider", Op::Pkcs11Provider),
    ("smartcarddevice", Op::Pkcs11Provider),
    ("forwardagent", Op::ForwardAgent),
    ("forwardx11", Op::ForwardX11),
    ("forwardx11trusted", Op::ForwardX11Trusted),
    ("forwardx11timeout", Op::ForwardX11Timeout),
    ("exitonforwardfailure", Op::ExitOnForwardFailure),
    ("xauthlocation", Op::XAuthLocation),
    ("gatewayports", Op::GatewayPorts),
    ("passwordauthentication", Op::PasswordAuthentication),
    (
        "kbdinteractiveauthentication",
        Op::KbdInteractiveAuthentication,
    ),
    ("kbdinteractivedevices", Op::KbdInteractiveDevices),
    (
        "challengeresponseauthentication",
        Op::KbdInteractiveAuthentication,
    ),
    ("skeyauthentication", Op::KbdInteractiveAuthentication),
    ("tisauthentication", Op::KbdInteractiveAuthentication),
    ("pubkeyauthentication", Op::PubkeyAuthentication),
    ("dsaauthentication", Op::PubkeyAuthentication),
    ("hostbasedauthentication", Op::HostbasedAuthentication),
    ("identityfile", Op::IdentityFile),
    ("identityfile2", Op::IdentityFile),
    ("identitiesonly", Op::IdentitiesOnly),
    ("certificatefile", Op::CertificateFile),
    ("addkeystoagent", Op::AddKeysToAgent),
    ("identityagent", Op::IdentityAgent),
    ("hostname", Op::Hostname),
    ("hostkeyalias", Op::HostKeyAlias),
    ("proxycommand", Op::ProxyCommand),
    ("port", Op::Port),
    ("ciphers", Op::Ciphers),
    ("macs", Op::Macs),
    ("remoteforward", Op::RemoteForward),
    ("localforward", Op::LocalForward),
    ("permitremoteopen", Op::PermitRemoteOpen),
    ("user", Op::User),
    ("host", Op::Host),
    ("match", Op::Match),
    ("tag", Op::Tag),
    ("escapechar", Op::EscapeChar),
    ("globalknownhostsfile", Op::GlobalKnownHostsFile),
    ("userknownhostsfile", Op::UserKnownHostsFile),
    ("connectionattempts", Op::ConnectionAttempts),
    ("batchmode", Op::BatchMode),
    ("checkhostip", Op::CheckHostIP),
    ("stricthostkeychecking", Op::StrictHostKeyChecking),
    ("compression", Op::Compression),
    ("tcpkeepalive", Op::TcpKeepAlive),
    ("keepalive", Op::TcpKeepAlive),
    ("numberofpasswordprompts", Op::NumberOfPasswordPrompts),
    ("syslogfacility", Op::LogFacility),
    ("loglevel", Op::LogLevel),
    ("logverbose", Op::LogVerbose),
    ("dynamicforward", Op::DynamicForward),
    ("preferredauthentications", Op::PreferredAuthentications),
    ("hostkeyalgorithms", Op::HostKeyAlgorithms),
    ("casignaturealgorithms", Op::CaSignatureAlgorithms),
    ("bindaddress", Op::BindAddress),
    ("bindinterface", Op::BindInterface),
    ("clearallforwardings", Op::ClearAllForwardings),
    ("enablesshkeysign", Op::EnableSshKeysign),
    ("verifyhostkeydns", Op::VerifyHostKeyDns),
    (
        "nohostauthenticationforlocalhost",
        Op::NoHostAuthenticationForLocalhost,
    ),
    ("rekeylimit", Op::RekeyLimit),
    ("connecttimeout", Op::ConnectTimeout),
    ("addressfamily", Op::AddressFamily),
    ("serveraliveinterval", Op::ServerAliveInterval),
    ("serveralivecountmax", Op::ServerAliveCountMax),
    ("sendenv", Op::SendEnv),
    ("setenv", Op::SetEnv),
    ("controlpath", Op::ControlPath),
    ("controlmaster", Op::ControlMaster),
    ("controlpersist", Op::ControlPersist),
    ("hashknownhosts", Op::HashKnownHosts),
    ("include", Op::Include),
    ("tunnel", Op::Tunnel),
    ("tunneldevice", Op::TunnelDevice),
    ("localcommand", Op::LocalCommand),
    ("permitlocalcommand", Op::PermitLocalCommand),
    ("remotecommand", Op::RemoteCommand),
    ("visualhostkey", Op::VisualHostKey),
    ("kexalgorithms", Op::KexAlgorithms),
    ("ipqos", Op::IpQos),
    ("requesttty", Op::RequestTty),
    ("sessiontype", Op::SessionType),
    ("stdinnull", Op::StdinNull),
    ("forkafterauthentication", Op::ForkAfterAuthentication),
    ("proxyusefdpass", Op::ProxyUseFdpass),
    ("canonicaldomains", Op::CanonicalDomains),
    ("canonicalizefallbacklocal", Op::CanonicalizeFallbackLocal),
    ("canonicalizehostname", Op::CanonicalizeHostname),
    ("canonicalizemaxdots", Op::CanonicalizeMaxDots),
    (
        "canonicalizepermittedcnames",
        Op::CanonicalizePermittedCnames,
    ),
    ("streamlocalbindmask", Op::StreamLocalBindMask),
    ("streamlocalbindunlink", Op::StreamLocalBindUnlink),
    ("revokedhostkeys", Op::RevokedHostKeys),
    ("fingerprinthash", Op::FingerprintHash),
    ("updatehostkeys", Op::UpdateHostkeys),
    (
        "hostbasedacceptedalgorithms",
        Op::HostbasedAcceptedAlgorithms,
    ),
    ("hostbasedkeytypes", Op::HostbasedAcceptedAlgorithms),
    ("pubkeyacceptedalgorithms", Op::PubkeyAcceptedAlgorithms),
    ("pubkeyacceptedkeytypes", Op::PubkeyAcceptedAlgorithms),
    ("ignoreunknown", Op::IgnoreUnknown),
    ("proxyjump", Op::ProxyJump),
    ("securitykeyprovider", Op::SecurityKeyProvider),
    ("knownhostscommand", Op::KnownHostsCommand),
    ("requiredrsasize", Op::RequiredRsaSize),
    ("enableescapecommandline", Op::EnableEscapeCommandline),
    ("obscurekeystroketiming", Op::ObscureKeystrokeTiming),
    ("channeltimeout", Op::ChannelTimeout),
    ("versionaddendum", Op::VersionAddendum),
    ("refuseconnection", Op::RefuseConnection),
    ("warnweakcrypto", Op::WarnWeakCrypto),
];

/// The result of `parse_token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lookup {
    Known(Op),
    /// `oIgnoredUnknownOption`: unknown, but matched by `IgnoreUnknown`.
    IgnoredUnknown,
    /// `oBadOption`.
    Unknown,
}

/// `parse_token`: look up a lower-cased keyword.
pub(crate) fn parse_token(keyword: &str, ignored_unknown: Option<&str>) -> Lookup {
    if let Some((_, op)) = KEYWORDS.iter().find(|(name, _)| *name == keyword) {
        return Lookup::Known(*op);
    }
    if let Some(patterns) = ignored_unknown
        && crate::matching::match_pattern_list(keyword, patterns, true)
            == crate::matching::ListMatch::Positive
    {
        return Lookup::IgnoredUnknown;
    }
    Lookup::Unknown
}

/// `lookup_opcode_name`: the first keyword of an opcode (the name `ssh -G` prints).
pub fn keyword_name(op: Op) -> &'static str {
    KEYWORDS
        .iter()
        .find(|(_, o)| *o == op)
        .map(|(name, _)| *name)
        .unwrap_or("UNKNOWN")
}

/// How this crate treats a known keyword (docs/design/ssh-backends.md "지시어 분류").
/// Some keywords depend on their value; [`Treatment::Supported`] keywords with a value rule
/// are judged by the resolver after the first value wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Treatment {
    /// "지원": applied with the OpenSSH meaning (some values of some keywords are refused or
    /// shown, see the resolver).
    Supported,
    /// "보안 관련 미구현": refused when applied.
    SecurityRefused,
    /// "읽었지만 미지원": read, not applied, shown (some only for some values).
    ReadNotSupported,
    /// "OpenSSH가 버린 지시어": `oDeprecated`, `oUnsupported` or `oIgnore`.
    Dropped,
}

/// The treatment of an opcode.
pub fn treatment(op: Op) -> Treatment {
    use Op::*;
    match op {
        Host
        | Match
        | Include
        | Tag
        | IgnoreUnknown
        | Hostname
        | User
        | Port
        | HostKeyAlias
        | IdentityFile
        | IdentitiesOnly
        | CertificateFile
        | IdentityAgent
        | PreferredAuthentications
        | PasswordAuthentication
        | KbdInteractiveAuthentication
        | KbdInteractiveDevices
        | NumberOfPasswordPrompts
        | BatchMode
        | PubkeyAuthentication
        | StrictHostKeyChecking
        | UserKnownHostsFile
        | GlobalKnownHostsFile
        | CheckHostIP
        | HashKnownHosts
        | RevokedHostKeys
        | RequiredRsaSize
        | HostKeyAlgorithms
        | KexAlgorithms
        | Ciphers
        | Macs
        | PubkeyAcceptedAlgorithms
        | CaSignatureAlgorithms
        | RekeyLimit
        | ConnectTimeout
        | ConnectionAttempts
        | ServerAliveInterval
        | ServerAliveCountMax
        | TcpKeepAlive
        | AddressFamily
        | BindAddress
        | ProxyJump
        | ProxyCommand
        | SendEnv
        | SetEnv
        | RemoteCommand
        | CanonicalizeHostname
        | CanonicalDomains
        | CanonicalizeFallbackLocal
        | CanonicalizeMaxDots
        | CanonicalizePermittedCnames
        | NoHostAuthenticationForLocalhost
        | VerifyHostKeyDns
        | SessionType
        | ChannelTimeout
        | RefuseConnection => Treatment::Supported,
        KnownHostsCommand | BindInterface => Treatment::SecurityRefused,
        LocalForward
        | RemoteForward
        | DynamicForward
        | ExitOnForwardFailure
        | GatewayPorts
        | ClearAllForwardings
        | PermitRemoteOpen
        | ControlMaster
        | ControlPath
        | ControlPersist
        | ForwardAgent
        | AddKeysToAgent
        | ForwardX11
        | ForwardX11Trusted
        | ForwardX11Timeout
        | XAuthLocation
        | Tunnel
        | TunnelDevice
        | StreamLocalBindMask
        | StreamLocalBindUnlink
        | RequestTty
        | StdinNull
        | ForkAfterAuthentication
        | EscapeChar
        | EnableEscapeCommandline
        | ObscureKeystrokeTiming
        | PermitLocalCommand
        | LocalCommand
        | GssAuthentication
        | GssDelegateCreds
        | HostbasedAuthentication
        | Pkcs11Provider
        | SecurityKeyProvider
        | ProxyUseFdpass
        | Compression
        | HostbasedAcceptedAlgorithms
        | EnableSshKeysign
        | UpdateHostkeys
        | LogLevel
        | LogVerbose
        | LogFacility
        | VisualHostKey
        | FingerprintHash
        | WarnWeakCrypto
        | VersionAddendum
        | IpQos => Treatment::ReadNotSupported,
        Ignore | Deprecated | Unsupported => Treatment::Dropped,
    }
}

/// Every keyword name this crate knows with its opcode and treatment, in the order of
/// `keywords[]`.
pub fn known_keywords() -> impl Iterator<Item = (&'static str, Op, Treatment)> {
    KEYWORDS
        .iter()
        .map(|(name, op)| (*name, *op, treatment(*op)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_lower_case() {
        let mut names: Vec<&str> = KEYWORDS.iter().map(|(n, _)| *n).collect();
        assert!(
            names
                .iter()
                .all(|n| n.bytes().all(|c| !c.is_ascii_uppercase()))
        );
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), KEYWORDS.len());
    }

    #[test]
    fn lookup() {
        assert_eq!(parse_token("host", None), Lookup::Known(Op::Host));
        assert_eq!(parse_token("nosuch", None), Lookup::Unknown);
        assert_eq!(parse_token("nosuch", Some("No*")), Lookup::IgnoredUnknown);
        assert_eq!(parse_token("nosuch", Some("other")), Lookup::Unknown);
        assert_eq!(
            keyword_name(Op::KbdInteractiveAuthentication),
            "kbdinteractiveauthentication"
        );
        assert_eq!(keyword_name(Op::Pkcs11Provider), "pkcs11provider");
        assert_eq!(keyword_name(Op::TcpKeepAlive), "tcpkeepalive");
    }
}
