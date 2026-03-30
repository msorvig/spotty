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
