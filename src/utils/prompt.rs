use anyhow::Result;
use console::style;
use dialoguer::{Confirm, Input, Select};

/// Print a step header
pub fn step(num: u32, total: u32, title: &str) {
    println!(
        "\n{} {}",
        style(format!("[{}/{}]", num, total)).cyan().bold(),
        style(title).bold()
    );
}

/// Print a success message
pub fn success(msg: &str) {
    println!("  {} {}", style("✓").green(), msg);
}

/// Print an info message
pub fn info(msg: &str) {
    println!("  {} {}", style("→").blue(), msg);
}

/// Print a warning message
pub fn warn(msg: &str) {
    println!("  {} {}", style("⚠").yellow(), msg);
}

/// Ask for confirmation
pub fn confirm(msg: &str, default: bool) -> Result<bool> {
    Ok(Confirm::new()
        .with_prompt(msg)
        .default(default)
        .interact()?)
}

/// Ask for confirmation, or return true if --yes was passed
pub fn confirm_or_yes(msg: &str, default: bool, yes: bool) -> Result<bool> {
    if yes {
        Ok(true)
    } else {
        confirm(msg, default)
    }
}

/// Ask for text input with a default value
pub fn input(prompt: &str, default: &str) -> Result<String> {
    Ok(Input::new()
        .with_prompt(prompt)
        .default(default.to_string())
        .interact_text()?)
}

/// Select from a list of options
pub fn select(prompt: &str, options: &[&str], default: usize) -> Result<usize> {
    Ok(Select::new()
        .with_prompt(prompt)
        .items(options)
        .default(default)
        .interact()?)
}

/// Print a section header
pub fn section(title: &str) {
    println!("\n{}", style(title).bold().underlined());
}

/// Print a key-value pair
pub fn kv(key: &str, value: &str) {
    println!("  {}: {}", style(key).dim(), value);
}
