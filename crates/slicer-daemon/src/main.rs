mod watch;

#[cfg(test)]
mod surface_tests;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "protype-slicer-daemon",
    about = "Protype Slicer headless daemon and CLI",
    version = slicer_rpc::build_info::VERSION_LINE
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Slice an STL/3MF file to G-code. Needs a build with
    /// `with-libslic3r`; without it the bridge emits rectangles.
    Slice {
        /// Mesh to place on the plate. Repeat for a multi-object plate.
        /// Append `@N` to assign a tool, 1-based as libslic3r counts them
        /// (`part.stl@1` is the first tool, `@2` the second) — that is what
        /// turns the plate into a multi-material print. Without `@N` the
        /// object keeps the preset's tool.
        #[arg(long = "input", required = true, value_name = "PATH[@EXTRUDER]")]
        inputs: Vec<String>,
        #[arg(long)]
        printer_preset: String,
        #[arg(long)]
        process_preset: String,
        #[arg(long)]
        filament_preset: String,
        #[arg(long)]
        output: String,
        /// Optional JSON file matching slicer_ffi::IdexInputs.
        #[arg(long)]
        idex_bundle: Option<String>,
        /// Optional JSON file with raw libslic3r config overrides.
        /// Format: `{"key1": "value1", "key2": "value2"}`. Strings get
        /// fed to `DynamicPrintConfig::set_deserialize` on the C++ side,
        /// so use libslic3r's text serialization (e.g. arrays as
        /// `"1,2,3"`). Empty file or absent flag = no overrides. A key
        /// libslic3r would ignore — unknown, obsolete, or a value in a
        /// legacy form — fails the slice and is named.
        #[arg(long)]
        overrides: Option<String>,
    },
    /// Run the daemon JSON-RPC server. Prints "READY port=<P> token=<T>" to stdout when ready.
    Serve {
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Save one camera frame, and say whether it is worth looking at.
    Camera {
        #[arg(long, default_value = "192.168.0.218")]
        host: String,
        #[arg(long)]
        output: String,
    },
    /// Build one CSG tree into a STEP file and report its volume as
    /// JSON on stdout.
    ///
    /// Not meant to be typed: this is the child half of the B-rep
    /// isolation, and `Serve` re-runs its own binary with it for every
    /// `geometry.build`. OCCT can dereference a null pointer on a part
    /// that is perfectly legal — measured on a chamfer over a union of a
    /// cylinder and a box — and no `catch` can hold that, so the only
    /// containment is a process boundary. Useful by hand for exactly one
    /// thing: reproducing such a part without taking a daemon with it.
    BuildGeometry {
        /// JSON file holding one `CsgNode`.
        #[arg(long)]
        tree: String,
        /// Where to write the STEP.
        #[arg(long)]
        output: String,
    },
    /// Print JSON Schema for the public types (used by codegen pipeline)
    Schema,
    /// Watch a print and journal it to disk.
    ///
    /// Exits non-zero with a distinct code per outcome so it can be run
    /// as a background watchdog. See `slicer_monitor::Outcome::exit_code`.
    Watch {
        /// Host, with or without the API port (default 7125).
        #[arg(long, default_value = "192.168.0.218")]
        host: String,
        #[arg(long)]
        logdir: String,
        #[arg(long, default_value_t = 20.0)]
        interval_s: f64,
        /// Idle on a layer counter that has moved before this counts as
        /// stuck.
        #[arg(long, default_value_t = slicer_monitor::watch::DEFAULT_STALL_AFTER_S)]
        stall_after_s: f64,
        /// How long the start block may run before a counter still at
        /// zero becomes a finding of its own.
        #[arg(long, default_value_t = slicer_monitor::watch::DEFAULT_START_GRACE_S)]
        start_grace_s: f64,
        /// Stop once this layer is reached.
        #[arg(long)]
        until_layer: Option<u32>,
        /// Take one snapshot, journal it, and exit. Useful for checking
        /// the machine is answering before committing to a long watch.
        #[arg(long, default_value_t = false)]
        once: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Slice {
            inputs,
            printer_preset,
            process_preset,
            filament_preset,
            output,
            idex_bundle,
            overrides,
        } => {
            let idex = if let Some(path) = idex_bundle {
                let blob = std::fs::read_to_string(&path)?;
                Some(serde_json::from_str::<slicer_ffi::IdexInputs>(&blob)?)
            } else {
                None
            };
            let config_overrides = if let Some(path) = overrides {
                let blob = std::fs::read_to_string(&path)?;
                let map: std::collections::BTreeMap<String, String> = serde_json::from_str(&blob)?;
                map.into_iter()
                    .map(|(key, value)| slicer_ffi::ConfigOverride { key, value })
                    .collect()
            } else {
                Vec::new()
            };
            let objects = inputs
                .iter()
                .map(|spec| parse_plate_object(spec))
                .collect::<Result<Vec<_>>>()?;
            let req = slicer_pipeline::SliceRequest {
                objects,
                output_path: output.clone(),
                printer_preset_path: printer_preset,
                process_preset_path: process_preset,
                filament_preset_path: filament_preset,
                idex,
                config_overrides,
            };
            tracing::info!(?inputs, ?output, "slicing");
            match slicer_pipeline::run_slice_blocking(req) {
                Ok(outcome) => {
                    tracing::info!(layers = outcome.total_layers, "slice complete");
                    println!("G-code written to {}", outcome.gcode_path);
                    Ok(())
                }
                Err(e) => {
                    tracing::error!(?e, "slice failed");
                    Err(anyhow::anyhow!("{e}"))
                }
            }
        }
        Command::Serve { port } => {
            // Point `geometry.build` at this same binary before serving
            // anything: from here on a B-rep build happens in a child,
            // and OCCT crashing on a generated part costs that part
            // rather than every client's session. Non-fatal if the path
            // cannot be resolved — building in process is what the
            // daemon did before, and refusing to boot over it would be
            // the worse trade.
            match std::env::current_exe() {
                Ok(exe) => slicer_rpc::brep_worker::use_worker(exe),
                Err(e) => tracing::warn!(
                    ?e,
                    "cannot resolve this executable; B-rep builds stay in process"
                ),
            }
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                // Attach session autosave so a crash leaves a recoverable
                // events.log behind. Failure to set up autosave is non-fatal
                // (we'd rather serve without it than refuse to boot).
                let state = match slicer_state::autosave::AutosaveSink::new() {
                    Ok(sink) => {
                        tracing::info!(
                            project_id = sink.project_id(),
                            dir = %sink.dir().display(),
                            "autosave session attached"
                        );
                        slicer_state::StateLayer::new().with_autosave(sink)
                    }
                    Err(e) => {
                        tracing::warn!(?e, "autosave setup failed; running without it");
                        slicer_state::StateLayer::new()
                    }
                };
                let handle = slicer_rpc::start_server(state.clone(), port).await?;
                println!("READY port={} token={}", handle.port, handle.token.as_str());
                use std::io::Write;
                let _ = std::io::stdout().flush();
                // Wait on BOTH SIGINT (ctrl_c) and SIGTERM. The host's
                // graceful_shutdown_child (Slicer/apps/host-tauri/src/daemon.rs)
                // sends SIGTERM with a 3s grace window before escalating to
                // SIGKILL; without a SIGTERM handler tokio's default would
                // terminate the process immediately, skipping the lockfile
                // release + autosave cleanup below.
                shutdown_signal().await?;
                let _ = handle.shutdown_tx.send(());
                if let Some(path) = state.current_path() {
                    let _ = slicer_state::lockfile::release(&path);
                }
                if let Some(sink) = state.autosave() {
                    sink.cleanup_on_exit();
                }
                Ok::<(), anyhow::Error>(())
            })
        }
        Command::Watch {
            host,
            logdir,
            interval_s,
            stall_after_s,
            start_grace_s,
            until_layer,
            once,
        } => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let code = runtime.block_on(watch::run(watch::Options {
                host,
                logdir,
                interval_s,
                stall_after_s,
                start_grace_s,
                until_layer,
                once,
            }))?;
            std::process::exit(code);
        }
        Command::Camera { host, output } => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let frame = runtime.block_on(async move {
                slicer_monitor::moonraker::Moonraker::new(&host)
                    .camera_frame()
                    .await
            })?;
            std::fs::write(&output, &frame.jpeg)?;
            println!("{} bytes -> {output}", frame.jpeg.len());
            match frame.caveat() {
                // A frame that cannot show anything is not evidence, and
                // saying so is the whole value of the check.
                Some(caveat) => {
                    println!("UNUSABLE: {caveat}");
                    std::process::exit(3);
                }
                None => println!("usable: the chamber light was on"),
            }
            Ok(())
        }
        Command::BuildGeometry { tree, output } => {
            let code = slicer_rpc::brep_worker::run_worker(
                std::path::Path::new(&tree),
                std::path::Path::new(&output),
            );
            std::process::exit(code);
        }
        Command::Schema => {
            // Emit two schemas + the baseline data so codegen produces TS types
            // for both events and settings, and the UI can ship with default
            // values that match what the daemon will return at runtime.
            let combined = serde_json::json!({
                "events": schemars::schema_for!(slicer_types::ProjectEvent),
                "settings": schemars::schema_for!(slicer_types::SettingsSchema),
                "settings_baseline": slicer_types::SettingsSchema::baseline(),
            });
            println!("{}", serde_json::to_string_pretty(&combined)?);
            Ok(())
        }
    }
}

