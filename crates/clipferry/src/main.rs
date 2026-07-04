#![allow(clippy::print_stderr)] // placeholder until the real logger lands (M1)

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!(
        "{} {}: not implemented yet — see DESIGN.md for the roadmap",
        clipferry::NAME,
        clipferry::VERSION
    );
    ExitCode::FAILURE
}
