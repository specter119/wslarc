use anyhow::{bail, Context, Result};
use console::style;
use log::{debug, trace};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

pub fn run(cmd: &str, args: &[&str]) -> Result<String> {
    debug!("Executing: {} {}", cmd, args.join(" "));

    let output = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("Failed to execute: {} {}", cmd, args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Command failed: {} {}\n{}",
            cmd,
            args.join(" "),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    trace!("Output: {}", stdout);
    Ok(stdout)
}

pub fn run_with_output(cmd: &str, args: &[&str]) -> Result<()> {
    debug!("Executing (streaming): {} {}", cmd, args.join(" "));

    let mut child = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to spawn: {} {}", cmd, args.join(" ")))?;

    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            println!("  {}", line);
        }
    }

    let status = child.wait()?;
    if !status.success() {
        bail!("Command failed: {} {}", cmd, args.join(" "));
    }

    Ok(())
}

pub fn run_or_dry(cmd: &str, args: &[&str], dry_run: bool) -> Result<String> {
    if dry_run {
        println!(
            "  {} {} {}",
            style("[dry-run]").yellow(),
            cmd,
            args.join(" ")
        );
        Ok(String::new())
    } else {
        run(cmd, args)
    }
}