/// Parse a `--input` value into a plate object.
///
/// `PATH@N` assigns tool `N`; a bare `PATH` leaves the preset's tool. The
/// split is on the **last** `@` and only counts when the suffix is a
/// positive integer, so paths that legitimately contain `@` (an email-named
/// directory, an OrcaSlicer preset filename like `PLA @Qidi X-Max 4.stl`)
/// still load instead of being rejected as a malformed tool number.
fn parse_plate_object(spec: &str) -> Result<slicer_pipeline::PlateObject> {
    if let Some((path, suffix)) = spec.rsplit_once('@') {
        if let Ok(slot) = suffix.parse::<u32>() {
            if slot == 0 {
                return Err(anyhow::anyhow!(
                    "extruder is 1-based, got `{spec}` — use @1 for the first tool"
                ));
            }
            return Ok(slicer_pipeline::PlateObject::new(path).with_extruder(slot));
        }
    }
    Ok(slicer_pipeline::PlateObject::new(spec))
}

/// Wait for SIGINT (ctrl_c) or SIGTERM on Unix; ctrl_c only on non-Unix.
///
/// The host process (Slicer/apps/host-tauri/src/daemon.rs) sends SIGTERM
/// during shutdown so the daemon can release its autosave lockfile and
/// flush the autosave session before exit. Default tokio behavior of
/// `signal::ctrl_c()` only catches SIGINT, so without this select the
/// daemon would terminate abruptly under SIGTERM and the next process
/// launch would have to clean up a stale lockfile.
#[cfg(unix)]
async fn shutdown_signal() -> anyhow::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate())?;
    tokio::select! {
        res = tokio::signal::ctrl_c() => res?,
        _ = sigterm.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> anyhow::Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// The feature has to reach every crate that gates behaviour on it, not
/// only the ones that link C++.
///
/// This exists because it did not. `with-libslic3r` was forwarded to
/// `slicer-ffi` and `slicer-pipeline` but not to `slicer-rpc`, so a
/// daemon with 43 MB of OCCT linked into it answered every
/// `geometry.build` with "this daemon was built without OCCT" and fell
/// back to the mesh kernel. No test in the workspace could see it,
/// because tests enable the feature on `slicer-rpc` directly; it took
/// running the shipped binary to find.
///
/// Checked at compile time rather than in a test, because a binary that
/// gets this wrong should not exist rather than fail a suite someone
/// might not run.
#[cfg(feature = "with-libslic3r")]
const _: () = assert!(
    slicer_rpc::geometry::BREP_KERNEL_COMPILED_IN,
    "this binary enables `with-libslic3r`, but slicer-rpc did not get it — add \
     `slicer-rpc/with-libslic3r` to that feature in slicer-daemon/Cargo.toml"
);
