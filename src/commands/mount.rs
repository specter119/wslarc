use anyhow::{bail, Context, Result};
use console::style;
use ini::Ini;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::config::{Config, Distribution};
use crate::generators::{btrbk, ext4_sync, invocation, systemd};
use crate::utils::cli::{ensure_dependencies_for, find_mount, Dependency, MountInfo};
use crate::utils::prompt::{confirm_or_yes, info, step, success, warn};
use crate::utils::shell::run_or_dry;
use crate::utils::storage::{install_copies, verify_mount};

const SYSTEMD_DIR: &str = "/etc/systemd/system";
const BTRBK_CONF: &str = "/etc/btrbk/btrbk.conf";
const WSLARC_BIN: &str = "/usr/local/bin/wslarc";
const WSL_CONF: &str = "/etc/wsl.conf";

fn has_usr_subvol(config: &Config) -> bool {
    usr_subvol_name(config).is_some()
}

fn usr_subvol_name(config: &Config) -> Option<&str> {
    config
        .subvolumes
        .backup
        .iter()
        .find(|(_, backup)| backup.mount() == "/usr")
        .map(|(subvol, _)| subvol.as_str())
}

pub fn run(
    config: &Config,
    distribution: Distribution,
    config_path: &str,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    println!("{}", style("WSL Btrfs Mount Setup").bold().cyan());

    if config.uuid.is_none() {
        bail!("UUID not set. Run 'wslarc init' first.");
    }
    validate_automation_config_path(config, Path::new(config_path))?;
    if fs::metadata(config_path)?.dev() != fs::metadata("/")?.dev() {
        bail!("Automation configuration must reside on the ext4 root filesystem");
    }

    let needs_ext4_sync = has_usr_subvol(config);
    let mut dependencies = vec![Dependency::new("btrbk", &["btrbk"])];
    if needs_ext4_sync {
        dependencies.push(Dependency::new("btrfs-progs", &["btrfs"]));
        dependencies.push(Dependency::new("rsync", &["rsync"]));
        match distribution {
            crate::config::Distribution::Arch => {
                dependencies.push(Dependency::new("pacman", &["pacman"]))
            }
            crate::config::Distribution::Debian => {
                dependencies.push(Dependency::new("dpkg", &["dpkg-query", "dpkg-deb"]))
            }
        }
    }
    ensure_dependencies_for(&dependencies, distribution)?;

    show_summary(config, distribution, needs_ext4_sync);

    if !confirm_or_yes("Generate and install systemd units?", true, yes)? {
        println!("Aborted.");
        return Ok(());
    }

    let total_steps = if needs_ext4_sync { 6 } else { 5 };

    step(1, total_steps, "Install wslarc binary");
    install_binary(config, dry_run)?;

    step(2, total_steps, "Setup wsl.conf boot command");
    update_wsl_conf(config_path, dry_run)?;

    step(3, total_steps, "Generate systemd mount units");
    generate_systemd_units(config, dry_run)?;

    step(4, total_steps, "Generate btrbk configuration");
    generate_btrbk_config(config, config_path, dry_run)?;

    if needs_ext4_sync {
        step(5, total_steps, "Setup ext4 systemd sync");
        setup_ext4_sync(config, distribution, config_path, dry_run)?;
        step(6, total_steps, "Enable systemd services");
    } else {
        step(5, total_steps, "Enable systemd services");
    }
    enable_services(config, dry_run)?;

    println!();
    println!("{}", style("Mount setup complete!").green().bold());
    println!();
    println!("Restart WSL to apply: {}", style("wsl --shutdown").cyan());

    Ok(())
}

