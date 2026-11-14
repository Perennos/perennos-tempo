//! Fixed, release-verified control binaries for an approved fleet enrollment.

use crate::setup::json::parse;
use crate::setup::registry::{Fetch, Fetched};
use crate::updates::{Version, RELEASES_URL};
use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const API: &str = "https://api.github.com/repos/Perennos/perennos-tempo/releases/tags/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub version: Version,
    pub target: String,
    pub name: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

pub fn target(os: &str, arch: &str) -> Result<&'static str, String> {
    match (os, arch) {
        ("Linux", "x86_64" | "amd64") => Ok("x86_64-unknown-linux-gnu"),
        ("Darwin", "arm64" | "aarch64") => Ok("aarch64-apple-darwin"),
        ("Windows", "x86_64" | "AMD64") => Ok("x86_64-pc-windows-msvc"),
        _ => Err(format!("Controls are not published for {os} {arch}.")),
    }
}

pub fn resolve_release(
    version: Version,
    target: &str,
    metadata: &str,
    sums: &str,
) -> Result<Artifact, String> {
    let tag = version.tag();
    let release =
        parse(metadata).map_err(|_| "The official controls release metadata was invalid.")?;
    let page = format!("{RELEASES_URL}{tag}");
    if release.str("tag_name") != Some(tag.as_str())
        || release.str("html_url") != Some(page.as_str())
        || release.bool("draft") != Some(false)
        || release.bool("prerelease") != Some(false)
    {
        return Err("GitHub did not return the exact official stable release.".into());
    }
    let numeric = format!("{}.{}.{}", version.0[0], version.0[1], version.0[2]);
    let name = format!("perennos-tempo-controls-{numeric}-{target}.tar.gz");
    let url = format!("https://github.com/Perennos/perennos-tempo/releases/download/{tag}/{name}");
    let mut matching = release
        .arr("assets")
        .iter()
        .filter(|a| a.str("name") == Some(name.as_str()));
    let asset = matching.next().ok_or("The official release has no controls bundle for this computer; no fallback installer will run.")?;
    if matching.next().is_some() || asset.str("browser_download_url") != Some(url.as_str()) {
        return Err("The controls asset name or official download URL did not match.".into());
    }
    let size = asset
        .f64("size")
        .filter(|v| *v >= 1.0 && *v <= MAX_ARCHIVE_BYTES as f64 && v.fract() == 0.0)
        .map(|v| v as u64)
        .ok_or("The controls archive size was missing or over the 64 MiB limit.")?;
    let mut digest = None;
    for line in sums.lines() {
        let mut fields = line.split_whitespace();
        let hash = fields.next().unwrap_or("");
        let file = fields.next().unwrap_or("").trim_start_matches('*');
        if file == name {
            if digest.is_some()
                || hash.len() != 64
                || !hash.bytes().all(|b| b.is_ascii_hexdigit())
                || fields.next().is_some()
            {
                return Err("SHA256SUMS repeated or malformed the controls archive entry.".into());
            }
            digest = Some(hash.to_ascii_lowercase());
        }
    }
    Ok(Artifact {
        version,
        target: target.into(),
        name,
        url,
        size,
        sha256: digest.ok_or("SHA256SUMS did not contain the exact controls archive.")?,
    })
}

/// Read only the exact release and checksum assets for the controller's package version.
pub fn release(fetch: &dyn Fetch, version: Version, target: &str) -> Result<Artifact, String> {
    let tag = version.tag();
    let metadata = body(fetch.get(
        &format!("{API}{tag}"),
        Some("Accept: application/vnd.github+json"),
        true,
    ))?;
    let parsed =
        parse(&metadata).map_err(|_| "The official controls release metadata was invalid.")?;
    let page = format!("{RELEASES_URL}{tag}");
    if parsed.str("tag_name") != Some(tag.as_str())
        || parsed.str("html_url") != Some(page.as_str())
        || parsed.bool("draft") != Some(false)
        || parsed.bool("prerelease") != Some(false)
    {
        return Err("GitHub did not return the exact official stable release.".into());
    }
    let sums_url = format!(
        "https://github.com/Perennos/perennos-tempo/releases/download/{tag}/SHA256SUMS.txt"
    );
    let mut sums_assets = parsed
        .arr("assets")
        .iter()
        .filter(|a| a.str("name") == Some("SHA256SUMS.txt"));
    let sums_asset = sums_assets
        .next()
        .ok_or("The official release has no SHA256SUMS asset.")?;
    if sums_assets.next().is_some()
        || sums_asset.str("browser_download_url") != Some(sums_url.as_str())
        || sums_asset
            .f64("size")
            .is_none_or(|n| n < 1.0 || n > (1 << 20) as f64)
    {
        return Err("The official SHA256SUMS asset facts were invalid.".into());
    }
    let sums = body(fetch.get(&sums_url, None, true))?;
    resolve_release(version, target, &metadata, &sums)
}

