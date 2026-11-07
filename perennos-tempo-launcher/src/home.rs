//! `tempo-launch home`: show which Tempo folder is in use and why, list any other Tempo folder that
//! holds files, and merge one into the chosen home. Nothing is ever overwritten or deleted: what is
//! merged away goes to `<home>/trash/<date>/`.

use crate::tempo_home::{legacy_dir, Why};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Files counted per folder before the count stops (it only says "holds files").
const COUNT_CAP: usize = 100_000;
/// Lines listed per group in the merge summary.
const LIST_CAP: usize = 200;

/// A folder the rule could name, and which step names it.
pub type Candidate = (PathBuf, String);

/// Every folder the rule can name on this machine, whether or not it exists: the two environment
/// variables if set, the legacy folder, the default.
pub fn candidates(
    windows: bool,
    env: &dyn Fn(&str) -> Option<OsString>,
    user_home: Option<&Path>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for name in ["TEMPO_HOME", "PERENNOS_TEMPO_DATA"] {
        if let Some(v) = env(name).filter(|v| !v.is_empty()) {
            out.push((PathBuf::from(v), format!("{name} is set")));
        }
    }
    if let Some(l) = legacy_dir(windows, env, user_home) {
        out.push((l, "the older Tempo folder".into()));
    }
    out.push((
        crate::tempo_home::default_dir(windows, env, user_home),
        "the default folder".into(),
    ));
    out
}

/// The step that chose a folder: a short word for `--json`, and a sentence.
pub fn step_of(why: &Why) -> (String, String) {
    match why {
        Why::Env(n) => (format!("env:{n}"), format!("{n} is set")),
        Why::Legacy => (
            "legacy".into(),
            "the older Tempo folder exists and TEMPO_HOME is not set".into(),
        ),
        Why::Default => (
            "default".into(),
            "TEMPO_HOME is not set and there is no older Tempo folder".into(),
        ),
    }
}

/// Files under `dir` (links are not followed), up to `COUNT_CAP`.
pub fn count_files(dir: &Path) -> usize {
    fn walk(d: &Path, n: &mut usize) {
        let Ok(rd) = fs::read_dir(d) else { return };
        for e in rd.flatten() {
            if *n >= COUNT_CAP {
                return;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path(), n),
                Ok(_) => *n += 1,
                Err(_) => {}
            }
        }
    }
    let mut n = 0;
    walk(dir, &mut n);
    n
}

/// Another Tempo folder that holds files.
#[derive(Debug, PartialEq, Eq)]
pub struct Other {
    pub dir: PathBuf,
    pub label: String,
    pub files: usize,
}

/// The candidates other than `chosen` that exist and hold files.
pub fn others(chosen: &Path, cands: &[Candidate]) -> Vec<Other> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (dir, label) in cands {
        if crate::same_place(dir, chosen) || !dir.is_dir() || !seen.insert(dir.clone()) {
            continue;
        }
        let files = count_files(dir);
        if files > 0 {
            out.push(Other {
                dir: dir.clone(),
                label: label.clone(),
                files,
            });
        }
    }
    out
}

/// The text (or JSON) `tempo-launch home` prints.
pub fn show(dir: &Path, step: &str, why: &str, others: &[Other], json: bool) -> String {
    use crate::json_escape as j;
    if json {
        let list: Vec<String> = others
            .iter()
            .map(|o| {
                format!(
                    "{{\"path\":\"{}\",\"why\":\"{}\",\"files\":{}}}",
                    j(&o.dir.to_string_lossy()),
                    j(&o.label),
                    o.files
                )
            })
            .collect();
        return format!(
            "{{\"home\":\"{}\",\"step\":\"{}\",\"why\":\"{}\",\"others\":[{}]}}",
            j(&dir.to_string_lossy()),
            j(step),
            j(why),
            list.join(",")
        );
    }
    let mut s = format!(
        "Perennos Tempo home: {}\nChosen because: {why}\n",
        dir.display()
    );
    if others.is_empty() {
        s.push_str("No other Tempo folder with files was found.");
    } else {
        s.push_str("Other Tempo folders that hold files:\n");
        for o in others {
            let cap = if o.files >= COUNT_CAP { "+" } else { "" };
            s.push_str(&format!(
                "  {}  ({}, {}{cap} files)\n    to merge it: tempo-launch home --merge \"{}\"\n",
                o.dir.display(),
                o.label,
                o.files,
                o.dir.display()
            ));
        }
        s.pop();
    }
    s
}

