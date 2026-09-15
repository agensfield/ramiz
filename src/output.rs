use serde::Serialize;

use std::path::Path;

use crate::error::ErrorData;

#[derive(Serialize)]
pub struct EncodedPath {
    pub display: String,
    pub bytes_base64: String,
}

impl EncodedPath {
    pub fn new(path: &Path) -> Self {
        #[cfg(unix)]
        let bytes = {
            use std::os::unix::ffi::OsStrExt;
            path.as_os_str().as_bytes()
        };
        #[cfg(not(unix))]
        let bytes = path.to_string_lossy().as_bytes();
        Self {
            display: path.to_string_lossy().into_owned(),
            bytes_base64: base64(bytes),
        }
    }
}

#[derive(Serialize)]
struct Success<'a, T> {
    schema: &'static str,
    ok: bool,
    command: &'a str,
    data: T,
    warnings: &'a [String],
}

#[derive(Serialize)]
struct Failure<'a> {
    schema: &'static str,
    ok: bool,
    command: &'a str,
    error: &'a ErrorData,
}

pub fn success<T: Serialize>(command: &str, data: T, warnings: &[String]) {
    let envelope = Success {
        schema: "ramiz.cli/v1",
        ok: true,
        command,
        data,
        warnings,
    };
    println!(
        "{}",
        serde_json::to_string(&envelope).expect("serializing a Ramiz envelope cannot fail")
    );
}

pub fn failure(command: &str, error: &ErrorData) {
    let envelope = Failure {
        schema: "ramiz.cli/v1",
        ok: false,
        command,
        error,
    };
    println!(
        "{}",
        serde_json::to_string(&envelope).expect("serializing a Ramiz error cannot fail")
    );
}

pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        encoded.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        encoded.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((value >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(value & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    encoded
}
