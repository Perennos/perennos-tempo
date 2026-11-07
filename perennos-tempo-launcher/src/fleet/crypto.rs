//! The small pieces the fleet messenger needs: HMAC-SHA256 over the launcher's own SHA-256,
//! hex, random bytes from the operating system, and a file only its owner can read.

use crate::setup::sha256::Sha256;
use std::io;
use std::path::Path;

/// HMAC-SHA256 (RFC 2104) of the parts joined, without joining them.
pub fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let mut h = Sha256::new();
        h.update(key);
        k[..32].copy_from_slice(&h.finish());
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(&k.map(|b| b ^ 0x36));
    for p in parts {
        inner.update(p);
    }
    let mut outer = Sha256::new();
    outer.update(&k.map(|b| b ^ 0x5c));
    outer.update(&inner.finish());
    outer.finish()
}

/// Equal without stopping at the first difference.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

pub fn random(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    // the system's source failing means nothing safe can be made: stop rather than guess
    getrandom::getrandom(&mut v).expect("the operating system has no random source");
    v
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Write a file that only the current user can read: mode 0600 on Unix, and on Windows the
/// inherited permissions removed and the user alone granted.
pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("key"),
        std::process::id()
    ));
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        io::Write::write_all(&mut o.open(&tmp)?, data)?;
    }
    #[cfg(windows)]
    lock_down(&tmp);
    std::fs::rename(&tmp, path)
}

/// Windows: an ACL, as `icacls` sets it (best effort: the file sits in the user's own folder).
#[cfg(windows)]
fn lock_down(path: &Path) {
    use std::os::windows::process::CommandExt;
    if let Some(user) = std::env::var_os("USERNAME") {
        let _ = std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{}:F", user.to_string_lossy()))
            .creation_flags(0x0800_0000)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231() {
        let k = [0x0bu8; 20];
        assert_eq!(
            hex(&hmac(&k, &[b"Hi ", b"There"])),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // a key longer than the block is hashed first (test case 6)
        let long = [0xaau8; 131];
        assert_eq!(
            hex(&hmac(
                &long,
                &[b"Test Using Larger Than Block-Size Key - Hash Key First"]
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn hex_round_trips_and_refuses_junk() {
        assert_eq!(unhex(&hex(&[0, 255, 16])).unwrap(), [0, 255, 16]);
        assert!(unhex("abc").is_none() && unhex("zz").is_none());
        assert!(same(b"ab", b"ab") && !same(b"ab", b"ac") && !same(b"a", b"ab"));
        assert_ne!(random(32), random(32));
    }
}