/// What a merge did.
#[derive(Debug, Default)]
pub struct Merged {
    /// Moved into the home by a rename.
    pub moved: Vec<String>,
    /// Copied (the rename crossed volumes); the original is in the trash.
    pub copied: Vec<String>,
    /// Copied, but the original could not be moved anywhere and is still where it was.
    pub left: Vec<String>,
    /// Already in the home: not touched.
    pub skipped: Vec<String>,
    /// Links and other special entries: not touched.
    pub skipped_links: Vec<String>,
    pub errors: Vec<String>,
    /// The emptied tree, now in the trash.
    pub tree_in_trash: Option<PathBuf>,
    /// Why the tree was not moved.
    pub tree_kept: Option<String>,
}

impl Merged {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn text(&self, home: &Path, other: &Path) -> String {
        fn group(s: &mut String, title: &str, v: &[String]) {
            if v.is_empty() {
                return;
            }
            s.push_str(&format!("{title} ({}):\n", v.len()));
            for r in v.iter().take(LIST_CAP) {
                s.push_str(&format!("  {r}\n"));
            }
            if v.len() > LIST_CAP {
                s.push_str(&format!("  ... and {} more\n", v.len() - LIST_CAP));
            }
        }
        let mut s = format!("Merged {} into {}\n", other.display(), home.display());
        group(&mut s, "moved", &self.moved);
        group(&mut s, "copied, original put in the trash", &self.copied);
        group(&mut s, "copied, original left where it was", &self.left);
        group(
            &mut s,
            "skipped, already in the home (not overwritten)",
            &self.skipped,
        );
        group(
            &mut s,
            "skipped, links and special files",
            &self.skipped_links,
        );
        group(&mut s, "problems", &self.errors);
        if self.moved.is_empty() && self.copied.is_empty() && self.left.is_empty() {
            s.push_str("nothing was moved\n");
        }
        match (&self.tree_in_trash, &self.tree_kept) {
            (Some(t), _) => s.push_str(&format!(
                "the emptied folder is now in the trash: {}\n",
                t.display()
            )),
            (None, Some(why)) => s.push_str(&format!("{} was kept: {why}\n", other.display())),
            _ => {}
        }
        s.pop();
        s
    }
}

fn clean_name(other: &Path) -> String {
    let n: String = other
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let n = n.trim_matches('.');
    if n.is_empty() {
        "folder".into()
    } else {
        n.to_string()
    }
}

/// `p`, or `p-2`, `p-3` ... the first name that is free.
fn free_path(p: &Path) -> PathBuf {
    if fs::symlink_metadata(p).is_err() {
        return p.to_path_buf();
    }
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    (2..)
        .map(|i| p.with_file_name(format!("{name}-{i}")))
        .find(|c| fs::symlink_metadata(c).is_err())
        .expect("a free name")
}

/// Copy a file to a name that must not exist yet. A partial copy is never left behind under the
/// real name: it is renamed `.partial` beside it (the caller reports the problem).
fn copy_new(src: &Path, dest: &Path) -> io::Result<()> {
    let mut from = fs::File::open(src)?;
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    let r = io::copy(&mut from, &mut to).and_then(|_| to.sync_all());
    drop(to);
    match r {
        Ok(()) => {
            let _ = fs::set_permissions(dest, fs::metadata(src)?.permissions());
            Ok(())
        }
        Err(e) => {
            let _ = fs::rename(dest, free_path(&dest.with_extension("partial")));
            Err(e)
        }
    }
}

struct Job<'a> {
    home: &'a Path,
    trash: PathBuf,
    /// A trash beside the other folder, for originals that cannot cross to the home's volume.
    side_trash: PathBuf,
    rename: &'a dyn Fn(&Path, &Path) -> io::Result<()>,
}

