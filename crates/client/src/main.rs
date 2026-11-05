#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! `chattercrab`: the Chattercrab desktop client.
//!
//! The iced UI runs on the main thread. Networking (REST, signaling, reconnect) and
//! the media engine run on a separate Tokio runtime thread; the two sides talk only
//! through bounded channels.

mod config;
mod icon;
mod net;
mod ui;

use std::sync::{Arc, Mutex};

use anyhow::Context;
use iced::Size;
use tokio::sync::mpsc;

use config::Config;
use net::{APP_EVENT_CAPACITY, UI_COMMAND_CAPACITY, UiCommand};
use ui::App;

/// Log filter when `RUST_LOG` is unset: our logs at info, chatty GPU/windowing crates at warn.
const DEFAULT_LOG_FILTER: &str =
    "info,iced_winit=warn,iced_wgpu=warn,wgpu=warn,wgpu_core=warn,wgpu_hal=warn,naga=warn";

const USAGE: &str = "\
Usage: chattercrab [OPTIONS]

Options:
  --software-render   Use the tiny-skia CPU renderer instead of the GPU (wgpu)
  --config <PATH>     Use this config file instead of the platform default
  -h, --help          Print this help
  -V, --version       Print the version";

/// Parsed command-line options.
#[derive(Debug, Default, PartialEq)]
struct Args {
    software_render: bool,
    config: Option<std::path::PathBuf>,
}

/// What `main` should do after parsing the command line.
#[derive(Debug, PartialEq)]
enum Cli {
    Run(Args),
    Help,
    Version,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> anyhow::Result<Cli> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--software-render" => parsed.software_render = true,
            "--config" => {
                let path = args.next().context("--config needs a path")?;
                parsed.config = Some(path.into());
            }
            "-h" | "--help" => return Ok(Cli::Help),
            "-V" | "--version" => return Ok(Cli::Version),
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    Ok(Cli::Run(parsed))
}

fn main() -> anyhow::Result<()> {
    let args = match parse_args(std::env::args().skip(1))? {
        Cli::Run(args) => args,
        Cli::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        Cli::Version => {
            println!("chattercrab {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    };
    if args.software_render {
        // SAFETY: no other threads exist yet, so nothing can read the environment
        // concurrently. iced reads ICED_BACKEND when it creates the compositor.
        unsafe { std::env::set_var("ICED_BACKEND", "tiny-skia") };
    } else if cfg!(windows) && std::env::var_os("WGPU_BACKEND").is_none() {
        // DX12 idles with markedly less memory than Vulkan or GL on Windows drivers.
        // If it is unavailable iced falls back to tiny-skia. Users can still override
        // the choice with WGPU_BACKEND.
        // SAFETY: as above, still single-threaded.
        unsafe { std::env::set_var("WGPU_BACKEND", "dx12") };
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER)),
        )
        .with_writer(std::io::stderr)
        .init();

    // One process-wide TLS crypto provider (ring) for reqwest and tungstenite.
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("a rustls crypto provider was already installed");
    }

    let config_path = args.config.or_else(Config::default_path);
    let config = match config_path.as_deref().map(Config::load).transpose() {
        Ok(config) => config.unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "ignoring unreadable config");
            Config::default()
        }
    };

    let (cmd_tx, cmd_rx) = mpsc::channel(UI_COMMAND_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(APP_EVENT_CAPACITY);
    let net_thread =
        net::backend::spawn_runtime_thread(cmd_rx, event_tx).context("spawning network thread")?;
    let events: ui::EventSlot = Arc::new(Mutex::new(Some(event_rx)));

    let window = iced::window::Settings {
        size: Size::new(1100.0, 720.0),
        min_size: Some(Size::new(720.0, 480.0)),
        icon: iced::window::icon::from_rgba(icon::crab_rgba(64), 64, 64).ok(),
        ..Default::default()
    };

    let boot_tx = cmd_tx.clone();
    let result = iced::application(
        move || {
            App::new(
                config.clone(),
                config_path.clone(),
                boot_tx.clone(),
                events.clone(),
            )
        },
        App::update,
        App::view,
    )
    .title(App::title)
    .theme(App::theme)
    .subscription(App::subscription)
    // No canvas or meshes are drawn, so MSAA would only cost GPU memory.
    .antialiasing(false)
    .window(window)
    .run();

    let _ = cmd_tx.try_send(UiCommand::Shutdown);
    drop(cmd_tx);
    if net_thread.join().is_err() {
        tracing::error!("network thread panicked");
    }
    result.context("running the UI")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> anyhow::Result<Cli> {
        parse_args(list.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn parses_flags() {
        assert_eq!(args(&[]).unwrap(), Cli::Run(Args::default()));
        assert_eq!(
            args(&["--software-render", "--config", "x.toml"]).unwrap(),
            Cli::Run(Args {
                software_render: true,
                config: Some("x.toml".into())
            })
        );
        assert_eq!(args(&["--help"]).unwrap(), Cli::Help);
        assert_eq!(args(&["-V"]).unwrap(), Cli::Version);
        assert!(args(&["--bogus"]).is_err());
        assert!(args(&["--config"]).is_err());
    }
}
