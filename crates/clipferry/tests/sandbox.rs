//! §12: sandbox selftest — run the binary's hidden mode and trust its
//! asserts (fs deny, TCP deny, dumpable=0). Skips itself gracefully on
//! kernels without Landlock.

#[test]
fn sandbox_selftest_passes() {
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_clipferry"))
        .arg("--sandbox-selftest")
        .status()
        .expect("run clipferry --sandbox-selftest");
    assert!(status.success(), "sandbox selftest failed: {status}");
}
