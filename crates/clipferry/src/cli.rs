//! CLI parsing (§9). Every flag is safe to omit; a bare `clipferry` is the
//! recommended setup. lexopt, not clap — binary-size budget (§11).

use anyhow::bail;
use log::LevelFilter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    #[default]
    Lazy,
    Eager,
}

#[allow(clippy::struct_excessive_bools)] // CLI flags are legitimately flags
pub struct Options {
    pub oneshot_check: bool,
    pub log_level: LevelFilter,
    /// §4.2 idle timeout in seconds; 0 (default) = no timeout.
    pub transfer_timeout: u64,
    /// §4.2.1 transfer strategy.
    pub sync_mode: SyncMode,
    /// Per-type eager snapshot cap in bytes; `None` = unlimited.
    pub eager_max_size: Option<usize>,
    /// Also bridge the PRIMARY selection (§3 non-goal by default).
    pub primary: bool,
    /// Do not bridge offers carrying the KDE password-manager hint (§8).
    pub skip_sensitive: bool,
    /// Debugging escape hatch (§8.2); logged loudly at WARN.
    pub no_landlock: bool,
    /// Hidden: apply the sandbox, assert it holds, exit (§8.2).
    pub sandbox_selftest: bool,
}

/// Parse a human size: plain bytes, `K`/`M`/`G` (binary) suffixes, or the
/// literals `0`/`unlimited` for "no cap" (§4.2.1).
pub fn parse_size(text: &str) -> anyhow::Result<Option<usize>> {
    let t = text.trim();
    if t.eq_ignore_ascii_case("unlimited") || t == "0" {
        return Ok(None);
    }
    let (digits, mult): (&str, usize) = match t.as_bytes().last() {
        Some(b'K' | b'k') => (&t[..t.len() - 1], 1 << 10),
        Some(b'M' | b'm') => (&t[..t.len() - 1], 1 << 20),
        Some(b'G' | b'g') => (&t[..t.len() - 1], 1 << 30),
        _ => (t, 1),
    };
    let value: usize = digits.parse().map_err(|_| {
        anyhow::anyhow!("invalid size {text:?} (expected e.g. 10M, 512K, 1G, unlimited)")
    })?;
    value
        .checked_mul(mult)
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("size {text:?} overflows"))
}

pub enum Parsed {
    Run(Options),
    /// --help / --version already printed; exit success.
    Exit,
}

const HELP: &str = "\
clipferry — X11 <-> Wayland clipboard bridge

Usage: clipferry [OPTIONS]

Options:
      --sync-mode MODE         lazy|eager  [default: lazy]
      --eager-max-size SIZE    Per-type eager snapshot cap (10M, 512K, 1G,
                               0/unlimited = no cap)  [default: 10M]
      --primary                Also bridge the PRIMARY selection
      --skip-sensitive         Do not bridge password-manager-hinted offers
      --transfer-timeout SECS  Idle timeout for payload transfers; 0 = none  [default: 0]
      --no-landlock            Disable the in-process Landlock sandbox
      --oneshot-check          Connect to both displays, print a diagnostic, exit
      --log-level LEVEL        error|warn|info|debug|trace  [default: info]
  -V, --version            Print version
  -h, --help               Print this help
";

pub fn parse() -> anyhow::Result<Parsed> {
    parse_from(lexopt::Parser::from_env())
}

