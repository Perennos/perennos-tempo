//! Shared helpers: dB conversions, path resolution, output naming that never overwrites,
//! and the allow-root guard used by the MCP server.
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};

pub fn db_to_amp(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Linear amplitude to dB; `None` for silence (or anything that is not a positive finite number).
pub fn amp_to_db(a: f64) -> Option<f64> {
    (a > 0.0 && a.is_finite()).then(|| 20.0 * a.log10())
}

/// Round to 2 decimals for reports (and never print "-0").
pub fn r2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0 + 0.0
}

/// Optional value rounded to 2 decimals, as JSON (`null` when absent).
pub fn jnum(x: Option<f64>) -> serde_json::Value {
    match x {
        Some(v) if v.is_finite() => serde_json::json!(r2(v)),
        _ => serde_json::Value::Null,
    }
}

/// "-14.02" or "-inf" for text output.
pub fn fmt_db(x: Option<f64>) -> String {
    match x {
        Some(v) => format!("{v:.2}"),
        None => "-inf".into(),
    }
}

/// Windows `canonicalize` gives verbatim paths (`\\?\D:\x`, `\\?\UNC\server\share`), which
/// other programs (REAPER among them) may not take: turn them back into plain paths.
pub fn strip_verbatim(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
        _ => s.to_string(),
    }
}

/// Absolute, lexically normalised, with symlinks resolved on the deepest existing ancestor.
/// On Windows the result is a plain (not verbatim) path.
pub fn resolve(path: &Path) -> PathBuf {
    let p = resolve_raw(path);
    if cfg!(windows) {
        PathBuf::from(strip_verbatim(&p.to_string_lossy()))
    } else {
        p
    }
}

fn resolve_raw(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut norm = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut base = norm.clone();
    loop {
        if let Ok(c) = base.canonicalize() {
            let mut out = c;
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (
            base.file_name().map(|n| n.to_os_string()),
            base.parent().map(|p| p.to_path_buf()),
        ) {
            (Some(n), Some(p)) => {
                tail.push(n);
                base = p;
            }
            _ => return norm,
        }
    }
}

/// A symlink, or on Windows any reparse point (junction, mount point).
fn is_link(md: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if md.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    md.file_type().is_symlink()
}

/// Path compared the way the OS does (case-insensitive on Windows).
fn cmp_key(p: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(p.to_string_lossy().to_lowercase())
    } else {
        p.to_path_buf()
    }
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    cmp_key(&resolve(a)) == cmp_key(&resolve(b))
}

/// `<stem>-<n>.<ext>` for n = 2, 3, ...: the first name that does not exist yet.
pub fn numbered(want: &Path, n: u32) -> PathBuf {
    let stem = want
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into());
    let name = match want.extension() {
        Some(e) => format!("{stem}-{n}.{}", e.to_string_lossy()),
        None => format!("{stem}-{n}"),
    };
    want.with_file_name(name)
}

/// `want` if nothing is there yet, else the first free numbered name.
pub fn free_path(want: &Path) -> Result<PathBuf, String> {
    if !exists_any(want) {
        return Ok(want.to_path_buf());
    }
    for n in 2..10_000 {
        let p = numbered(want, n);
        if !exists_any(&p) {
            return Ok(p);
        }
    }
    Err(format!("no free output name next to {}", want.display()))
}