impl Job<'_> {
    fn move_into(&self, base: &Path, rel: &Path, src: &Path) -> io::Result<PathBuf> {
        let to = free_path(&base.join(rel));
        if let Some(p) = to.parent() {
            fs::create_dir_all(p)?;
        }
        (self.rename)(src, &to)?;
        Ok(to)
    }

    fn file(&self, src: &Path, rel: &Path, out: &mut Merged) {
        // the same on every system: folders joined with /
        let shown = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let dest = self.home.join(rel);
        if fs::symlink_metadata(&dest).is_ok() {
            out.skipped.push(shown);
            return;
        }
        if let Some(p) = dest.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                out.errors.push(format!("{shown}: {e}"));
                return;
            }
        }
        if (self.rename)(src, &dest).is_ok() {
            out.moved.push(shown);
            return;
        }
        // the rename did not work (another volume): copy, then put the original in the trash
        if let Err(e) = copy_new(src, &dest) {
            out.errors.push(format!("{shown}: copy failed: {e}"));
            return;
        }
        let in_trash = self
            .move_into(&self.trash, rel, src)
            .or_else(|_| self.move_into(&self.side_trash, rel, src));
        match in_trash {
            Ok(_) => out.copied.push(shown),
            Err(_) => out.left.push(shown),
        }
    }

    fn walk(&self, dir: &Path, rel: &Path, out: &mut Merged) {
        let Ok(rd) = fs::read_dir(dir) else {
            out.errors
                .push(format!("{}: cannot be read", rel.display()));
            return;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let r = rel.join(e.file_name());
            match e.file_type() {
                Ok(t) if t.is_dir() => self.walk(&e.path(), &r, out),
                Ok(t) if t.is_file() => self.file(&e.path(), &r, out),
                _ => out.skipped_links.push(r.display().to_string()),
            }
        }
    }
}

/// Anything under `dir` that is not a folder.
fn holds_entries(dir: &Path) -> bool {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| !e.file_type().is_ok_and(|t| t.is_dir()) || holds_entries(&e.path()))
}

/// Merge `other` into `home`. See the module text; `date` is `YYYYMMDD`.
pub fn merge(home: &Path, other: &Path, date: &str) -> Result<Merged, String> {
    merge_with(home, other, date, &|a, b| fs::rename(a, b))
}

