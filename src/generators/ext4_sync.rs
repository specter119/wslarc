use std::process::Command;

use anyhow::Result;

use crate::config::Config;
use crate::generators::systemd::path_to_unit_name;

pub const SYSTEMD_PACKAGES: [&str; 3] = ["systemd", "systemd-libs", "systemd-sysvcompat"];

/// Get ext4 root UUID dynamically
pub fn get_ext4_root_uuid() -> Option<String> {
    Command::new("findmnt")
        .args(["/", "-o", "UUID", "-n"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
    let mut targets = std::collections::HashSet::new();

    for pkg in SYSTEMD_PACKAGES {
        match Command::new("pacman").args(["-Qi", pkg]).output() {
            Ok(output) if output.status.success() => {
                let output = String::from_utf8_lossy(&output.stdout);
                let deps = parse_pacman_depends(&output);
                targets.insert(pkg.to_string());
                targets.extend(deps);
            }
            _ => {
                targets.insert(pkg.to_string());
            }
        }
    }

    let mut list: Vec<String> = targets.into_iter().collect();
    list.sort();
    Ok(list)
}

fn parse_pacman_depends(output: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let mut in_depends = false;

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("Depends On") {
            in_depends = true;
            if let Some(after_colon) = rest.splitn(2, ':').nth(1) {
                push_dep_tokens(after_colon, &mut deps);
            }
            continue;
        }

        if in_depends {
            if line.starts_with(' ') || line.starts_with('\t') {
                push_dep_tokens(line, &mut deps);
            } else {
                in_depends = false;
            }
        }
    }

    deps
}

fn push_dep_tokens(line: &str, deps: &mut Vec<String>) {
    for token in line.split_whitespace() {
        if token == "None" {
            continue;
        }
        let name = token
            .split(|c| c == '<' || c == '>' || c == '=')
            .next()
            .unwrap_or("")
            .trim();
        if !name.is_empty() {
            deps.push(name.to_string());
        }
    }
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
    fn test_parse_pacman_depends_strips_version_constraints() {
        let output = "\
Depends On      : glibc  libcap>=2.0  sh\n\
Optional Deps   : None\n";

        let deps = parse_pacman_depends(output);

        assert_eq!(deps, vec!["glibc", "libcap", "sh"]);
    }
}