/// True for files, folders and dangling symlinks alike.
fn exists_any(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// An output reserved without ever overwriting: data goes to `<name>.partial` (a brand-new file)
/// and is renamed into place by `commit`. A failed run leaves only the `.partial` file; nothing is
/// ever deleted.
#[derive(Debug)]
pub struct OutFile {
    pub path: PathBuf,
    partial: PathBuf,
    force: bool,
}

impl OutFile {
    /// `want` if it is free (or `force` is set), else the first free numbered name. Refuses an
    /// output that is one of `inputs`, even with `force`.
    pub fn create(want: &Path, force: bool, inputs: &[PathBuf]) -> Result<(OutFile, File), String> {
        if want.as_os_str().is_empty() {
            return Err("empty output path".into());
        }
        for i in inputs {
            if same_path(want, i) {
                return Err(format!(
                    "refusing to write the output over an input file ({}); choose another --out",
                    want.display()
                ));
            }
        }
        if want.is_dir() {
            return Err(format!(
                "output {} is a folder; give a file name",
                want.display()
            ));
        }
        if let Some(dir) = want.parent().filter(|d| !d.as_os_str().is_empty()) {
            if !dir.is_dir() {
                return Err(format!("output folder {} does not exist", dir.display()));
            }
        }
        let path = if force {
            want.to_path_buf()
        } else {
            free_path(want)?
        };
        let base = format!("{}.partial", path.to_string_lossy());
        let mut last = String::new();
        for n in 1..1000 {
            let partial = if n == 1 {
                PathBuf::from(&base)
            } else {
                PathBuf::from(format!("{base}-{n}"))
            };
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial)
            {
                Ok(f) => {
                    return Ok((
                        OutFile {
                            path,
                            partial,
                            force,
                        },
                        f,
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    last = e.to_string();
                    break;
                }
            }
        }
        Err(format!("cannot create {}.partial: {last}", path.display()))
    }

    pub fn partial(&self) -> &Path {
        &self.partial
    }

    /// Move the finished `.partial` into place. Without `force`, a name taken in the meantime
    /// moves the output on to the next free numbered name.
    pub fn commit(self) -> Result<PathBuf, String> {
        let target = if self.force {
            // never overwritten: the old file goes to the trash folder first
            if self.path.exists() {
                trash(&self.path)?;
            }
            self.path.clone()
        } else {
            free_path(&self.path)?
        };
        std::fs::rename(&self.partial, &target).map_err(|e| {
            format!(
                "could not move the finished output into place at {} ({e}); it is complete at {}",
                target.display(),
                self.partial.display()
            )
        })?;
        Ok(target)
    }

    /// Text added to an error when the run failed after the `.partial` was created.
    pub fn left_note(&self) -> String {
        format!(
            "(the unfinished output is left at {})",
            self.partial.display()
        )
    }
}

/// Today's date (UTC) as YYYYMMDD.
fn today_ymd() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    // days to civil date (Howard Hinnant's algorithm)
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}")
}

