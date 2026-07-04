//! CLI parsing (§9). Every flag is safe to omit; a bare `clipferry` is the
//! recommended setup. lexopt, not clap — binary-size budget (§11).

use anyhow::bail;
use log::LevelFilter;

pub struct Options {
    pub oneshot_check: bool,
    pub log_level: LevelFilter,
    /// §4.2 idle timeout in seconds; 0 (default) = no timeout.
    pub transfer_timeout: u64,
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
      --oneshot-check          Connect to both displays, print a diagnostic, exit
      --log-level LEVEL        error|warn|info|debug|trace  [default: info]
      --transfer-timeout SECS  Idle timeout for payload transfers; 0 = none  [default: 0]
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
    };
    while let Some(arg) = parser.next()? {
        match arg {
            Long("oneshot-check") => options.oneshot_check = true,
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
    }

    #[test]
    fn bad_level_and_unknown_flag_error() {
        assert!(run(&["--log-level", "loud"]).is_err());
        assert!(run(&["--frobnicate"]).is_err());
    }
}
