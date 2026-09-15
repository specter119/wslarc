/// Quote one literal argument for a POSIX shell or libalpm's command parser.
pub fn shell_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// systemd ExecStart is not a shell and expands percent/dollar sequences.
pub fn systemd_argument(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    )
}

pub fn shell_command(config_path: &str) -> String {
    format!(
        "/usr/local/bin/wslarc --config {}",
        shell_argument(config_path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_quotes_literal_config_paths() {
        assert_eq!(shell_argument("/etc/a'b"), "'/etc/a'\\''b'");
        assert_eq!(systemd_argument("/etc/a $x%f"), "\"/etc/a $$x%%f\"");
    }
}