/// Move a file into `<its folder>/.tempo-trash/<YYYYMMDD>/` under a free name (never deleted).
pub fn trash(p: &Path) -> Result<PathBuf, String> {
    let dir = p
        .parent()
        .unwrap_or(Path::new("."))
        .join(".tempo-trash")
        .join(today_ymd());
    // a link in place of the trash folder would carry the file out of the folder it is in
    for d in [dir.parent().unwrap_or(&dir), &dir] {
        if std::fs::symlink_metadata(d).is_ok_and(|m| is_link(&m)) {
            return Err(format!(
                "{} is a link; the old output was not moved and nothing was overwritten",
                d.display()
            ));
        }
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot make the trash folder {}: {e}", dir.display()))?;
    let name = p
        .file_name()
        .ok_or_else(|| format!("{} has no file name", p.display()))?;
    let dest = free_path(&dir.join(name))?;
    std::fs::rename(p, &dest).map_err(|e| {
        format!(
            "cannot move {} to the trash folder ({e}); nothing was overwritten",
            p.display()
        )
    })?;
    Ok(dest)
}

/// A file name Windows would treat as a device (`CON`, `NUL.txt`, `COM1`...), an alternate data
/// stream (`a.wav:x`) or would silently rename (a trailing dot or space).
pub fn windows_unsafe_name(name: &str) -> bool {
    if name.contains(':') || name.ends_with(['.', ' ']) {
        return true;
    }
    let up = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    matches!(
        up.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (up.chars().count() == 4
        && (up.starts_with("COM") || up.starts_with("LPT"))
        && up
            .chars()
            .last()
            .is_some_and(|d| matches!(d, '1'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}')))
}

/// Folders an MCP caller may touch. Every path must be absolute and inside one of them.
#[derive(Clone, Debug, Default)]
pub struct Roots {
    pub roots: Vec<PathBuf>,
}

impl Roots {
    pub fn new(roots: &[PathBuf]) -> Roots {
        Roots {
            roots: roots.iter().map(|r| resolve(r)).collect(),
        }
    }

    /// The resolved path, or why it is refused.
    pub fn check(&self, p: &str) -> Result<PathBuf, String> {
        let raw = Path::new(p);
        if p.trim().is_empty() {
            return Err("empty path".into());
        }
        if p.chars().any(char::is_control) {
            return Err("a path cannot hold control characters".into());
        }
        if !raw.is_absolute() {
            return Err(format!("path must be absolute: {p}"));
        }
        if cfg!(windows)
            && raw.components().any(
                |c| matches!(c, Component::Normal(n) if windows_unsafe_name(&n.to_string_lossy())),
            )
        {
            return Err(format!(
                "{p} holds a name Windows treats as a device or stream (CON, NUL, COM1, a:b ...)"
            ));
        }
        let r = resolve(raw);
        let key = cmp_key(&r);
        if self.roots.iter().any(|root| key.starts_with(cmp_key(root))) {
            Ok(r)
        } else {
            Err(format!(
                "path {} is outside the allowed folders ({}); start the server with --allow-root",
                r.display(),
                self.roots
                    .iter()
                    .map(|x| x.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    }
}

#[cfg(test)]
pub mod testutil {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// A fresh scratch dir under the OS temp dir (never removed; the OS cleans its temp dir).
    pub fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tstudio-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        // Canonical, so paths compare equal to resolved ones (macOS /tmp is a symlink).
        std::fs::canonicalize(&p).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::tmpdir;
    use super::*;
    use std::io::Write;

    #[test]
    fn db_helpers() {
        assert!((db_to_amp(-6.0) - 0.501187).abs() < 1e-6);
        assert!((amp_to_db(0.5).unwrap() + 6.0206).abs() < 1e-4);
        assert_eq!(amp_to_db(0.0), None);
        assert_eq!(amp_to_db(f64::NAN), None);
        assert_eq!(fmt_db(None), "-inf");
        assert_eq!(fmt_db(Some(-14.004)), "-14.00");
        assert_eq!(jnum(Some(-14.006)), serde_json::json!(-14.01));
        assert_eq!(jnum(None), serde_json::Value::Null);
        assert_eq!(r2(-0.001).to_string(), "0");
    }

    #[test]
    fn verbatim_windows_paths_are_made_plain() {
        assert_eq!(strip_verbatim(r"\\?\D:\Music\a.wav"), r"D:\Music\a.wav");
        assert_eq!(
            strip_verbatim(r"\\?\UNC\nas\share\a.wav"),
            r"\\nas\share\a.wav"
        );
        assert_eq!(strip_verbatim(r"\\?\Volume{x}\a"), r"\\?\Volume{x}\a");
        assert_eq!(strip_verbatim(r"D:\plain"), r"D:\plain");
        assert_eq!(strip_verbatim("/home/x/a.wav"), "/home/x/a.wav");
    }

    #[test]
    fn free_names_never_reuse_existing_files() {
        let d = tmpdir("free");
        let want = d.join("song-master.wav");
        assert_eq!(free_path(&want).unwrap(), want);
        std::fs::write(&want, b"x").unwrap();
        assert_eq!(free_path(&want).unwrap(), d.join("song-master-2.wav"));
        std::fs::write(d.join("song-master-2.wav"), b"x").unwrap();
        assert_eq!(free_path(&want).unwrap(), d.join("song-master-3.wav"));
        assert_eq!(
            numbered(Path::new("/x/noext"), 4),
            PathBuf::from("/x/noext-4")
        );
    }

    #[test]
    fn outfile_picks_free_name_force_replaces_and_inputs_are_refused() {
        let d = tmpdir("out");
        let want = d.join("mix.wav");
        std::fs::write(&want, b"old").unwrap();
        // Without force: a numbered name, the old file untouched.
        let (o, mut f) = OutFile::create(&want, false, &[]).unwrap();
        f.write_all(b"new").unwrap();
        drop(f);
        let got = o.commit().unwrap();
        assert_eq!(got, d.join("mix-2.wav"));
        assert_eq!(std::fs::read(&want).unwrap(), b"old");
        assert_eq!(std::fs::read(&got).unwrap(), b"new");
        // With force: replaced, and the old file kept in the trash folder (never deleted).
        let (o, mut f) = OutFile::create(&want, true, &[]).unwrap();
        f.write_all(b"forced").unwrap();
        drop(f);
        assert_eq!(o.commit().unwrap(), want);
        assert_eq!(std::fs::read(&want).unwrap(), b"forced");
        let day = std::fs::read_dir(d.join(".tempo-trash"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(day.file_name().unwrap().len(), 8, "a YYYYMMDD folder");
        assert_eq!(std::fs::read(day.join("mix.wav")).unwrap(), b"old");
        // Never over an input, even with force (also through a `..` spelling).
        let input = d.join("in.wav");
        std::fs::write(&input, b"in").unwrap();
        let sneaky = d.join("sub").join("..").join("in.wav");
        for force in [false, true] {
            let e = OutFile::create(&sneaky, force, std::slice::from_ref(&input)).unwrap_err();
            assert!(e.contains("over an input"), "{e}");
        }
        assert_eq!(std::fs::read(&input).unwrap(), b"in");
        // A missing folder or a folder as the output is a clear error.
        assert!(OutFile::create(&d.join("nope").join("x.wav"), false, &[])
            .unwrap_err()
            .contains("does not exist"));
        assert!(OutFile::create(&d, false, &[])
            .unwrap_err()
            .contains("is a folder"));
    }

    #[test]
    fn failed_run_leaves_partial_and_a_second_run_gets_its_own_partial() {
        let d = tmpdir("partial");
        let want = d.join("a.wav");
        let (o1, _f1) = OutFile::create(&want, false, &[]).unwrap();
        assert!(o1.left_note().contains("a.wav.partial"));
        // o1 is never committed (a failed run): its partial stays, a new run does not clash.
        let (o2, _f2) = OutFile::create(&want, false, &[]).unwrap();
        assert_ne!(o1.partial(), o2.partial());
        assert!(o1.partial().exists() && o2.partial().exists());
        // A name taken while we worked moves the commit on to a free name.
        std::fs::write(&want, b"someone else").unwrap();
        let got = o2.commit().unwrap();
        assert_eq!(got, d.join("a-2.wav"));
        assert_eq!(std::fs::read(&want).unwrap(), b"someone else");
    }

    #[test]
    fn roots_guard_needs_absolute_paths_inside_roots() {
        let d = tmpdir("roots");
        let inside = d.join("in");
        std::fs::create_dir_all(&inside).unwrap();
        let g = Roots::new(std::slice::from_ref(&inside));
        assert!(g.check(&inside.join("a.wav").to_string_lossy()).is_ok());
        assert!(g.check(&inside.to_string_lossy()).is_ok());
        let e = g.check("relative/a.wav").unwrap_err();
        assert!(e.contains("absolute"), "{e}");
        let e = g.check(&d.join("other.wav").to_string_lossy()).unwrap_err();
        assert!(e.contains("outside the allowed folders"), "{e}");
        // `..` cannot climb out, and a sibling with the same prefix is not inside.
        let climb = inside.join("..").join("x.wav");
        assert!(g.check(&climb.to_string_lossy()).is_err());
        let sibling = PathBuf::from(format!("{}2", inside.display())).join("x.wav");
        assert!(g.check(&sibling.to_string_lossy()).is_err());
        assert!(g.check("").is_err());
    }
}
