//! Shared utility functions.

use std::borrow::Cow;
use std::process::Command;

/// Expand tilde (~) to home directory in paths.
pub fn expand_tilde(path: &str) -> Cow<'_, str> {
    if path.starts_with('~') {
        if let Some(home) = std::env::var_os("HOME") {
            return Cow::Owned(path.replacen('~', &home.to_string_lossy(), 1));
        }
    }
    Cow::Borrowed(path)
}

/// Simple base64 encoding for user data.
pub fn base64_encode(input: &str) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let bytes = input.as_bytes();
    let mut result = String::new();

    for chunk in bytes.chunks(3) {
        let mut n: u32 = 0;
        for (i, &byte) in chunk.iter().enumerate() {
            n |= (byte as u32) << (16 - 8 * i);
        }

        let padding = 3 - chunk.len();
        for i in 0..(4 - padding) {
            let idx = ((n >> (18 - 6 * i)) & 0x3F) as usize;
            result.push(ALPHABET[idx] as char);
        }
        for _ in 0..padding {
            result.push('=');
        }
    }

    result
}

/// Add standard SSH options to a Command to disable host key checking.
/// These options are commonly used for cloud instances where host keys change.
pub fn add_ssh_options(cmd: &mut Command) {
    cmd.arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null");
}

/// Add SSH options plus BatchMode for non-interactive connections.
pub fn add_ssh_batch_options(cmd: &mut Command) {
    add_ssh_options(cmd);
    cmd.arg("-o").arg("BatchMode=yes");
}

/// Write a file containing a secret — a credential cache, an SSH private key —
/// creating it owner-only.
///
/// The mode is set as the file is created rather than afterwards: chmod'ing an
/// already-written file leaves a window in which it is readable by anyone the
/// umask allows.
pub fn write_secret(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;

    let dir = path.parent().context("secret path has no parent directory")?;
    std::fs::create_dir_all(dir)?;
    restrict_permissions(dir, 0o700)?;

    let mut open = std::fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }

    let mut file = open
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.write_all(contents.as_bytes())?;

    // `mode` above only applies when the file is created, so an existing file
    // keeps whatever permissions it had.
    restrict_permissions(path, 0o600)?;
    Ok(())
}

/// Restrict a path to its owner.
fn restrict_permissions(path: &std::path::Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// Point ssh/scp at exactly one private key.
///
/// `IdentitiesOnly` matters: without it ssh also offers every default identity
/// in `~/.ssh`, which muddies the error when the intended key is the problem
/// (a broken `id_rsa` reports the same "Load key" failure) and can exhaust the
/// server's MaxAuthTries before the right key is ever reached.
pub fn add_identity(cmd: &mut Command, key_path: &str) {
    cmd.arg("-i").arg(key_path).arg("-o").arg("IdentitiesOnly=yes");
}
