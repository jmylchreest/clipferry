//! Self-applied sandboxing (§8.1/§8.2): the guarantee travels with the
//! binary, not just the systemd unit.
//!
//! Lock sequence (order matters — Landlock does not affect already-open
//! fds): connections are established first (§6 retry loop included), then
//! the ruleset is enforced. One deliberate divergence from the original
//! design draft: per-transfer X11 connections (§4.4) re-read the Xauthority
//! file after the lock, so that one file gets a read-only exception instead
//! of the draft's zero-rules ruleset. Nothing else is reachable, and no
//! write access exists anywhere.

use anyhow::Context as _;
use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible as _, Ruleset, RulesetAttr as _,
    RulesetCreatedAttr as _, RulesetStatus, path_beneath_rules,
};
use log::{info, warn};

/// No core dumps, no ptrace from unprivileged peers (§8.1). Applied at
/// startup, before anything sensitive can be in memory.
pub fn disable_dumps() {
    if let Err(e) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        warn!("PR_SET_DUMPABLE=0 failed: {e}");
    }
}

fn xauthority_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Ok(xauth) = std::env::var("XAUTHORITY") {
        paths.push(xauth.into());
    }
    if let Ok(home) = std::env::var("HOME") {
        paths.push(std::path::Path::new(&home).join(".Xauthority"));
    }
    paths.retain(|p| p.exists());
    paths
}

/// Apply the Landlock ruleset.
///
/// All filesystem access denied except read-only Xauthority (for post-lock
/// per-transfer X11 connections); all TCP denied (ABI v4). Unix sockets
/// are unaffected — exactly right for us.
pub fn apply() -> anyhow::Result<RulesetStatus> {
    let abi = ABI::V4; // fs + tcp
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .context("handle filesystem access")?
        .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)
        .context("handle TCP access")?
        .create()
        .context("create Landlock ruleset")?
        .add_rules(path_beneath_rules(xauthority_paths(), AccessFs::ReadFile))
        .context("allow Xauthority read")?
        .restrict_self()
        .context("enforce Landlock ruleset")?;
    Ok(status.ruleset)
}

pub fn apply_and_log(disabled: bool) {
    if disabled {
        warn!("landlock: DISABLED by --no-landlock — running without the in-process sandbox");
        return;
    }
    match apply() {
        Ok(RulesetStatus::FullyEnforced) => info!("landlock: enforced (fs+tcp)"),
        Ok(RulesetStatus::PartiallyEnforced) => {
            info!(
                "landlock: partially enforced (older kernel ABI); systemd hardening still applies"
            );
        }
        Ok(RulesetStatus::NotEnforced) => {
            info!("landlock: unavailable, relying on systemd hardening");
        }
        Err(e) => warn!("landlock: failed to apply ({e:#}); relying on systemd hardening"),
    }
}

/// Hidden `--sandbox-selftest` (§8.2): lock, then prove the locks hold.
/// Exit code is the test result; runs without any display connection.
#[allow(clippy::print_stdout)] // diagnostic tool speaks on stdout
pub fn selftest() -> std::process::ExitCode {
    disable_dumps();
    let status = match apply() {
        Ok(status) => status,
        Err(e) => {
            println!("sandbox-selftest: FAILED to apply landlock: {e:#}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if status == RulesetStatus::NotEnforced {
        println!("sandbox-selftest: landlock not supported by this kernel — nothing to assert");
        return std::process::ExitCode::SUCCESS;
    }

    let fs_denied = std::fs::File::open("/etc/passwd").is_err();
    let tcp_denied = "127.0.0.1:80".parse().is_ok_and(|addr| {
        matches!(
            std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)),
            Err(ref e) if e.kind() == std::io::ErrorKind::PermissionDenied
        )
    });
    let not_dumpable = matches!(
        rustix::process::dumpable_behavior(),
        Ok(rustix::process::DumpableBehavior::NotDumpable)
    );

    println!(
        "sandbox-selftest: fs_denied={fs_denied} tcp_denied={tcp_denied} not_dumpable={not_dumpable}"
    );
    // TCP assertion only holds from ABI v4 (kernel ≥ 6.7); a partial
    // enforcement without net support still passes on fs + dumpable.
    let tcp_ok = tcp_denied || status == RulesetStatus::PartiallyEnforced;
    if fs_denied && tcp_ok && not_dumpable {
        println!("sandbox-selftest: OK");
        std::process::ExitCode::SUCCESS
    } else {
        println!("sandbox-selftest: FAILED");
        std::process::ExitCode::FAILURE
    }
}
