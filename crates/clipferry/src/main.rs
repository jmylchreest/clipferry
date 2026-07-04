use std::os::fd::{AsFd, BorrowedFd};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use calloop_wayland_source::WaylandSource;
use log::{error, info};
use wayland_client::Connection;
use wayland_client::globals::registry_queue_init;
use x11rb::rust_connection::RustConnection;

use clipferry::app::App;
use clipferry::{cli, logging, wayland, x11};

/// calloop needs an owner for the X11 socket fd; borrow it from the
/// connection for the lifetime of the loop.
struct X11Fd(Arc<RustConnection>);

impl AsFd for X11Fd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.stream().as_fd()
    }
}

fn main() -> ExitCode {
    let options = match cli::parse() {
        Ok(cli::Parsed::Run(options)) => options,
        Ok(cli::Parsed::Exit) => return ExitCode::SUCCESS,
        Err(e) => {
            // The logger isn't up yet during argument parsing.
            #[allow(clippy::print_stderr)]
            {
                eprintln!("clipferry: {e}");
            }
            return ExitCode::FAILURE;
        }
    };
    logging::init(options.log_level);
    if options.sandbox_selftest {
        return clipferry::sandbox::selftest();
    }
    clipferry::sandbox::disable_dumps();
    match run(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::print_stdout)] // --oneshot-check is a diagnostic that speaks on stdout
fn run(options: &cli::Options) -> anyhow::Result<()> {
    // Connection order per §8.2: both displays first (Landlock lands in M5
    // and must come after Xauthority has been read).
    let wl_conn = Connection::connect_to_env().context("connect to Wayland ($WAYLAND_DISPLAY)")?;
    let (globals, mut event_queue) =
        registry_queue_init::<App>(&wl_conn).context("initialize Wayland registry")?;
    let qh = event_queue.handle();
    let manager = wayland::bind_manager(&globals, &qh)?;
    let seat = wayland::bind_seat(&globals, &qh)?;
    info!("wayland: {} bound", manager.protocol_name());

    if options.primary && !manager.supports_primary() {
        anyhow::bail!(
            "--primary needs ext-data-control-v1 or zwlr-data-control v2; \
             this compositor offers neither"
        );
    }
    let (x_conn, screen_num) = x11::connect_with_retry();
    let x = x11::X11::new(x_conn, screen_num, options.primary).context("initialize X11 backend")?;
    info!(
        "x11: connected (vendor {:?}, XFIXES {}.{})",
        x.vendor(),
        x.xfixes_version.0,
        x.xfixes_version.1
    );

    if options.oneshot_check {
        println!("clipferry {} — oneshot check", clipferry::VERSION);
        println!(
            "  wayland : {} (via $WAYLAND_DISPLAY)",
            manager.protocol_name()
        );
        println!(
            "  x11     : vendor {:?}, XFIXES {}.{} (via $DISPLAY)",
            x.vendor(),
            x.xfixes_version.0,
            x.xfixes_version.1
        );
        println!("result: both sides reachable; bridging supported");
        return Ok(());
    }

    // §8.2 lock sequence: both connections exist (Xauthority has been read
    // for the main connection; transfer connections keep read access to it).
    clipferry::sandbox::apply_and_log(options.no_landlock);

    let device = manager.get_data_device(&seat, &qh);
    let mut app = App::new(x, wl_conn.clone(), manager, device, qh, options);

    // Startup rule (§4.1): the roundtrip delivers the current Wayland
    // selection (if any); the probe fills the Wayland side if only X11 has
    // an owner. Both sides owned → touch nothing.
    event_queue
        .roundtrip(&mut app)
        .context("initial Wayland roundtrip")?;
    app.probe_x11_startup();

    let mut event_loop = EventLoop::<App>::try_new().context("create event loop")?;
    app.loop_signal = Some(event_loop.get_signal());
    app.loop_handle = Some(event_loop.handle());

    // Eager snapshots (§4.2.1) land back on the loop through this channel.
    let (snapshot_tx, snapshot_rx) = calloop::channel::channel();
    app.snapshot_tx = Some(snapshot_tx);
    event_loop
        .handle()
        .insert_source(snapshot_rx, |msg, (), app: &mut App| {
            if let calloop::channel::Event::Msg(msg) = msg {
                app.on_snapshot(msg);
            }
        })
        .map_err(|e| anyhow!("insert snapshot channel: {e}"))?;

    WaylandSource::new(wl_conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow!("insert Wayland source: {e}"))?;
    event_loop
        .handle()
        .insert_source(
            Generic::new(X11Fd(app.x11.conn.clone()), Interest::READ, Mode::Level),
            |_, _, app: &mut App| {
                app.drain_x11().map_err(std::io::Error::other)?;
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow!("insert X11 source: {e}"))?;

    info!(
        "bridging CLIPBOARD{} (all MIME types, bidirectional, {} mode)",
        if options.primary { " + PRIMARY" } else { "" },
        match options.sync_mode {
            cli::SyncMode::Lazy => "lazy",
            cli::SyncMode::Eager => "eager",
        }
    );
    event_loop
        .run(None, &mut app, |_| {})
        .context("event loop")?;

    app.exit.take().map_or(Ok(()), Err)
}