fn show_summary(config: &Config, distribution: Distribution, needs_ext4_sync: bool) {
    println!();
    println!("{}", style("Files to generate:").bold());

    println!("  {}", WSLARC_BIN);
    println!("  {} (update [boot] command)", WSL_CONF);

    let base_unit = systemd::mount_unit_filename(&config.mount.base);
    println!("  {}/{}", SYSTEMD_DIR, base_unit);

    for backup in config.subvolumes.backup.values() {
        let unit = systemd::mount_unit_filename(backup.mount());
        println!("  {}/{}", SYSTEMD_DIR, unit);
    }

    for transfer in config.subvolumes.transfer.values() {
        let unit = systemd::mount_unit_filename(&transfer.mount);
        println!("  {}/{}", SYSTEMD_DIR, unit);
    }

    println!("  {}", BTRBK_CONF);
    println!("  {}/btrbk.service", SYSTEMD_DIR);
    println!("  {}/btrbk.timer", SYSTEMD_DIR);

    if needs_ext4_sync {
        let ext4_unit = ext4_sync::ext4_mount_unit_filename(config);
        println!("  {}/{}", SYSTEMD_DIR, ext4_unit);
        let (hook_path, _) = ext4_sync::generate_package_hook(distribution, &[]);
        println!("  {}", hook_path);
    }

    println!();
}

/// Install wslarc binary to /usr/local/bin and the configured /usr subvolume.
fn install_binary(config: &Config, dry_run: bool) -> Result<()> {
    let current_exe = std::env::current_exe()?;

    if dry_run {
        info(&format!(
            "[dry-run] Would ensure {} is installed and copy it to the configured /usr subvolume",
            WSLARC_BIN
        ));
        return Ok(());
    }

    let root = find_mount("/")?.context("Root filesystem is not mounted")?;
    let usr_mounted = find_mount("/usr")?.is_some();
    let ext4 = if usr_mounted {
        find_mount(&config.ext4_sync.mount_point)?
    } else {
        None
    };
    let mut targets = vec![ext4_binary_path(config, &root, usr_mounted, ext4.as_ref())?];

    // Also copy to the configured /usr subvolume if it is mounted.
    if let Some(usr_subvol) = usr_subvol_name(config) {
        let base = find_mount(&config.mount.base)?;
        verify_mount(
            base.as_ref(),
            &config.mount.base,
            "btrfs",
            config.uuid.as_deref().context("Btrfs UUID is missing")?,
        )?;
        let subvol_path = Path::new(&config.mount.base).join(usr_subvol);
        if !subvol_path
            .canonicalize()?
            .starts_with(Path::new(&config.mount.base).canonicalize()?)
        {
            bail!("Configured /usr subvolume is outside the Btrfs base");
        }
        run_or_dry(
            "btrfs",
            &["subvolume", "show", &subvol_path.to_string_lossy()],
            false,
        )?;
        targets.push(subvol_path.join("local/bin/wslarc"));
    }
    install_copies(&current_exe, &targets)?;

    success(&format!("wslarc installed to {}", WSLARC_BIN));
    Ok(())
}

fn ext4_binary_path(
    config: &Config,
    root: &MountInfo,
    usr_mounted: bool,
    ext4: Option<&MountInfo>,
) -> Result<PathBuf> {
    let uuid = root
        .uuid
        .as_deref()
        .context("Root filesystem UUID is unavailable")?;
    verify_mount(Some(root), "/", "ext4", uuid)?;
    if !usr_mounted {
        return Ok(PathBuf::from(WSLARC_BIN));
    }
    verify_mount(ext4, &config.ext4_sync.mount_point, "ext4", uuid).context(
        "Mount the ext4 root at the configured ext4_sync.mount_point before updating wslarc",
    )?;
    if ext4.is_some_and(|mount| mount.source != root.source) {
        bail!("The ext4 sync mount must expose the whole root filesystem, not a subdirectory");
    }
    Ok(Path::new(&config.ext4_sync.mount_point).join("usr/local/bin/wslarc"))
}

