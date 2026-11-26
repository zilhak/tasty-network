//! Roles of a link (docs/design/transport.md "역할").
//!
//! Four roles are told apart: the side that launched SSH, the side that opened the app
//! channel (the dialer), the side that provides the workspace, and the holder (the side that
//! occupies the workspace and shows it as a mirror). The handshake carries only the holder;
//! the rest follows from it and the transport.

use crate::transport::TransportKind;

/// One end of a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// The side that opened the channel.
    Dialer,
    /// The side that accepted the channel.
    Acceptor,
}

impl Side {
    /// The other end.
    pub fn opposite(self) -> Self {
        match self {
            Self::Dialer => Self::Acceptor,
            Self::Acceptor => Self::Dialer,
        }
    }
}

/// The handshake field `holder`: which end of an attach link holds the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Holder {
    /// The dialer holds: the forward direction (SSH forward, a P2P dialer that attaches).
    Dialer,
    /// The acceptor holds: the role reversal (a proposal the acceptor approves or refuses).
    Acceptor,
}

impl Holder {
    /// The wire form, `dialer` or `acceptor`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dialer => "dialer",
            Self::Acceptor => "acceptor",
        }
    }

    /// The value for a wire form.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "dialer" => Some(Self::Dialer),
            "acceptor" => Some(Self::Acceptor),
            _ => None,
        }
    }

    /// The end that holds.
    pub fn side(self) -> Side {
        match self {
            Self::Dialer => Side::Dialer,
            Self::Acceptor => Side::Acceptor,
        }
    }
}

/// Every role of one attach link, from the holder and the transport (the table in
/// docs/design/transport.md "역할").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Roles {
    /// The end that launched `ssh`. `None` for P2P and memory channels.
    pub ssh_launcher: Option<Side>,
    /// The end that opened the app channel. Always [`Side::Dialer`].
    pub dialer: Side,
    /// The end that provides the workspace. It sends the first Control frame (the descriptor)
    /// after the ACK.
    pub provider: Side,
    /// The end that holds the workspace.
    pub holder: Side,
    /// The end that connects again after a drop. Always the dialer: the receiving end of an SSH
    /// tunnel cannot open the tunnel, and a P2P acceptor opens a connection of its own.
    pub redialer: Side,
}

/// The roles of an attach link over `transport` with `holder`.
///
/// SSH is the forward case with [`Holder::Dialer`] and the role reversal (b) with
/// [`Holder::Acceptor`]; in both the dialer launched `ssh`. [`TransportKind::Tcp`] is the
/// receiving end of an SSH tunnel and gets the same table.
pub fn roles(transport: TransportKind, holder: Holder) -> Roles {
    let ssh_launcher = match transport {
        TransportKind::Ssh | TransportKind::Tcp => Some(Side::Dialer),
        TransportKind::P2p | TransportKind::Memory => None,
    };
    Roles {
        ssh_launcher,
        dialer: Side::Dialer,
        provider: holder.side().opposite(),
        holder: holder.side(),
        redialer: Side::Dialer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_ssh_has_the_dialer_launch_ssh_hold_and_redial() {
        let r = roles(TransportKind::Ssh, Holder::Dialer);
        assert_eq!(r.ssh_launcher, Some(Side::Dialer));
        assert_eq!(r.dialer, Side::Dialer);
        assert_eq!(r.holder, Side::Dialer);
        assert_eq!(r.provider, Side::Acceptor);
        assert_eq!(r.redialer, Side::Dialer);
        // The receiving end of the tunnel sees the same table.
        assert_eq!(roles(TransportKind::Tcp, Holder::Dialer), r);
    }

    #[test]
    fn role_reversal_moves_only_holder_and_provider() {
        let r = roles(TransportKind::Ssh, Holder::Acceptor);
        assert_eq!(r.ssh_launcher, Some(Side::Dialer));
        assert_eq!(r.holder, Side::Acceptor);
        assert_eq!(r.provider, Side::Dialer);
        assert_eq!(r.redialer, Side::Dialer);
    }

    #[test]
    fn p2p_has_no_ssh_launcher() {
        let r = roles(TransportKind::P2p, Holder::Acceptor);
        assert_eq!(r.ssh_launcher, None);
        assert_eq!(r.holder, Side::Acceptor);
    }

    #[test]
    fn holder_wire_form_round_trips() {
        for h in [Holder::Dialer, Holder::Acceptor] {
            assert_eq!(Holder::parse(h.as_str()), Some(h));
        }
        assert_eq!(Holder::parse("both"), None);
    }
}
