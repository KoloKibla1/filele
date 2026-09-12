mod discovery;
mod gui;
mod progress;
mod protocol;
mod receiver;
mod sender;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "filele", version, about = "Fastest LAN file transfer for Windows (parallel streams + pipelined small files). Run with no args to open the GUI.")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Commands>,
    /// Verbose logs
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Send files/dirs to a receiver
    Send {
        /// Files or directories to send
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Receiver IP/hostname (or use `discover` to find peers)
        #[arg(short, long)]
        to: String,
        /// Control port (data = port+1)
        #[arg(short, long, default_value_t = protocol::DEFAULT_PORT)]
        port: u16,
        /// Parallel data streams for large files (0-8). 0 = auto/single.
        #[arg(short, long, default_value_t = 4)]
        streams: u8,
        /// LZ4-compress small files (fast, skips media/archives)
        #[arg(long)]
        compress: bool,
        /// Disable xxh3 checksums (max speed, no integrity check)
        #[arg(long)]
        no_checksum: bool,
        /// Ask receiver to overwrite existing files
        #[arg(long)]
        overwrite: bool,
    },
    /// Receive files (listen)
    Recv {
        /// Bind address
        #[arg(short, long, default_value = "0.0.0.0")]
        bind: String,
        /// Control port (data = port+1)
        #[arg(short, long, default_value_t = protocol::DEFAULT_PORT)]
        port: u16,
        /// Output directory
        #[arg(short, long, default_value = ".")]
        out: PathBuf,
        /// Overwrite existing files by default
        #[arg(long)]
        overwrite: bool,
    },
    /// Alias for recv
    Receive {
        #[arg(short, long, default_value = "0.0.0.0")]
        bind: String,
        #[arg(short, long, default_value_t = protocol::DEFAULT_PORT)]
        port: u16,
        #[arg(short, long, default_value = ".")]
        out: PathBuf,
        #[arg(long)]
        overwrite: bool,
    },
    /// Discover receivers on LAN via UDP broadcast
    Discover {
        /// Timeout in seconds
        #[arg(short, long, default_value_t = 3)]
        timeout: u64,
    },
    /// Open the graphical interface (also the default when no args are given)
    Gui,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // GUI mode detaches from the console so only the app window remains.
    // CLI modes (send/recv/discover) keep the console.
    // Double-click friendly: no args -> GUI.
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() <= 1 {
        #[cfg(windows)]
        hide_console();
        return gui::launch_gui().await;
    }
    let cli = Cli::parse();
    init_logging(cli.verbose);

    // Windows: raise IO efficiency — big threadpool for disk + net.
    // Tokio multi_thread default uses num_cpus workers; ensure at least 8 for NVMe+10G.
    match cli.cmd {
        None => {
            #[cfg(windows)]
            hide_console();
            gui::launch_gui().await?
        }
        Some(Commands::Gui) => {
            #[cfg(windows)]
            hide_console();
            gui::launch_gui().await?
        }
        Some(Commands::Send { paths, to, port, streams, compress, no_checksum, overwrite }) => {
            for p in &paths {
                if !p.exists() {
                    anyhow::bail!("path not found: {}", p.display());
                }
            }
            sender::run_send(
                paths,
                sender::SendOptions {
                    target: to,
                    port,
                    streams,
                    compress,
                    checksum: !no_checksum,
                    overwrite,
                },
            )
            .await?;
        }
        Some(Commands::Recv { bind, port, out, overwrite }) => {
            receiver::run_recv(receiver::RecvOptions { bind, port, out, overwrite }).await?;
        }
        Some(Commands::Receive { bind, port, out, overwrite }) => {
            receiver::run_recv(receiver::RecvOptions { bind, port, out, overwrite }).await?;
        }
        Some(Commands::Discover { timeout }) => {
            println!("Broadcasting discovery ({}s)...", timeout);
            let peers = discovery::discover(Duration::from_secs(timeout)).await?;
            if peers.is_empty() {
                println!("No peers found. Is `filele recv` running on the other PC? (check firewall)");
                println!("Windows Firewall must allow TCP {}-{} and UDP {} inbound.", protocol::DEFAULT_PORT, protocol::DEFAULT_PORT+1, protocol::DISCOVERY_PORT);
            } else {
                println!("Found {} peer(s):", peers.len());
                for p in peers {
                    println!("  {}  {}:{}", p.name, p.addr.ip(), p.port);
                }
                println!("\nSend with: filele send <files> --to <ip>");
            }
        }
    }
    Ok(())
}

fn init_logging(verbose: bool) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = if verbose {
        EnvFilter::new("filele=debug,tower=info")
    } else {
        EnvFilter::new("filele=info")
    };
    let _ = fmt().with_env_filter(filter).try_init();
}

/// Detach the GUI process from its console so only the app window remains.
/// Safe when launched from an existing terminal: the parent terminal stays
/// visible, only this process detaches. When double-clicked (own console),
/// the console window closes. CLI modes never call this.
#[cfg(windows)]
fn hide_console() {
    unsafe {
        extern "system" {
            fn FreeConsole() -> i32;
        }
        FreeConsole();
    }
}