/// `merge` with the rename injected, so a test can make it fail the way a move across volumes does.
pub fn merge_with(
    home: &Path,
    other: &Path,
    date: &str,
    rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> Result<Merged, String> {
    let meta = fs::symlink_metadata(other)
        .map_err(|e| format!("{} cannot be read: {e}", other.display()))?;
    if !meta.is_dir() {
        return Err(format!(
            "{} is not a folder (a link is not followed)",
            other.display()
        ));
    }
    fs::create_dir_all(home).map_err(|e| format!("{} cannot be made: {e}", home.display()))?;
    let (ch, co) = (
        fs::canonicalize(home).map_err(|e| e.to_string())?,
        fs::canonicalize(other).map_err(|e| e.to_string())?,
    );
    if ch.starts_with(&co) || co.starts_with(&ch) {
        return Err(format!(
            "{} and {} are the same folder or one is inside the other; nothing was changed",
            other.display(),
            home.display()
        ));
    }
    let name = clean_name(&co);
    let job = Job {
        home: &ch,
        trash: ch
            .join("trash")
            .join(date)
            .join(format!("merged-from-{name}")),
        side_trash: co
            .parent()
            .unwrap_or(&co)
            .join(".tempo-merged-trash")
            .join(date)
            .join(&name),
        rename,
    };
    let mut out = Merged::default();
    job.walk(&co, Path::new(""), &mut out);
    if !out.errors.is_empty() {
        out.tree_kept = Some("some files had problems".into());
    } else if holds_entries(&co) {
        out.tree_kept = Some("it still holds files that were skipped or left".into());
    } else {
        let tree = Path::new("emptied").join(&name);
        let moved = job
            .move_into(&ch.join("trash").join(date), &tree, &co)
            .or_else(|_| job.move_into(&job.side_trash, Path::new("emptied"), &co));
        match moved {
            Ok(t) => out.tree_in_trash = Some(t),
            Err(e) => out.tree_kept = Some(format!("it could not be moved to the trash: {e}")),
        }
    }
    Ok(out)
}

/// `YYYYMMDD` for the trash folder.
pub fn today() -> String {
    crate::file_stamp(crate::unix_now())[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "tempo-home-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn read(root: &Path, rel: &str) -> String {
        fs::read_to_string(root.join(rel)).unwrap()
    }

    /// Every file under `dir`, relative, sorted.
    fn tree(dir: &Path) -> Vec<String> {
        fn walk(d: &Path, base: &Path, out: &mut Vec<String>) {
            for e in fs::read_dir(d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, base, out);
                } else {
                    out.push(
                        p.strip_prefix(base)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        let mut v = vec![];
        walk(dir, dir, &mut v);
        v.sort();
        v
    }

    // ---- show

    #[test]
    fn candidates_list_what_the_rule_can_name() {
        let m: HashMap<&str, OsString> = [
            ("TEMPO_HOME", "/th".into()),
            ("PERENNOS_TEMPO_DATA", "/pt".into()),
        ]
        .into_iter()
        .collect();
        let env = |k: &str| m.get(k).cloned();
        let c = candidates(false, &env, Some(Path::new("/h")));
        let dirs: Vec<_> = c.iter().map(|c| c.0.clone()).collect();
        assert_eq!(
            dirs,
            [
                PathBuf::from("/th"),
                PathBuf::from("/pt"),
                PathBuf::from("/h/.local/state/perennos-tempo"),
                PathBuf::from("/h/.tempo")
            ]
        );
        let none = |_: &str| None;
        assert_eq!(candidates(true, &none, Some(Path::new("C:/u"))).len(), 1);
        let legacy_env = |k: &str| (k == "TEMPO_LEGACY_HOME").then(|| OsString::from("D:/old"));
        assert_eq!(
            candidates(true, &legacy_env, Some(Path::new("C:/u"))).len(),
            2
        );
    }

    #[test]
    fn others_lists_only_folders_that_exist_hold_files_and_are_not_the_home() {
        let root = tmp("others");
        let chosen = root.join("chosen");
        let legacy = root.join("legacy");
        let empty = root.join("empty");
        let missing = root.join("missing");
        for d in [&chosen, &legacy, &empty] {
            fs::create_dir_all(d).unwrap();
        }
        fs::create_dir_all(empty.join("only/folders")).unwrap();
        put(&legacy, "agent/conversations/a.json", "{}");
        put(&legacy, "engines.json", "[]");
        put(&chosen, "remote.json", "[]");
        let cands = vec![
            (chosen.clone(), "chosen".to_string()),
            (legacy.clone(), "legacy".to_string()),
            (legacy.clone(), "again".to_string()),
            (empty.clone(), "empty".to_string()),
            (missing, "missing".to_string()),
        ];
        let o = others(&chosen, &cands);
        assert_eq!(
            o,
            vec![Other {
                dir: legacy.clone(),
                label: "legacy".into(),
                files: 2
            }]
        );
        let text = show(&chosen, "default", "why", &o, false);
        assert!(text.contains("Chosen because: why") && text.contains("2 files"));
        assert!(text.contains(&format!("home --merge \"{}\"", legacy.display())));
        let json = show(&chosen, "default", "why", &o, true);
        assert!(json.starts_with("{\"home\":") && json.contains("\"step\":\"default\""));
        assert!(json.contains("\"files\":2") && json.ends_with("}"));
        assert!(show(&chosen, "default", "why", &[], false).contains("No other Tempo folder"));
        assert!(show(&chosen, "default", "why", &[], true).contains("\"others\":[]"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn step_words_name_the_step() {
        assert_eq!(step_of(&Why::Env("TEMPO_HOME")).0, "env:TEMPO_HOME");
        assert_eq!(step_of(&Why::Legacy).0, "legacy");
        assert_eq!(step_of(&Why::Default).0, "default");
        assert!(step_of(&Why::Env("PERENNOS_TEMPO_DATA"))
            .1
            .contains("PERENNOS_TEMPO_DATA"));
    }

    // ---- merge

    #[test]
    fn merge_moves_new_files_keeps_the_layout_and_never_overwrites() {
        let root = tmp("merge");
        let home = root.join("home");
        let old = root.join("old-tempo");
        put(&home, "engines.json", "HOME engines");
        put(&home, "agent/conversations/keep.json", "home conv");
        put(&old, "engines.json", "OLD engines");
        put(&old, "connections.json", "old connections");
        put(&old, "agent/conversations/keep.json", "old conv");
        put(&old, "agent/conversations/new.json", "new conv");
        put(&old, "workflows/templates/t.json", "tpl");
        fs::create_dir_all(old.join("fleet/empty-dir")).unwrap();
        let m = merge(&home, &old, "20261106").unwrap();
        assert!(m.ok(), "{:?}", m.errors);
        assert_eq!(
            m.moved,
            [
                "agent/conversations/new.json",
                "connections.json",
                "workflows/templates/t.json"
            ]
        );
        assert_eq!(m.skipped, ["agent/conversations/keep.json", "engines.json"]);
        // never overwritten
        assert_eq!(read(&home, "engines.json"), "HOME engines");
        assert_eq!(read(&home, "agent/conversations/keep.json"), "home conv");
        // layout kept
        assert_eq!(read(&home, "agent/conversations/new.json"), "new conv");
        assert_eq!(read(&home, "workflows/templates/t.json"), "tpl");
        // the skipped files stay where they were, so the folder is kept
        assert_eq!(read(&old, "engines.json"), "OLD engines");
        assert!(m.tree_in_trash.is_none());
        assert!(m.tree_kept.as_deref().unwrap().contains("skipped"));
        let text = m.text(&home, &old);
        assert!(text.contains("moved (3)") && text.contains("skipped, already in the home"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_emptied_tree_goes_to_the_trash_and_nothing_is_deleted() {
        let root = tmp("empty-tree");
        let home = root.join("home");
        let old = root.join("old tempo!");
        put(&old, "a.json", "A");
        put(&old, "deep/er/b.json", "B");
        fs::create_dir_all(old.join("hollow")).unwrap();
        let m = merge(&home, &old, "20261106").unwrap();
        assert_eq!(m.moved, ["a.json", "deep/er/b.json"]);
        assert!(m.skipped.is_empty());
        assert!(!old.exists(), "the emptied tree was moved away");
        let t = m.tree_in_trash.clone().unwrap();
        // compare real paths: on macOS the temp folder is a link (/var to /private/var)
        let trash = fs::canonicalize(home.join("trash").join("20261106")).unwrap();
        assert!(fs::canonicalize(&t).unwrap().starts_with(&trash), "{t:?}");
        assert!(t.is_dir() && t.join("hollow").is_dir());
        assert!(m
            .text(&home, &old)
            .contains("emptied folder is now in the trash"));
        // a second merge of a folder with the same name gets a free name, nothing is replaced
        put(&old, "c.json", "C");
        let m2 = merge(&home, &old, "20261106").unwrap();
        assert_ne!(m2.tree_in_trash.clone().unwrap(), t);
        assert!(t.is_dir());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_rename_that_crosses_volumes_copies_then_puts_the_original_in_the_trash() {
        let root = tmp("cross");
        let home = root.join("home");
        let old = root.join("old");
        put(&old, "one.json", "1");
        put(&old, "sub/two.json", "2");
        put(&home, "sub/two.json", "home two");
        // a rename into the home fails (as it does between volumes); into its trash it works
        let home_c = fs::canonicalize(&home).unwrap();
        let trash = home_c.join("trash");
        let cross = move |a: &Path, b: &Path| -> io::Result<()> {
            if b.starts_with(&home_c) && !b.starts_with(&trash) {
                Err(io::Error::other("cross-device link"))
            } else {
                fs::rename(a, b)
            }
        };
        let m = merge_with(&home, &old, "20261106", &cross).unwrap();
        assert!(m.ok(), "{:?}", m.errors);
        assert_eq!(m.copied, ["one.json"]);
        assert!(m.moved.is_empty());
        assert_eq!(m.skipped, ["sub/two.json"]);
        assert_eq!(read(&home, "one.json"), "1");
        assert_eq!(read(&home, "sub/two.json"), "home two");
        let kept = tree(&home.join("trash/20261106"));
        assert!(
            kept.iter().any(|f| f.ends_with("merged-from-old/one.json")),
            "{kept:?}"
        );
        assert!(
            !old.join("one.json").exists(),
            "the original moved to the trash, not deleted"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn when_even_the_trash_is_on_another_volume_the_original_goes_to_a_trash_beside_it() {
        let root = tmp("side");
        let home = root.join("home");
        let old = root.join("old");
        put(&old, "one.json", "1");
        fs::create_dir_all(&home).unwrap();
        let home_c = fs::canonicalize(&home).unwrap();
        // nothing can be moved into the home's volume, the trash included
        let cross = move |a: &Path, b: &Path| -> io::Result<()> {
            if b.starts_with(&home_c) {
                Err(io::Error::other("cross-device link"))
            } else {
                fs::rename(a, b)
            }
        };
        let m = merge_with(&home, &old, "20261106", &cross).unwrap();
        assert!(m.ok(), "{:?}", m.errors);
        assert_eq!(m.copied, ["one.json"]);
        assert_eq!(read(&home, "one.json"), "1");
        let side = tree(&root.join(".tempo-merged-trash"));
        assert!(side.iter().any(|f| f.ends_with("one.json")), "{side:?}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn when_nothing_can_be_moved_the_original_is_left_and_reported() {
        let root = tmp("left");
        let home = root.join("home");
        let old = root.join("old");
        put(&old, "one.json", "1");
        let never = |_: &Path, _: &Path| -> io::Result<()> { Err(io::Error::other("nope")) };
        let m = merge_with(&home, &old, "20261106", &never).unwrap();
        assert_eq!(m.left, ["one.json"]);
        assert_eq!(read(&home, "one.json"), "1");
        assert_eq!(read(&old, "one.json"), "1", "never deleted");
        assert!(m.tree_in_trash.is_none());
        assert!(m.text(&home, &old).contains("original left where it was"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_merge_is_refused_for_the_same_folder_overlaps_files_and_missing_folders() {
        let root = tmp("refuse");
        let home = root.join("home");
        put(&home, "x.json", "x");
        put(&home, "inner/y.json", "y");
        assert!(merge(&home, &home, "d").is_err());
        assert!(merge(&home, &home.join("inner"), "d").is_err());
        assert!(merge(&home.join("inner"), &home, "d").is_err());
        assert!(merge(&home, &home.join("x.json"), "d").is_err());
        assert!(merge(&home, &root.join("nope"), "d").is_err());
        assert_eq!(tree(&home), ["inner/y.json", "x.json"], "nothing changed");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_never_followed_or_moved() {
        let root = tmp("links");
        let home = root.join("home");
        let old = root.join("old");
        let outside = root.join("outside");
        put(&outside, "secret.txt", "s");
        put(&old, "real.json", "r");
        std::os::unix::fs::symlink(&outside, old.join("link-dir")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), old.join("link-file")).unwrap();
        let m = merge(&home, &old, "20261106").unwrap();
        assert_eq!(m.moved, ["real.json"]);
        assert_eq!(m.skipped_links, ["link-dir", "link-file"]);
        assert!(!home.join("link-dir").exists() && !home.join("link-file").exists());
        assert_eq!(read(&outside, "secret.txt"), "s");
        // a link given as the folder itself is refused
        let l = root.join("as-link");
        std::os::unix::fs::symlink(&outside, &l).unwrap();
        assert!(merge(&home, &l, "d").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copy_new_never_overwrites_and_a_partial_copy_is_not_left_under_the_real_name() {
        let root = tmp("copynew");
        put(&root, "src.txt", "S");
        put(&root, "dest.txt", "D");
        assert!(copy_new(&root.join("src.txt"), &root.join("dest.txt")).is_err());
        assert_eq!(read(&root, "dest.txt"), "D");
        copy_new(&root.join("src.txt"), &root.join("fresh.txt")).unwrap();
        assert_eq!(read(&root, "fresh.txt"), "S");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn names_and_dates_for_the_trash_are_plain() {
        assert_eq!(clean_name(Path::new("/a/.tempo")), "tempo");
        assert_eq!(clean_name(Path::new("/a/My Tempo (old)")), "My_Tempo__old_");
        assert_eq!(clean_name(Path::new("/")), "folder");
        let d = today();
        assert!(d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()), "{d}");
        let root = tmp("free");
        put(&root, "a", "1");
        assert_eq!(free_path(&root.join("a")), root.join("a-2"));
        assert_eq!(free_path(&root.join("b")), root.join("b"));
        let _ = fs::remove_dir_all(root);
    }
}
