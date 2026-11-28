//! The effective target of an `ssh` child, read with `ssh -G` and the child's own arguments
//! (docs/design/ssh-backends.md "포트 발견과 터널의 대상 일치", director 2937).

use std::process::Command;

use crate::backend::{EffectiveTarget, SshBackendError};
use crate::discovery::Limits;
use crate::system::discovery::run_capture_with_budget;
use crate::{SshLauncher, SshTarget};

/// Runs `cmd` (an `ssh -G` from [`SshLauncher::resolve_command`]) through the launcher's spawn
/// hook within `discovery.step_timeout` and reads `hostname`, `port`, `user` and `proxyjump`
/// from what it prints.
pub(crate) fn effective_target(
    launcher: &SshLauncher,
    target: &SshTarget,
    cmd: Command,
    purpose: &'static str,
) -> Result<EffectiveTarget, SshBackendError> {
    let settings = launcher.settings();
    let limits = Limits::new(settings, target);
    let what = format!("ssh -G ({purpose})");
    let out = run_capture_with_budget(
        cmd,
        settings.step_timeout(),
        &what,
        launcher.spawn_hook(),
        limits,
    )
    .map_err(|e| SshBackendError::TargetUnresolved {
        purpose,
        detail: e.to_string(),
    })?;
    parse_dump(&out).ok_or_else(|| SshBackendError::TargetUnresolved {
        purpose,
        detail: "ssh -G printed no hostname, port and user".to_owned(),
    })
}

/// The target fields of `ssh -G` output (`dump_client_config`: one `keyword value` line per
/// option, keywords in lower case). `proxyjump` is absent when there is no jump host.
pub(crate) fn parse_dump(out: &str) -> Option<EffectiveTarget> {
    let mut hostname = None;
    let mut port = None;
    let mut user = None;
    let mut jump = String::new();
    for line in out.lines() {
        let Some((keyword, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match keyword {
            "hostname" => hostname = Some(value.to_owned()),
            "port" => port = value.parse().ok(),
            "user" => user = Some(value.to_owned()),
            "proxyjump" => jump = value.to_owned(),
            _ => {}
        }
    }
    Some(EffectiveTarget {
        hostname: hostname?,
        port: port?,
        user: user?,
        jump,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dump_gives_host_port_user_and_jump() {
        let out = "host box\nuser me\nhostname 10.0.0.5\nport 2222\nproxyjump bastion,u@hop:22\n\
                   addressfamily any\n";
        assert_eq!(
            parse_dump(out),
            Some(EffectiveTarget {
                hostname: "10.0.0.5".into(),
                port: 2222,
                user: "me".into(),
                jump: "bastion,u@hop:22".into(),
            })
        );
        let no_jump = parse_dump("hostname h\nport 22\nuser u\n").expect("dump");
        assert_eq!(no_jump.jump, "");
        assert_eq!(parse_dump("hostname h\nuser u\n"), None, "no port");
        assert_eq!(parse_dump("port forty\nhostname h\nuser u\n"), None);
    }
}
