use anyhow::Result;
use console::style;

use crate::config::Config;
use crate::utils::cli::{ensure_dependencies, list_directory_names, Dependency};
use crate::utils::prompt::{info, success};
use crate::utils::shell::{run as shell_run, run_with_output};

pub fn run(_config: &Config) -> Result<()> {
    println!("{}", style("Creating Btrfs Snapshot").bold().cyan());
    println!();

    ensure_dependencies(&[Dependency::new("btrbk", &["btrbk"])])?;

    info("Running btrbk...");
    run_with_output("btrbk", &["-v", "run"])?;

    success("Snapshot created");
    println!();
    println!("View snapshots: {}", style("wslarc snapshot list").cyan());

    Ok(())
}

pub fn list(config: &Config) -> Result<()> {
    println!("{}", style("Btrfs Snapshots").bold().cyan());
    println!();

    ensure_dependencies(&[Dependency::new("btrbk", &["btrbk"])])?;

    // Try btrbk list first
    let btrbk_list = shell_run("btrbk", &["list", "snapshots"]);

    match btrbk_list {
        Ok(output) if !output.is_empty() => {
            println!("{}", output);
        }
        _ => {
            // Fallback to direct directory listing
            let snapshot_dir = format!("{}/{}", config.mount.base, config.btrbk.snapshot_dir);
            info(&format!("Listing {}", snapshot_dir));
            println!();

            match list_directory_names(&snapshot_dir) {
                Ok(entries) if !entries.is_empty() => {
                    for entry in entries {
                        println!("{}", entry);
                    }
                }
                Ok(_) => println!("No snapshots found"),
                Err(e) => println!("Could not list snapshots: {}", e),
            }
        }
    }

    Ok(())
}
