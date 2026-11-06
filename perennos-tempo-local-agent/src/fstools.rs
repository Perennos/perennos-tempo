//! Precise file tools: edit_file, grep, glob. Offline, never deletes (backups are copies into the trash dir).
use crate::util::{self, glob_in, glob_match, is_secret_path, sort_newest, walk_files, Guard};
use regex::RegexBuilder;
use std::path::{Path, PathBuf};

/// Exact string replacement. Unique match unless `replace_all`; keeps CRLF and BOM; refuses non-UTF-8;
/// copies the original into `<trash>/<YYYYMMDD>/tempo-agent-edits/` first (never overwriting an older backup).
pub fn edit_file(
    guard: &Guard,
    trash: &Path,
    path: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> String {
    if is_secret_path(path) {
        return "refused: secret path".into();
    }
    if !guard.in_roots(path) {
        return "refused: outside allowed write folders".into();
    }
    if !Path::new(path).is_file() {
        return format!("error: no such file: {path}");
    }
    if old.is_empty() {
        return "error: old must not be empty".into();
    }
    if old == new {
        return "error: old and new are identical".into();
    }
    let Ok(raw) = std::fs::read(path) else {
        return format!("error: cannot read {path}");
    };
    let bom = raw.starts_with(&[0xef, 0xbb, 0xbf]);
    let Ok(text) = String::from_utf8(if bom { raw[3..].to_vec() } else { raw.clone() }) else {
        return "error: file is not UTF-8; refusing to edit".into();
    };
    let (mut old, mut new) = (old.to_string(), new.to_string());
    if text.contains("\r\n") {
        // file is CRLF: model text uses \n
        let crlf = |s: &str| s.replace("\r\n", "\n").replace('\n', "\r\n");
        old = crlf(&old);
        new = crlf(&new);
    }
    let n = text.matches(old.as_str()).count();
    if n == 0 {
        return "error: old string not found (match is exact, whitespace included); read the file again".into();
    }
    if n > 1 && !replace_all {
        return format!("error: old string appears {n} times; add surrounding lines to make it unique or set replace_all=true");
    }
    let out = if replace_all {
        text.replace(&old, &new)
    } else {
        text.replacen(&old, &new, 1)
    };
    let bdir = trash.join(util::date_ymd()).join("tempo-agent-edits");
    if let Err(e) = std::fs::create_dir_all(&bdir) {
        return format!("error: cannot create backup folder: {e}");
    }
    let base = Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let mut bak = bdir.join(format!("{base}.{}.bak", util::time_hms()));
    let mut i = 1;
    while bak.exists() {
        bak = bdir.join(format!("{base}.{}.{i}.bak", util::time_hms()));
        i += 1;
    }
    if let Err(e) = std::fs::copy(path, &bak) {
        return format!("error: backup failed, file not changed: {e}");
    }
    let mut bytes = if bom { vec![0xef, 0xbb, 0xbf] } else { vec![] };
    bytes.extend_from_slice(out.as_bytes());
    if let Err(e) = std::fs::write(path, bytes) {
        return format!("error: write failed: {e}");
    }
    format!(
        "edited {path}: {} replacement(s); backup {}",
        if replace_all { n } else { 1 },
        bak.display()
    )
}

fn files_of(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let mut v = Vec::new();
    walk_files(path, &mut v, 0);
    v.sort();
    v
}

pub fn grep(
    pattern: &str,
    path: &str,
    glob: &str,
    ignore_case: bool,
    context: usize,
    max_results: usize,
) -> String {
    if is_secret_path(path) {
        return "refused: secret path".into();
    }
    let rx = match RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
    {
        Ok(r) => r,
        Err(e) => return format!("error: bad regex: {e}"),
    };
    let max_results = max_results.max(1);
    let (mut out, mut hits) = (Vec::<String>::new(), 0usize);
    for fp in files_of(Path::new(path)) {
        let name = fp
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let fps = fp.to_string_lossy().into_owned();
        if (!glob.is_empty() && !glob_match(glob, &name)) || is_secret_path(&fps) {
            continue;
        }
        if std::fs::metadata(&fp)
            .map(|m| m.len() > 2_000_000)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(raw) = std::fs::read(&fp) else {
            continue;
        };
        if raw.iter().take(8192).any(|b| *b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        for (i, ln) in lines.iter().enumerate() {
            if rx.is_match(ln) {
                for (j, l) in lines
                    .iter()
                    .enumerate()
                    .take((i + context + 1).min(lines.len()))
                    .skip(i.saturating_sub(context))
                {
                    out.push(format!(
                        "{fps}:{}{} {}",
                        j + 1,
                        if j == i { ":" } else { "-" },
                        util::head_chars(l, 300)
                    ));
                }
                if context > 0 {
                    out.push("--".into());
                }
                hits += 1;
                if hits >= max_results {
                    return format!("{}\n...[stopped at {max_results} matches]", out.join("\n"));
                }
            }
        }
    }
    if out.is_empty() {
        "no matches".into()
    } else {
        out.join("\n")
    }
}

pub fn glob(pattern: &str, path: &str) -> String {
    if is_secret_path(path) {
        return "refused: secret path".into();
    }
    let mut found: Vec<PathBuf> = glob_in(Path::new(path), pattern)
        .into_iter()
        .filter(|p| !is_secret_path(&p.to_string_lossy()))
        .collect();
    sort_newest(&mut found);
    if found.is_empty() {
        return "no matches".into();
    }
    let more = found.len().saturating_sub(100);
    let mut s = found
        .iter()
        .take(100)
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    if more > 0 {
        s.push_str(&format!("\n...[{more} more]"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::testutil::tmpdir;

    struct T {
        d: PathBuf,
        trash: PathBuf,
        g: Guard,
    }

    fn t() -> T {
        let d = tmpdir("fs");
        T {
            g: Guard::new(&[d.to_string_lossy().into_owned()]),
            trash: tmpdir("fstrash"),
            d,
        }
    }

    impl T {
        fn w(&self, name: &str, data: &[u8]) -> String {
            let p = self.d.join(name);
            std::fs::write(&p, data).unwrap();
            p.to_string_lossy().into_owned()
        }
        fn ed(&self, p: &str, o: &str, n: &str, all: bool) -> String {
            edit_file(&self.g, &self.trash, p, o, n, all)
        }
    }

    fn baks(dir: &Path, prefix: &str) -> usize {
        let mut v = Vec::new();
        walk_files(dir, &mut v, 0);
        v.iter()
            .filter(|p| {
                p.file_name().unwrap().to_string_lossy().starts_with(prefix)
                    && p.extension().is_some_and(|e| e == "bak")
            })
            .count()
    }

    #[test]
    fn unique_edit_and_backup() {
        let t = t();
        let p = t.w("a.txt", b"one\ntwo\nthree\n");
        assert!(t.ed(&p, "two", "2", false).starts_with("edited"));
        assert_eq!(std::fs::read(&p).unwrap(), b"one\n2\nthree\n");
        assert_eq!(baks(&t.trash, "a.txt."), 1);
        // a second edit in the same second does not overwrite the first backup
        t.ed(&p, "one", "1", false);
        assert_eq!(baks(&t.trash, "a.txt."), 2);
    }

    #[test]
    fn missing_old_fails() {
        let t = t();
        let p = t.w("a.txt", b"one\n");
        assert!(t.ed(&p, "zzz", "y", false).contains("not found"));
        assert_eq!(std::fs::read(&p).unwrap(), b"one\n");
    }

    #[test]
    fn duplicate_fails_without_replace_all_and_replace_all_works() {
        let t = t();
        let p = t.w("a.txt", b"x x\n");
        assert!(t.ed(&p, "x", "y", false).contains("2 times"));
        assert_eq!(std::fs::read(&p).unwrap(), b"x x\n");
        assert!(t.ed(&p, "x", "y", true).contains("2 replacement"));
        assert_eq!(std::fs::read(&p).unwrap(), b"y y\n");
    }

    #[test]
    fn crlf_kept() {
        let t = t();
        let p = t.w("a.txt", b"a\r\nb\r\nc\r\n");
        t.ed(&p, "b\nc", "B\nC\nD", false);
        assert_eq!(std::fs::read(&p).unwrap(), b"a\r\nB\r\nC\r\nD\r\n");
    }

    #[test]
    fn bom_kept_and_non_utf8_refused() {
        let t = t();
        let p = t.w("b.txt", b"\xef\xbb\xbfhello world");
        t.ed(&p, "world", "there", false);
        assert_eq!(std::fs::read(&p).unwrap(), b"\xef\xbb\xbfhello there");
        let q = t.w("c.bin", b"\xff\xfe\x00bad");
        assert!(t.ed(&q, "bad", "x", false).contains("not UTF-8"));
        assert_eq!(std::fs::read(&q).unwrap(), b"\xff\xfe\x00bad");
    }

    #[test]
    fn guards_and_arg_checks() {
        let t = t();
        let out = tmpdir("outside").join("o.txt");
        std::fs::write(&out, "x").unwrap();
        assert!(t
            .ed(out.to_str().unwrap(), "x", "y", false)
            .contains("refused"));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "x");
        let p = t.w("a.txt", b"x");
        assert!(t.ed(&p, "", "y", false).contains("must not be empty"));
        assert!(t.ed(&p, "x", "x", false).contains("identical"));
        assert!(t
            .ed(&format!("{}/nope.txt", t.d.display()), "x", "y", false)
            .contains("no such file"));
        let s = t.w("my.env", b"KEY=1");
        assert!(t.ed(&s, "1", "2", false).contains("secret"));
    }

    #[test]
    fn grep_context_binary_glob_filter_and_limit() {
        let t = t();
        t.w("a.py", b"l1\nl2\nNEEDLE here\nl4\n");
        t.w("b.bin", b"\x00\x01NEEDLE\x00");
        t.w("c.txt", b"NEEDLE in text\n");
        let d = t.d.to_str().unwrap();
        let r = grep("needle", d, "*.py", true, 1, 50);
        assert!(r.contains("a.py:3: NEEDLE here"), "{r}");
        assert!(r.contains("a.py:2- l2") && r.contains("a.py:4- l4"));
        assert!(!r.contains("c.txt"));
        let all = grep("NEEDLE", d, "", false, 0, 50);
        assert!(all.contains("c.txt") && !all.contains("b.bin"));
        assert_eq!(grep("ZZZ", d, "", false, 0, 50), "no matches");
        assert!(grep("(", d, "", false, 0, 5).contains("bad regex"));
        let lim = grep("NEEDLE", d, "", false, 0, 1);
        assert!(lim.contains("stopped at 1 matches"));
        assert!(grep("x", "/tmp/.env", "", false, 0, 1).contains("secret"));
    }

    #[test]
    fn grep_skips_git_and_big_files() {
        let t = t();
        std::fs::create_dir_all(t.d.join(".git")).unwrap();
        std::fs::write(t.d.join(".git/config"), "NEEDLE").unwrap();
        std::fs::write(t.d.join("big.txt"), "NEEDLE".repeat(400_000)).unwrap();
        assert_eq!(
            grep("NEEDLE", t.d.to_str().unwrap(), "", false, 0, 5),
            "no matches"
        );
    }

    #[test]
    fn glob_newest_first_double_star_and_cap() {
        let t = t();
        let a = t.w("old.txt", b"1");
        let b = t.w("new.txt", b"2");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        std::fs::File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let r = glob("*.txt", t.d.to_str().unwrap());
        let names: Vec<&str> = r.lines().map(|l| l.rsplit('/').next().unwrap()).collect();
        assert_eq!(names, ["new.txt", "old.txt"]);
        let _ = b;
        std::fs::create_dir_all(t.d.join("sub/deep")).unwrap();
        std::fs::write(t.d.join("sub/deep/x.rs"), "").unwrap();
        std::fs::write(t.d.join("top.rs"), "").unwrap();
        let r = glob("**/*.rs", t.d.to_str().unwrap());
        assert!(r.contains("x.rs") && r.contains("top.rs"));
        assert_eq!(glob("*.zzz", t.d.to_str().unwrap()), "no matches");
        for i in 0..105 {
            std::fs::write(t.d.join(format!("f{i}.dat")), "").unwrap();
        }
        assert!(glob("*.dat", t.d.to_str().unwrap()).ends_with("...[5 more]"));
    }
}
