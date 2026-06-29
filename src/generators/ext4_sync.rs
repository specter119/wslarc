use anyhow::Result;
use std::collections::{BTreeSet, VecDeque};

use crate::config::Config;
use crate::generators::systemd::path_to_unit_name;
use crate::utils::cli::{find_mount_uuid, pacman_query_depends, pacman_query_version};

pub const SYSTEMD_PACKAGES: [&str; 3] = ["systemd", "systemd-libs", "systemd-sysvcompat"];

/// Get ext4 root UUID dynamically
pub fn get_ext4_root_uuid() -> Option<String> {
    find_mount_uuid("/")
}

/// Generate systemd mount unit for ext4 root
pub fn generate_ext4_mount(config: &Config, uuid: &str) -> String {
    let mount_point = &config.ext4_sync.mount_point;

    format!(
        r#"[Unit]
Description=Mount ext4 root for sync

[Mount]
What=UUID={uuid}
Where={mount_point}
Type=ext4
Options=defaults

[Install]
WantedBy=multi-user.target
"#
    )
}

pub fn generate_pacman_hook(targets: &[String]) -> String {
    let mut lines = Vec::new();
    lines.push("[Trigger]".to_string());
    lines.push("Operation = Upgrade".to_string());
    lines.push("Type = Package".to_string());
    for target in targets {
        lines.push(format!("Target = {}", target));
    }
    lines.push(String::new());
    lines.push("[Action]".to_string());
    lines.push("Description = Syncing systemd to ext4...".to_string());
    lines.push("When = PostTransaction".to_string());
    lines.push("NeedsTargets".to_string());
    lines.push("Exec = /usr/local/bin/wslarc hook-sync-systemd".to_string());
    lines.push(String::new());
    lines.join("\n")
}

pub fn collect_hook_targets() -> Result<Vec<String>> {
    collect_recursive_targets(&SYSTEMD_PACKAGES, pacman_query_depends, |pkg| {
        Ok(pacman_query_version(pkg)?.is_some())
    })
}

fn collect_recursive_targets<F, G>(
    roots: &[&str],
    mut deps_for: F,
    mut is_installed: G,
) -> Result<Vec<String>>
where
    F: FnMut(&str) -> Result<Vec<String>>,
    G: FnMut(&str) -> Result<bool>,
{
    let mut queue: VecDeque<String> = roots.iter().map(|pkg| (*pkg).to_string()).collect();
    let mut seen = BTreeSet::new();

    while let Some(pkg) = queue.pop_front() {
        if !seen.insert(pkg.clone()) {
            continue;
        }

        for dep in deps_for(&pkg)? {
            if !seen.contains(&dep) && is_installed(&dep)? {
                queue.push_back(dep);
            }
        }
    }

    Ok(seen.into_iter().collect())
}

pub fn ext4_mount_unit_filename(config: &Config) -> String {
    let mount_point = &config.ext4_sync.mount_point;
    format!("{}.mount", path_to_unit_name(mount_point))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_pacman_hook_includes_needs_targets() {
        let hook = generate_pacman_hook(&["systemd".to_string(), "glibc".to_string()]);

        assert!(hook.contains("Target = systemd"));
        assert!(hook.contains("Target = glibc"));
        assert!(hook.contains("NeedsTargets"));
    }

    #[test]
    fn test_collect_recursive_targets_walks_transitive_dependencies() {
        let targets = collect_recursive_targets(
            &["systemd"],
            |pkg| {
                Ok(match pkg {
                    "systemd" => vec!["liba".to_string(), "libb".to_string()],
                    "liba" => vec!["libc".to_string()],
                    _ => Vec::new(),
                })
            },
            |_| Ok(true),
        )
        .unwrap();

        assert_eq!(
            targets,
            vec![
                "liba".to_string(),
                "libb".to_string(),
                "libc".to_string(),
                "systemd".to_string(),
            ]
        );
    }

    #[test]
    fn test_collect_recursive_targets_skips_uninstalled_and_cycles() {
        let targets = collect_recursive_targets(
            &["systemd-libs"],
            |pkg| {
                Ok(match pkg {
                    "systemd-libs" => vec!["glibc".to_string(), "sh".to_string()],
                    "glibc" => vec!["systemd-libs".to_string(), "libcap".to_string()],
                    _ => Vec::new(),
                })
            },
            |pkg| Ok(pkg != "sh"),
        )
        .unwrap();

        assert_eq!(
            targets,
            vec![
                "glibc".to_string(),
                "libcap".to_string(),
                "systemd-libs".to_string(),
            ]
        );
    }
}
