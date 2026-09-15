use anyhow::{bail, Context, Result};
use console::style;
use std::io::Write;
use std::path::Path;

use crate::config::{Config, Distribution};
use crate::generators::btrbk;
use crate::utils::cli::{ensure_dependencies_for, find_mount, list_directory_names, Dependency};
use crate::utils::prompt::{confirm_or_yes, info, success, warn};
use crate::utils::shell::{run as shell_run, run_with_output};
use crate::utils::storage::{atomic_write, verify_mount};

const BTRBK_CONF: &str = "/etc/btrbk/btrbk.conf";

pub fn run(config: &Config, distribution: Distribution) -> Result<()> {
    println!("{}", style("Creating Btrfs Snapshot").bold().cyan());
    println!();

    let mut dependencies = vec![
        Dependency::new("btrbk", &["btrbk"]),
        Dependency::new("btrfs-progs", &["btrfs"]),
    ];
    if !config.subvolumes.snapshot_only.is_empty() {
        dependencies.push(Dependency::new("rsync", &["rsync"]));
    }
    ensure_dependencies_for(&dependencies, distribution)?;

    verify_volume(config)?;
    verify_snapshot_targets(config)?;
    let generated = temporary_btrbk_config(config)?;
    write_btrbk_config(config)?;
    sync_snapshot_only(config)?;

    info("Running btrbk...");
    run_btrbk(generated.path(), &["-v", "run"])?;

    success("Snapshot and retention run completed");
    println!();
    println!("View snapshots: {}", style("wslarc snapshot list").cyan());

    Ok(())
}

fn verify_volume(config: &Config) -> Result<()> {
    let uuid = config
        .uuid
        .as_deref()
        .context("UUID not set; run wslarc init first")?;
    let mounted = find_mount(&config.mount.base)?;
    verify_mount(mounted.as_ref(), &config.mount.base, "btrfs", uuid)
}

fn verify_snapshot_targets(config: &Config) -> Result<()> {
    let base = Path::new(&config.mount.base).canonicalize()?;
    for (subvol, snapshot) in &config.subvolumes.snapshot_only {
        let target = base.join(subvol).canonicalize()?;
        let source = Path::new(&snapshot.source).canonicalize()?;
        if !target.starts_with(&base) || source.starts_with(&target) || target.starts_with(&source)
        {
            bail!("Snapshot source and destination overlap or escape the Btrfs base: {subvol}");
        }
        if !source.is_dir() {
            bail!("Snapshot source is not a directory: {}", source.display());
        }
        shell_run("btrfs", &["subvolume", "show", &target.to_string_lossy()])?;
    }
    Ok(())
}

fn sync_snapshot_only(config: &Config) -> Result<()> {
    for (subvol, snapshot) in &config.subvolumes.snapshot_only {
        info(&format!("Syncing {} to {}...", snapshot.source, subvol));
        let target = format!("{}/{}/", config.mount.base, subvol);
        run_with_output(
            "rsync",
            &[
                "-aAX",
                "--delete",
                &format!("{}/", snapshot.source),
                &target,
            ],
        )?;
        success(&format!("{} synced to {}", snapshot.source, subvol));
    }

    Ok(())
}

pub fn prune(config: &Config, distribution: Distribution, yes: bool, dry_run: bool) -> Result<()> {
    println!("{}", style("Prune Btrfs Snapshots").bold().cyan());
    println!();

    ensure_dependencies_for(&[Dependency::new("btrbk", &["btrbk"])], distribution)?;
    verify_volume(config)?;

    println!(
        "  Retention minimum: {}",
        style(&config.btrbk.preserve_min).yellow()
    );
    println!(
        "  Retention policy: {}",
        style(&config.btrbk.preserve).yellow()
    );

    if dry_run {
        info("Previewing snapshots that are outside the retention policy...");
        execute_prune(config, Path::new(BTRBK_CONF), true, run_with_output)?;
        success("Prune preview completed; no snapshots were removed");
        return Ok(());
    }

    warn("This permanently deletes snapshots outside the configured retention policy.");
    if !confirm_or_yes("Proceed with snapshot cleanup?", false, yes)? {
        println!("Aborted.");
        return Ok(());
    }

    info("Removing snapshots outside the retention policy...");
    execute_prune(config, Path::new(BTRBK_CONF), false, run_with_output)?;
    success("Snapshot cleanup completed");

    Ok(())
}