#[allow(clippy::print_stdout)] // --help/--version speak on stdout by convention
fn parse_from(mut parser: lexopt::Parser) -> anyhow::Result<Parsed> {
    use lexopt::prelude::*;

    let mut options = Options {
        oneshot_check: false,
        log_level: LevelFilter::Info,
        transfer_timeout: 0,
        sync_mode: SyncMode::Lazy,
        eager_max_size: Some(10 << 20),
        primary: false,
        skip_sensitive: false,
        no_landlock: false,
        sandbox_selftest: false,
    };
    while let Some(arg) = parser.next()? {
        match arg {
            Long("oneshot-check") => options.oneshot_check = true,
            Long("no-landlock") => options.no_landlock = true,
            // Hidden diagnostic (§8.2); deliberately not in --help.
            Long("sandbox-selftest") => options.sandbox_selftest = true,
            Long("primary") => options.primary = true,
            Long("skip-sensitive") => options.skip_sensitive = true,
            Long("sync-mode") => {
                let value = parser.value()?;
                options.sync_mode = match value.to_str() {
                    Some("lazy") => SyncMode::Lazy,
                    Some("eager") => SyncMode::Eager,
                    _ => bail!(
                        "--sync-mode: expected lazy|eager, got {}",
                        value.to_string_lossy()
                    ),
                };
            }
            Long("eager-max-size") => {
                let value = parser.value()?;
                let Some(text) = value.to_str() else {
                    bail!("--eager-max-size: invalid UTF-8");
                };
                options.eager_max_size = parse_size(text)?;
            }
            Long("transfer-timeout") => {
                options.transfer_timeout = parser.value()?.parse()?;
            }
            Long("log-level") => {
                let value = parser.value()?;
                let Some(text) = value.to_str() else {
                    bail!("--log-level: invalid UTF-8");
                };
                match text.parse::<LevelFilter>() {
                    Ok(level) => options.log_level = level,
                    Err(_) => {
                        bail!("--log-level: expected error|warn|info|debug|trace, got {text:?}")
                    }
                }
            }
            Short('V') | Long("version") => {
                println!("{} {}", crate::NAME, crate::VERSION);
                return Ok(Parsed::Exit);
            }
            Short('h') | Long("help") => {
                print!("{HELP}");
                return Ok(Parsed::Exit);
            }
            _ => return Err(arg.unexpected().into()),
        }
    }
    Ok(Parsed::Run(options))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn run(args: &[&str]) -> anyhow::Result<Parsed> {
        parse_from(lexopt::Parser::from_iter(
            std::iter::once("clipferry").chain(args.iter().copied()),
        ))
    }

    #[test]
    fn defaults() {
        let Parsed::Run(o) = run(&[]).unwrap() else {
            panic!("expected Run")
        };
        assert!(!o.oneshot_check);
        assert_eq!(o.log_level, LevelFilter::Info);
    }

    #[test]
    fn flags_parse() {
        let Parsed::Run(o) = run(&[
            "--oneshot-check",
            "--log-level",
            "debug",
            "--transfer-timeout",
            "30",
        ])
        .unwrap() else {
            panic!("expected Run")
        };
        assert!(o.oneshot_check);
        assert_eq!(o.log_level, LevelFilter::Debug);
        assert_eq!(o.transfer_timeout, 30);
    }

    #[test]
    fn timeout_defaults_to_infinite() {
        let Parsed::Run(o) = run(&[]).unwrap() else {
            panic!("expected Run")
        };
        assert_eq!(o.transfer_timeout, 0);
        assert_eq!(o.sync_mode, SyncMode::Lazy);
        assert_eq!(o.eager_max_size, Some(10 << 20));
        assert!(!o.primary);
        assert!(!o.skip_sensitive);
    }

    #[test]
    fn m4_flags_parse() {
        let Parsed::Run(o) = run(&[
            "--sync-mode",
            "eager",
            "--eager-max-size",
            "512K",
            "--primary",
            "--skip-sensitive",
        ])
        .unwrap() else {
            panic!("expected Run")
        };
        assert_eq!(o.sync_mode, SyncMode::Eager);
        assert_eq!(o.eager_max_size, Some(512 << 10));
        assert!(o.primary);
        assert!(o.skip_sensitive);
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("10M").unwrap(), Some(10 << 20));
        assert_eq!(parse_size("512k").unwrap(), Some(512 << 10));
        assert_eq!(parse_size("1G").unwrap(), Some(1 << 30));
        assert_eq!(parse_size("12345").unwrap(), Some(12345));
        assert_eq!(parse_size("0").unwrap(), None);
        assert_eq!(parse_size("unlimited").unwrap(), None);
        assert!(parse_size("ten").is_err());
        assert!(parse_size("10T").is_err());
    }

    #[test]
    fn bad_level_and_unknown_flag_error() {
        assert!(run(&["--log-level", "loud"]).is_err());
        assert!(run(&["--frobnicate"]).is_err());
    }
}