fn validate_automation_config_path(config: &Config, path: &Path) -> Result<()> {
    if !path.is_absolute() || path.to_string_lossy().chars().any(char::is_control) {
        bail!("Automation requires an absolute configuration path without control characters");
    }
    let blocked = std::iter::once(config.mount.base.as_str())
        .chain(config.subvolumes.backup.values().map(|entry| entry.mount()))
        .chain(
            config
                .subvolumes
                .transfer
                .values()
                .map(|entry| entry.mount.as_str()),
        )
        .chain([
            "/tmp",
            "/run",
            "/var/tmp",
            config.ext4_sync.mount_point.as_str(),
        ]);
    if blocked.into_iter().any(|mount| path.starts_with(mount)) {
        bail!(
            "Configuration must be on persistent ext4 storage available before managed mounts: {}",
            path.display()
        );
    }
    Ok(())
}

fn update_wsl_conf(config_path: &str, dry_run: bool) -> Result<()> {
    let attach_cmd = format!("{} attach", invocation::shell_command(config_path));
    if dry_run {
        info(&format!(
            "[dry-run] Would update {} with [boot] command",
            WSL_CONF
        ));
        return Ok(());
    }

    let mut conf = Ini::load_from_file(WSL_CONF).unwrap_or_else(|_| Ini::new());

    if let Some(boot) = conf.section(Some("boot")) {
        if let Some(cmd) = boot.get("command") {
            if cmd == attach_cmd {
                success("wsl.conf already configured");
                return Ok(());
            }
            warn(&format!("Overwriting existing [boot] command: {}", cmd));
        }
    }

    conf.with_section(Some("boot")).set("command", attach_cmd);

    conf.write_to_file(WSL_CONF)?;
    success("wsl.conf updated with boot command");
    Ok(())
}

fn generate_systemd_units(config: &Config, dry_run: bool) -> Result<()> {
    let mut units_to_verify = Vec::new();

    // Base mount
    let base_content = systemd::generate_base_mount(config);
    let base_unit = systemd::mount_unit_filename(&config.mount.base);
    write_systemd_unit(&base_unit, &base_content, dry_run)?;
    units_to_verify.push(format!("{}/{}", SYSTEMD_DIR, base_unit));
    success(&format!("{} created", base_unit));

    // Backup subvolumes (A-class)
    info("Creating A-class (backup) mount units...");
    for (subvol, backup) in &config.subvolumes.backup {
        let content =
            systemd::generate_subvol_mount(config, subvol, backup.mount(), backup.options());
        let unit = systemd::mount_unit_filename(backup.mount());
        write_systemd_unit(&unit, &content, dry_run)?;
        units_to_verify.push(format!("{}/{}", SYSTEMD_DIR, unit));
    }

    // Transfer subvolumes (C-class)
    info("Creating C-class (transfer) mount units...");
    for (subvol, transfer) in &config.subvolumes.transfer {
        let content = systemd::generate_subvol_mount(
            config,
            subvol,
            &transfer.mount,
            transfer.options.as_deref(),
        );
        let unit = systemd::mount_unit_filename(&transfer.mount);
        write_systemd_unit(&unit, &content, dry_run)?;
        units_to_verify.push(format!("{}/{}", SYSTEMD_DIR, unit));
    }

    // Verify all units with systemd-analyze
    if !dry_run {
        info("Validating systemd units...");
        let mut args = vec!["verify"];
        let unit_refs: Vec<&str> = units_to_verify.iter().map(|s| s.as_str()).collect();
        args.extend(unit_refs);
        run_or_dry("systemd-analyze", &args, false)?;
    }

    success("All mount units created and validated");
    Ok(())
}