pub fn list(config: &Config, distribution: Distribution) -> Result<()> {
    println!("{}", style("Btrfs Snapshots").bold().cyan());
    println!();

    ensure_dependencies_for(&[Dependency::new("btrbk", &["btrbk"])], distribution)?;

    // Try btrbk list first
    verify_volume(config)?;
    let generated = temporary_btrbk_config(config)?;
    let generated_path = generated
        .path()
        .to_str()
        .context("Invalid temporary path")?;
    let args = btrbk_args(generated_path, &["list", "snapshots"]);
    let btrbk_list = shell_run("btrbk", &args);

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

fn write_btrbk_config(config: &Config) -> Result<()> {
    atomic_write(
        Path::new(BTRBK_CONF),
        btrbk::generate_config(config).as_bytes(),
    )
}

fn temporary_btrbk_config(config: &Config) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(btrbk::generate_config(config).as_bytes())?;
    file.flush()?;
    Ok(file)
}

fn execute_prune<F>(
    config: &Config,
    installed_path: &Path,
    dry_run: bool,
    mut execute: F,
) -> Result<()>
where
    F: FnMut(&str, &[&str]) -> Result<()>,
{
    let generated = temporary_btrbk_config(config)?;
    if !dry_run {
        atomic_write(installed_path, btrbk::generate_config(config).as_bytes())?;
    }
    let config_path = generated
        .path()
        .to_str()
        .context("Invalid temporary path")?;
    execute("btrbk", &btrbk_args(config_path, &prune_args(dry_run)))
}

fn btrbk_args<'a>(config_path: &'a str, action: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["-c", config_path];
    args.extend_from_slice(action);
    args
}

fn run_btrbk(config_path: &Path, action: &[&str]) -> Result<()> {
    let path = config_path.to_str().context("Invalid btrbk config path")?;
    let args = btrbk_args(path, action);
    run_with_output("btrbk", &args)
}

fn prune_args(dry_run: bool) -> Vec<&'static str> {
    if dry_run {
        vec!["--dry-run", "-v", "prune"]
    } else {
        vec!["-v", "prune"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_args_use_dry_run_only_for_preview() {
        assert_eq!(prune_args(true), vec!["--dry-run", "-v", "prune"]);
        assert_eq!(prune_args(false), vec!["-v", "prune"]);
    }

    #[test]
    fn btrbk_args_always_use_wslarc_config() {
        assert_eq!(
            btrbk_args(BTRBK_CONF, &["-v", "run"]),
            vec!["-c", "/etc/btrbk/btrbk.conf", "-v", "run"]
        );
    }

    #[test]
    fn prune_preview_uses_current_config_without_changing_installed_file() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("btrbk.conf");
        std::fs::write(&installed, b"original").unwrap();
        let before = std::fs::metadata(&installed).unwrap().modified().unwrap();
        let mut config = Config::for_distribution(Distribution::Arch);
        config.mount.base = "/custom/btrfs".into();
        let mut called = false;
        execute_prune(&config, &installed, true, |command, args| {
            called = true;
            assert_eq!(command, "btrbk");
            assert!(args.contains(&"--dry-run"));
            assert_ne!(Path::new(args[1]), installed);
            assert!(std::fs::read_to_string(args[1])?.contains("volume /custom/btrfs"));
            Ok(())
        })
        .unwrap();
        assert!(called);
        assert_eq!(std::fs::read(&installed).unwrap(), b"original");
        assert_eq!(
            std::fs::metadata(installed).unwrap().modified().unwrap(),
            before
        );
    }

    #[test]
    fn prune_preview_does_not_create_installed_directory() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("absent/btrbk.conf");
        execute_prune(&Config::default(), &installed, true, |_, _| Ok(())).unwrap();
        assert!(!installed.parent().unwrap().exists());
    }
}
