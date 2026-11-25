//! WSL distros on this Windows computer, offered as hosts.
//!
//! A distro is reached with `wsl.exe -d <distro> -- <command>` (see
//! [`super::Remote`]), which runs the command through the distro's login
//! shell; its projects have Linux paths and its `orbitd` is a Linux build.

/// The installed distros, from `wsl.exe -l -q`. Empty when WSL is not
/// installed (or not on Windows). Docker Desktop's internal distros are
/// left out: nothing runs in them.
pub fn list(tools: &super::Tools) -> Vec<String> {
    if !cfg!(windows) && std::env::var_os("ORBIT_WSL").is_none() {
        return Vec::new();
    }
    let Some((program, args)) = tools.wsl.split_first() else { return Vec::new() };
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).args(["-l", "-q"]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    super::no_window(&mut cmd);
    match cmd.output() {
        Ok(output) if output.status.success() => parse_list(&output.stdout),
        _ => Vec::new(),
    }
}

/// Distro names from `wsl.exe -l -q` output (UTF-16LE, one per line).
pub fn parse_list(output: &[u8]) -> Vec<String> {
    super::decode_output(output)
        .lines()
        .map(|l| l.trim().trim_matches('\0').trim())
        .filter(|l| !l.is_empty())
        .filter(|l| !l.starts_with("docker-desktop"))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_read_from_utf16() {
        let text = "\u{feff}Ubuntu-24.04\r\ndocker-desktop\r\npodman-machine-default\r\n\r\n";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(parse_list(&bytes), ["Ubuntu-24.04", "podman-machine-default"]);
        assert!(parse_list(&[]).is_empty());
    }
}