fn generate_btrbk_config(config: &Config, config_path: &str, dry_run: bool) -> Result<()> {
    // Create /etc/btrbk directory
    if !dry_run {
        fs::create_dir_all("/etc/btrbk")?;
    }

    // Generate btrbk.conf
    let conf_content = btrbk::generate_config(config);
    write_file(BTRBK_CONF, &conf_content, dry_run)?;

    // Validate btrbk config syntax
    if !dry_run {
        info("Validating btrbk.conf syntax...");
        run_or_dry("btrbk", &["-c", BTRBK_CONF, "dryrun"], false)?;
    }
    success("btrbk.conf created and validated");

    // Generate btrbk.service
    let service_content = btrbk::generate_service(config, config_path);
    write_systemd_unit("btrbk.service", &service_content, dry_run)?;
    success("btrbk.service created");

    // Generate btrbk.timer
    let timer_content = btrbk::generate_timer(&config.btrbk.timer_schedule);
    write_systemd_unit("btrbk.timer", &timer_content, dry_run)?;
    success("btrbk.timer created");

    Ok(())
}

fn enable_services(config: &Config, dry_run: bool) -> Result<()> {
    // Reload systemd
    run_or_dry("systemctl", &["daemon-reload"], dry_run)?;
    success("systemd daemon reloaded");

    // Enable base mount
    let base_unit = systemd::mount_unit_filename(&config.mount.base);
    run_or_dry("systemctl", &["enable", &base_unit], dry_run)?;

    // Enable backup mounts
    for backup in config.subvolumes.backup.values() {
        let unit = systemd::mount_unit_filename(backup.mount());
        run_or_dry("systemctl", &["enable", &unit], dry_run)?;
    }

    // Enable transfer mounts
    for transfer in config.subvolumes.transfer.values() {
        let unit = systemd::mount_unit_filename(&transfer.mount);
        run_or_dry("systemctl", &["enable", &unit], dry_run)?;
    }

    // Enable btrbk timer
    run_or_dry("systemctl", &["enable", "btrbk.timer"], dry_run)?;

    // Enable the ext4 root mount after its unit file has been generated.
    if has_usr_subvol(config) {
        let ext4_unit = ext4_sync::ext4_mount_unit_filename(config);
        run_or_dry("systemctl", &["enable", &ext4_unit], dry_run)?;
    }

    success("All services enabled");
    Ok(())
}

fn write_file(path: &str, content: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        info(&format!("[dry-run] Would write {}", path));
        return Ok(());
    }

    // Create parent directory if needed
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, content)?;
    Ok(())
}

/// Write systemd unit file to ext4 /etc
fn write_systemd_unit(filename: &str, content: &str, dry_run: bool) -> Result<()> {
    let path = format!("{}/{}", SYSTEMD_DIR, filename);
    write_file(&path, content, dry_run)
}

fn setup_ext4_sync(
    config: &Config,
    distribution: Distribution,
    config_path: &str,
    dry_run: bool,
) -> Result<()> {
    let ext4_uuid = ext4_sync::get_ext4_root_uuid()
        .ok_or_else(|| anyhow::anyhow!("Could not get ext4 root UUID"))?;
    info(&format!("ext4 root UUID: {}", ext4_uuid));

    let mount_point = &config.ext4_sync.mount_point;
    if !dry_run {
        fs::create_dir_all(mount_point)?;
    }

    let mount_unit = ext4_sync::generate_ext4_mount(config, &ext4_uuid);
    let mount_unit_name = ext4_sync::ext4_mount_unit_filename(config);
    write_systemd_unit(&mount_unit_name, &mount_unit, dry_run)?;
    success(&format!("{} created", mount_unit_name));

    if !dry_run {
        info("Validating ext4 mount unit...");
        let unit_path = format!("{}/{}", SYSTEMD_DIR, mount_unit_name);
        run_or_dry("systemd-analyze", &["verify", &unit_path], false)?;
    }

    let hook_targets = ext4_sync::collect_hook_targets(distribution)?;
    let (hook_path, hook) = configured_package_hook(distribution, &hook_targets, config_path);
    write_file(hook_path, &hook, dry_run)?;
    success(&format!(
        "{} package hook created",
        distribution.display_name()
    ));

    Ok(())
}

