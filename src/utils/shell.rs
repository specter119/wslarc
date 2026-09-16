use anyhow::{bail, Context, Result};
use console::style;
use log::{debug, trace};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;

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
    run_with_output_inner(cmd, args, false)
}

pub fn run_with_output_interruptible(cmd: &str, args: &[&str]) -> Result<()> {
    #[cfg(unix)]
    install_ctrl_c_handler()?;
    run_with_output_inner(cmd, args, true)
}

fn run_with_output_inner(cmd: &str, args: &[&str], reset_ctrl_c: bool) -> Result<()> {
    debug!("Executing (streaming): {} {}", cmd, args.join(" "));

    let mut command = Command::new(cmd);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    if reset_ctrl_c {
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("Failed to spawn: {} {}", cmd, args.join(" ")))?;

    let stdout_handle = child.stdout.take().map(|stdout| {
        thread::spawn(move || {
            let mut reader = stdout;
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let mut output = std::io::stdout().lock();
                        let _ = output.write_all(&buffer[..count]);
                        let _ = output.flush();
                    }
                }
            }
        })
    });

    let stderr_handle = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut reader = stderr;
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let mut output = std::io::stderr().lock();
                        let _ = output.write_all(&buffer[..count]);
                        let _ = output.flush();
                    }
                }
            }
        })
    });

    let status = child.wait()?;
    if let Some(handle) = stdout_handle {
        let _ = handle.join();
    }
    if let Some(handle) = stderr_handle {
        let _ = handle.join();
    }
    if !status.success() {
        bail!("Command failed: {} {}", cmd, args.join(" "));
    }

    Ok(())
}

#[cfg(unix)]
fn install_ctrl_c_handler() -> Result<()> {
    static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();

    INSTALLED
        .get_or_init(|| ctrlc::set_handler(|| {}).map_err(|error| error.to_string()))
        .as_ref()
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("Failed to install Ctrl-C handler: {error}"))
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