fn body(result: Fetched) -> Result<String, String> {
    match result {
        Fetched::Body(s) => Ok(s),
        Fetched::NotFound => Err("The official release has no controls bundle for this computer; no fallback installer will run.".into()),
        _ => Err("The official controls release could not be checked.".into()),
    }
}

/// Download once to an owned `.part` file with a hard byte and wall-clock ceiling.
pub fn download(artifact: &Artifact, part: &Path) -> Result<(), String> {
    let expected = format!("https://github.com/Perennos/perennos-tempo/releases/download/{}/perennos-tempo-controls-{}-{}.tar.gz", artifact.version.tag(), artifact.version_tag(), artifact.target);
    if artifact.size == 0
        || artifact.size > MAX_ARCHIVE_BYTES
        || artifact.url != expected
        || artifact.sha256.len() != 64
        || !artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("The approved controls download facts are invalid.".into());
    }
    let output = Command::new("curl")
        .arg("-q")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--location",
            "--max-redirs",
            "5",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "8",
            "--max-time",
            "45",
            "--max-filesize",
        ])
        .arg(artifact.size.to_string())
        .arg("--output")
        .arg(part)
        .arg(&artifact.url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Could not start the bounded controls download: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "The controls download failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("curl failed")
        ));
    }
    let meta = std::fs::metadata(part)
        .map_err(|e| format!("Could not read the downloaded controls bundle: {e}"))?;
    if meta.len() != artifact.size {
        return Err("The controls download size did not match the approved release.".into());
    }
    let hash = crate::setup::sha256::file_sha256(part, &mut |_| {})
        .map_err(|e| format!("Could not verify the controls bundle: {e}"))?;
    if hash != artifact.sha256 {
        return Err("The controls bundle did not match SHA256SUMS.".into());
    }
    Ok(())
}

pub fn inspect_bundle(artifact: &Artifact, archive: &Path) -> Result<Vec<ControlFile>, String> {
    let meta = std::fs::symlink_metadata(archive)
        .map_err(|e| format!("Could not read the controls bundle: {e}"))?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err("The controls archive must be a regular file.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err("The controls archive must not be a reparse point.".into());
        }
    }
    if meta.len() != artifact.size
        || meta.len() > MAX_ARCHIVE_BYTES
        || crate::setup::sha256::file_sha256(archive, &mut |_| {}).map_err(|e| e.to_string())?
            != artifact.sha256
    {
        return Err("The controls archive changed after its approved download.".into());
    }
    let root = format!(
        "perennos-tempo-controls-{}-{}",
        artifact.version_tag(),
        artifact.target
    );
    let mut list = Command::new(tar_program()?);
    list.args(["-tvzf"]).arg(archive);
    let (list_ok, listing) = bounded_tar(list, 256 * 1024)?;
    if !list_ok || !safe_listing(&String::from_utf8_lossy(&listing), &root, &artifact.target) {
        return Err("The controls archive contains an unexpected member or file type.".into());
    }
    let manifest_bytes = read_member(archive, &root, "CONTROLS.json", 64 * 1024)?;
    let manifest_text =
        std::str::from_utf8(&manifest_bytes).map_err(|_| "The controls manifest was not UTF-8.")?;
    let manifest = parse(manifest_text).map_err(|_| "The controls manifest was invalid.")?;
    let version = format!(
        "{}.{}.{}",
        artifact.version.0[0], artifact.version.0[1], artifact.version.0[2]
    );
    if manifest.f64("schema") != Some(1.0)
        || manifest.str("version") != Some(version.as_str())
        || manifest.str("target") != Some(artifact.target.as_str())
        || String::from_utf8(read_member(archive, &root, "VERSION", 64)?)
            .map_err(|_| "Bad VERSION file")?
            != format!("{version}\n")
        || String::from_utf8(read_member(archive, &root, "TARGET", 128)?)
            .map_err(|_| "Bad TARGET file")?
            != format!("{}\n", artifact.target)
        || String::from_utf8(read_member(archive, &root, "EDITION", 64)?)
            .map_err(|_| "Bad EDITION file")?
            != "source-available\n"
    {
        return Err(
            "The controls manifest version, target or edition did not match the approved plan."
                .into(),
        );
    }
    let expected = expected_files(&artifact.target);
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for item in manifest.arr("files") {
        let path = item
            .str("path")
            .ok_or("The controls manifest omitted a file path.")?;
        let size = item
            .f64("bytes")
            .filter(|n| *n >= 0.0 && *n <= MAX_ARCHIVE_BYTES as f64 && n.fract() == 0.0)
            .ok_or("The controls manifest had an invalid file size.")? as u64;
        let digest = item
            .str("sha256")
            .ok_or("The controls manifest omitted a file digest.")?
            .to_ascii_lowercase();
        if !expected.contains(&path)
            || !seen.insert(path.to_string())
            || digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("The controls manifest contains an unexpected or repeated file.".into());
        }
        let bytes = read_member(archive, &root, path, size as usize)?;
        if bytes.len() as u64 != size || crate::setup::sha256::sha256_hex(&bytes) != digest {
            return Err(format!(
                "A controls file failed its size or SHA-256 check: {path}"
            ));
        }
        files.push(ControlFile {
            path: path.into(),
            bytes,
            sha256: digest,
        });
    }
    if seen.len() != expected.len() {
        return Err("The controls manifest did not list every required file.".into());
    }
    Ok(files)
}