fn configured_package_hook(
    distribution: Distribution,
    targets: &[String],
    config_path: &str,
) -> (&'static str, String) {
    let (hook_path, hook) = ext4_sync::generate_package_hook(distribution, targets);
    let command = invocation::shell_command(config_path);
    let command = if distribution == Distribution::Debian {
        command.replace('\\', "\\\\").replace('"', "\\\"")
    } else {
        command
    };
    (hook_path, hook.replace(WSLARC_BIN, &command))
}

#[cfg(test)]
mod tests {
    use super::{
        configured_package_hook, ext4_binary_path, usr_subvol_name,
        validate_automation_config_path, MountInfo,
    };
    use crate::config::{BackupSubvol, Config, Distribution};
    use std::path::Path;

    #[test]
    fn usr_subvolume_uses_configured_key() {
        let mut config = Config::for_distribution(Distribution::Arch);
        let usr = config.subvolumes.backup.remove("@usr").unwrap();
        config
            .subvolumes
            .backup
            .insert("@system_usr".to_string(), usr);

        assert_eq!(usr_subvol_name(&config), Some("@system_usr"));
    }

    #[test]
    fn usr_subvolume_ignores_other_mounts() {
        let mut config = Config::for_distribution(Distribution::Arch);
        config.subvolumes.backup.insert(
            "@data".to_string(),
            BackupSubvol::Simple("/data".to_string()),
        );

        assert_eq!(usr_subvol_name(&config), Some("@usr"));
    }

    #[test]
    fn automation_config_must_be_available_before_managed_mounts() {
        let config = Config::for_distribution(Distribution::Arch);
        assert!(validate_automation_config_path(&config, Path::new("/etc/custom.toml")).is_ok());
        for path in [
            "/usr/local/config.toml",
            "/mnt/btrfs/config.toml",
            "/tmp/config.toml",
            "/etc/a\nb",
        ] {
            assert!(validate_automation_config_path(&config, Path::new(path)).is_err());
        }
    }

    #[test]
    fn generated_hooks_keep_the_selected_config() {
        let path = "/etc/wslarc/custom config.toml";
        let (_, arch) = configured_package_hook(Distribution::Arch, &[], path);
        let (_, debian) = configured_package_hook(Distribution::Debian, &[], path);
        assert!(arch.contains("--config '/etc/wslarc/custom config.toml'"));
        assert_eq!(
            debian
                .matches("--config '/etc/wslarc/custom config.toml'")
                .count(),
            2
        );
    }

    #[test]
    fn binary_install_selects_verified_hidden_ext4_root() {
        let config = Config::for_distribution(Distribution::Arch);
        let root = MountInfo {
            target: "/".into(),
            source: "/dev/root".into(),
            fstype: "ext4".into(),
            options: "rw".into(),
            uuid: Some("root-id".into()),
        };
        let mut ext4 = root.clone();
        ext4.target = config.ext4_sync.mount_point.clone();
        assert_eq!(
            ext4_binary_path(&config, &root, true, Some(&ext4)).unwrap(),
            Path::new(&config.ext4_sync.mount_point).join("usr/local/bin/wslarc")
        );
        assert!(ext4_binary_path(&config, &root, true, None).is_err());
        ext4.uuid = Some("wrong".into());
        assert!(ext4_binary_path(&config, &root, true, Some(&ext4)).is_err());
        assert_eq!(
            ext4_binary_path(&config, &root, false, None).unwrap(),
            Path::new("/usr/local/bin/wslarc")
        );
    }

    #[test]
    fn apt_hook_escapes_config_for_both_shell_and_apt_string() {
        let (_, hook) = configured_package_hook(Distribution::Debian, &[], "/etc/a'b\"c.toml");
        assert!(hook.contains(r#"/etc/a'\\''b\"c.toml"#));
    }
}
