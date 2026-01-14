use anyhow::Result;
use clap::{Parser, Subcommand};
use log::debug;

mod commands;
mod config;
mod generators;
mod utils;

#[derive(Parser)]
#[command(name = "wslarc")]
#[command(about = "WSL2 Btrfs backup and restore tool", long_about = None)]
#[command(version)]
struct Cli {
    /// Path to config file
    #[arg(short, long, global = true)]
    config: Option<String>,

    /// Skip confirmation prompts
    #[arg(short, long, global = true)]
    yes: bool,

    /// Verbose output (can be repeated: -v, -vv, -vvv)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize Btrfs VHDX and create subvolumes
    Init {
        /// Only show what would be done
        #[arg(long)]
        dry_run: bool,
    },

    /// Generate and install systemd mount units
    Mount {
        /// Only generate files, don't install
        #[arg(long)]
        dry_run: bool,
    },

    /// Disable systemd mount units
    Unmount {
        /// Only show what would be done
        #[arg(long)]
        dry_run: bool,
    },

    /// Show current status (mounts, subvolumes, snapshots)
    Status,

    /// Snapshot operations
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },

    /// Restore from a snapshot
    Restore {
        /// Snapshot name to restore from
        #[arg(short, long)]
        snapshot: Option<String>,
    },

    /// Sync systemd packages to ext4 root (called by pacman hook)
    HookSyncSystemd {
        #[arg(long)]
        dry_run: bool,
    },

    /// Attach Btrfs VHDX if not already mounted (called by wsl.conf at boot)
    Attach,
}

#[derive(Subcommand)]
enum SnapshotAction {
    /// Create a new snapshot (runs btrbk)
    Run,
    /// List available snapshots
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = match cli.verbose {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Info,
        2 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    };
    env_logger::Builder::new()
        .filter_level(log_level)
        .format_timestamp(None)
        .format_target(false)
        .init();

    debug!("Log level: {:?}", log_level);

    let config_path = cli.config.as_deref().unwrap_or("/etc/wslarc/config.toml");
    debug!("Loading config from: {}", config_path);
    let cfg = config::Config::load_or_default(config_path)?;

    match cli.command {
        Commands::Init { dry_run } => {
            commands::init::run(&cfg, cli.yes, dry_run)?;
        }
        Commands::Mount { dry_run } => {
            commands::mount::run(&cfg, cli.yes, dry_run)?;
        }
        Commands::Unmount { dry_run } => {
            commands::unmount::run(&cfg, cli.yes, dry_run)?;
        }
        Commands::Status => {
            commands::status::run(&cfg)?;
        }
        Commands::Snapshot { action } => match action {
            SnapshotAction::Run => commands::snapshot::run(&cfg)?,
            SnapshotAction::List => commands::snapshot::list(&cfg)?,
        },
        Commands::Restore { snapshot } => {
            commands::restore::run(&cfg, snapshot, cli.yes)?;
        }
        Commands::HookSyncSystemd { dry_run } => {
            commands::hook_sync_systemd::run(&cfg, dry_run)?;
        }
        Commands::Attach => {
            commands::attach::run(&cfg)?;
        }
    }

    Ok(())
}
