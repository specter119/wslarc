use anyhow::Result;
use clap::{Parser, Subcommand};
use log::debug;

mod commands;
mod config;
mod generators;
mod utils;

#[cfg(test)]
mod ablation;

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
    Umount {
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

    /// Sync systemd packages to ext4 root (called by the distribution package hook)
    HookSyncSystemd {
        #[arg(long)]
        dry_run: bool,
        #[arg(long, hide = true)]
        apt_pre: bool,
        #[arg(long, hide = true)]
        apt_post: bool,
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
    /// Preview or remove snapshots outside the retention policy
    Prune {
        /// Only show what would be removed
        #[arg(long)]
        dry_run: bool,
    },
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

    let distribution = config::Distribution::detect();
    let config_path = cli.config.as_deref().unwrap_or("/etc/wslarc/config.toml");
    debug!("Loading config from: {}", config_path);
    let cfg = if matches!(&cli.command, Commands::Init { .. }) {
        config::Config::load_or_default_unexpanded(config_path, distribution)?
    } else {
        // A missing runtime config must not silently select a fresh template.
        config::Config::load(config_path)?
    };

    match cli.command {
        Commands::Init { dry_run } => {
            commands::init::run(&cfg, distribution, config_path, cli.yes, dry_run)?;
        }
        Commands::Mount { dry_run } => {
            let absolute_config = std::fs::canonicalize(config_path)?;
            let absolute_config = absolute_config
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Configuration path must be UTF-8"))?;
            commands::mount::run(&cfg, distribution, absolute_config, cli.yes, dry_run)?;
        }
        Commands::Umount { dry_run } => {
            commands::umount::run(&cfg, cli.yes, dry_run)?;
        }
        Commands::Status => {
            commands::status::run(&cfg, distribution)?;
        }
        Commands::Snapshot { action } => match action {
            SnapshotAction::Run => commands::snapshot::run(&cfg, distribution)?,
            SnapshotAction::List => commands::snapshot::list(&cfg, distribution)?,
            SnapshotAction::Prune { dry_run } => {
                commands::snapshot::prune(&cfg, distribution, cli.yes, dry_run)?
            }
        },
        Commands::Restore { snapshot } => {
            commands::restore::run(&cfg, distribution, snapshot, cli.yes)?;
        }
        Commands::HookSyncSystemd {
            dry_run,
            apt_pre,
            apt_post,
        } => {
            if apt_pre {
                if dry_run {
                    println!("[dry-run] Would record APT package targets");
                } else if let Err(error) = commands::hook_sync_systemd::run_apt_pre() {
                    eprintln!("warning: APT pre-sync hook failed: {error:#}");
                }
            } else if apt_post {
                if let Err(error) =
                    commands::hook_sync_systemd::run_apt_post(&cfg, distribution, dry_run)
                {
                    eprintln!("warning: APT ext4 synchronization failed: {error:#}");
                }
            } else {
                commands::hook_sync_systemd::run(&cfg, distribution, dry_run)?;
            }
        }
        Commands::Attach => {
            commands::attach::run(&cfg)?;
        }
    }

    Ok(())
}