fn safe_member(name: &str) -> bool {
    matches!(
        name,
        "VERSION"
            | "TARGET"
            | "EDITION"
            | "bin/tempo-launch"
            | "bin/tempo-local-agent"
            | "bin/tempo-launch.exe"
            | "bin/tempo-local-agent.exe"
            | "legal/LICENSE-PolyForm-Shield-1.0.0.txt"
            | "legal/NOTICE.txt"
            | "legal/THIRD-PARTY-NOTICES.txt"
    ) && !name.starts_with('/')
        && !name.split('/').any(|p| p == ".." || p.is_empty())
}

fn expected_files(target: &str) -> Vec<&'static str> {
    let exe = if target.starts_with("x86_64-pc-windows-") {
        ".exe"
    } else {
        ""
    };
    vec![
        "VERSION",
        "TARGET",
        "EDITION",
        if exe.is_empty() {
            "bin/tempo-launch"
        } else {
            "bin/tempo-launch.exe"
        },
        if exe.is_empty() {
            "bin/tempo-local-agent"
        } else {
            "bin/tempo-local-agent.exe"
        },
        "legal/LICENSE-PolyForm-Shield-1.0.0.txt",
        "legal/NOTICE.txt",
        "legal/THIRD-PARTY-NOTICES.txt",
    ]
}

fn safe_listing(text: &str, root: &str, target: &str) -> bool {
    let root = root.trim_end_matches('/');
    let files = expected_files(target);
    let dirs = [
        format!("{root}/"),
        format!("{root}/bin/"),
        format!("{root}/legal/"),
    ];
    let expected: HashSet<String> = dirs
        .into_iter()
        .chain(files.into_iter().map(|p| format!("{root}/{p}")))
        .chain([format!("{root}/CONTROLS.json")])
        .collect();
    let mut seen = HashSet::new();
    text.lines().all(|line| {
        let Some(kind) = line.as_bytes().first().copied() else {
            return false;
        };
        let member = line.split_whitespace().last().unwrap_or("");
        let wanted_dir = member.ends_with('/');
        let safe_kind = if wanted_dir {
            kind == b'd'
        } else {
            kind == b'-'
        };
        safe_kind
            && expected.contains(member)
            && seen.insert(member.to_string())
            && !member.contains("..")
    }) && seen == expected
}

fn read_member(archive: &Path, root: &str, name: &str, cap: usize) -> Result<Vec<u8>, String> {
    if name != "CONTROLS.json" && !safe_member(name) {
        return Err("The controls archive path was not allowed.".into());
    }
    let member = format!("{root}/{name}");
    let mut command = Command::new(tar_program()?);
    command.arg("-xOzf").arg(archive).arg(member);
    let (ok, bytes) = bounded_tar(command, cap)?;
    if !ok || bytes.len() > cap {
        return Err("Could not read the expected regular controls file.".into());
    }
    Ok(bytes)
}

fn tar_program() -> Result<std::path::PathBuf, String> {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").ok_or("Windows SystemRoot is unavailable.")?;
        Ok(std::path::PathBuf::from(root)
            .join("System32")
            .join("tar.exe"))
    }
    #[cfg(not(windows))]
    {
        Ok(std::path::PathBuf::from("/usr/bin/tar"))
    }
}

/// The fixed USTAR bundle is read by the system tar with a wall clock and stdout cap.
fn bounded_tar(mut command: Command, cap: usize) -> Result<(bool, Vec<u8>), String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Could not start the system tar: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("The system tar had no output pipe.")?;
    let (tx, rx) = mpsc::channel();
    let cap = cap.min(MAX_ARCHIVE_BYTES as usize).saturating_add(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::with_capacity(cap.min(8192));
        let _ = std::io::Read::read_to_end(
            &mut std::io::Read::take(&mut stdout, cap as u64),
            &mut bytes,
        );
        let _ = tx.send(bytes);
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                if reader.is_finished() {
                    let _ = reader.join();
                }
                return Err("The system tar exceeded its 15-second read limit.".into());
            }
        }
    };
    let bytes = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(bytes) => bytes,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            if reader.is_finished() {
                let _ = reader.join();
            }
            return Err("The system tar output did not close before its deadline.".into());
        }
    };
    let _ = reader.join();
    if bytes.len() > cap.saturating_sub(1) {
        return Err("The controls archive member exceeded its approved size cap.".into());
    }
    Ok((status.success(), bytes))
}

impl Artifact {
    fn version_tag(&self) -> String {
        format!(
            "{}.{}.{}",
            self.version.0[0], self.version.0[1], self.version.0[2]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::json::J;

    fn release_fixture() -> (String, String) {
        let name = "perennos-tempo-controls-2.1.0-x86_64-unknown-linux-gnu.tar.gz";
        let url =
            format!("https://github.com/Perennos/perennos-tempo/releases/download/v2.1.0/{name}");
        let metadata = J::Obj(vec![
            ("tag_name".into(), J::s("v2.1.0")),
            (
                "html_url".into(),
                J::s("https://github.com/Perennos/perennos-tempo/releases/tag/v2.1.0"),
            ),
            ("draft".into(), J::Bool(false)),
            ("prerelease".into(), J::Bool(false)),
            (
                "assets".into(),
                J::Arr(vec![J::Obj(vec![
                    ("name".into(), J::s(name)),
                    ("browser_download_url".into(), J::s(&url)),
                    ("size".into(), J::n(100.0)),
                ])]),
            ),
        ])
        .pretty();
        (metadata, format!("{}  {name}\n", "a".repeat(64)))
    }

    #[test]
    fn release_requires_exact_publisher_tag_asset_size_and_checksum() {
        let (metadata, sums) = release_fixture();
        let resolve = |m: &str, s: &str| {
            resolve_release(Version([2, 1, 0]), "x86_64-unknown-linux-gnu", m, s)
        };
        assert_eq!(resolve(&metadata, &sums).unwrap().size, 100);
        for changed in [
            metadata.replace("Perennos/", "Other/"),
            metadata.replace("v2.1.0", "v2.1.1"),
            metadata.replace("100", "67108865"),
            metadata.replace("false", "true"),
        ] {
            assert!(resolve(&changed, &sums).is_err());
        }
        assert!(resolve(&metadata, "").is_err());
        assert!(resolve(&metadata, &format!("{sums}{sums}")).is_err());
    }

    #[test]
    fn listing_rejects_links_traversal_duplicates_and_missing_members() {
        let root = "perennos-tempo-controls-2.1.0-x86_64-unknown-linux-gnu";
        let mut listing = ["", "bin/", "legal/"]
            .map(|p| format!("drwx------ 0 0 0 0 {root}/{p}\n"))
            .concat();
        for file in expected_files("x86_64-unknown-linux-gnu")
            .into_iter()
            .chain(["CONTROLS.json"])
        {
            listing.push_str(&format!("-rw------- 0 0 0 0 {root}/{file}\n"));
        }
        assert!(safe_listing(&listing, root, "x86_64-unknown-linux-gnu"));
        for changed in [
            listing.replace("-rw", "lrw"),
            listing.replace("/VERSION", "/../VERSION"),
            format!("{listing}{listing}"),
            listing.replace(&format!("-rw------- 0 0 0 0 {root}/VERSION\n"), ""),
        ] {
            assert!(!safe_listing(&changed, root, "x86_64-unknown-linux-gnu"));
        }
        assert!(target("Windows", "AMD64").is_ok());
        assert!(target("Darwin", "arm64").is_ok());
        assert!(target("Linux", "aarch64").is_err());
    }

    #[test]
    fn corrupt_archive_is_refused_before_tar_or_execution() {
        let (metadata, sums) = release_fixture();
        let artifact = resolve_release(
            Version([2, 1, 0]),
            "x86_64-unknown-linux-gnu",
            &metadata,
            &sums,
        )
        .unwrap();
        let path = super::super::tests::tmp("corrupt").join("archive");
        std::fs::write(&path, b"interrupted transfer").unwrap();
        let error = inspect_bundle(&artifact, &path).unwrap_err();
        assert!(error.contains("changed"));
        assert_eq!(std::fs::read(&path).unwrap(), b"interrupted transfer");
    }
}
