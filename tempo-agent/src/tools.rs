//! Built-in tools offered in every heavy-lane turn: `list_dir`, `read_file`,
//! `write_file` (only inside the configured write roots; overwrites need approval)
//! and `workflow_update`.
use crate::error::{Error, Result};
use crate::ollama::Tool;
use crate::text::is_secret_path;
use crate::workflow::{Task, Workflow};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const BUILTIN_NAMES: [&str; 4] = ["list_dir", "read_file", "write_file", "workflow_update"];

/// What a built-in may touch while it runs.
pub struct BuiltinCtx<'a> {
    pub workflow: &'a Workflow,
    pub write_roots: &'a [PathBuf],
    pub result_cap: usize,
    /// Ask the person: (what, detail) -> approved?
    pub confirm: &'a mut dyn FnMut(&str, &str) -> bool,
}

fn string(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

fn roots_label(roots: &[PathBuf]) -> String {
    roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join("; ")
}

pub fn builtin_tools(write_roots: &[PathBuf], result_cap: usize) -> Vec<Tool> {
    vec![
        Tool::function(
            "list_dir",
            "List the files and folders in a directory.",
            json!({"type": "object", "properties": {"path": string("directory path")}, "required": ["path"]}),
        ),
        Tool::function(
            "read_file",
            &format!("Read a text file (first {result_cap} chars unless offset given)."),
            json!({"type": "object", "properties": {"path": string("file path"), "offset": {"type": "integer"}},
                   "required": ["path"]}),
        ),
        Tool::function(
            "write_file",
            &format!("Write a text file inside the allowed folders ({}). Overwrites need approval.", roots_label(write_roots)),
            json!({"type": "object", "properties": {"path": string("file path"), "content": string("text")},
                   "required": ["path", "content"]}),
        ),
        Tool::function(
            "workflow_update",
            "Update this conversation's persistent task list and notes. Call it when you plan work and after each step.",
            json!({"type": "object", "properties": {
                "tasks": {"type": "array", "items": {"type": "object",
                          "properties": {"text": {"type": "string"}, "done": {"type": "boolean"}}}},
                "note": string("short note")}, "required": []}),
        ),
    ]
}

fn str_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| Error::Tool(format!("missing string argument '{key}'")))
}

fn uint_arg(args: &Map<String, Value>, key: &str) -> usize {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0) as usize,
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// Run a built-in, or `None` if `name` is not one.
pub fn run_builtin(name: &str, args: &Map<String, Value>, ctx: &mut BuiltinCtx) -> Option<Result<String>> {
    Some(match name {
        "list_dir" => str_arg(args, "path").and_then(list_dir),
        "read_file" => str_arg(args, "path").and_then(|p| read_file(p, uint_arg(args, "offset"), ctx.result_cap)),
        "write_file" => write_file(args, ctx),
        "workflow_update" => workflow_update(args, ctx.workflow),
        _ => return None,
    })
}

fn list_dir(path: &str) -> Result<String> {
    if is_secret_path(path) {
        return Ok("refused: secret path".to_string());
    }
    let mut rows: Vec<(bool, String, u64)> = fs::read_dir(path)?
        .filter_map(|e| e.ok())
        .map(|e| {
            let md = fs::metadata(e.path()).ok();
            let is_dir = md.as_ref().is_some_and(|m| m.is_dir());
            (is_dir, e.file_name().to_string_lossy().into_owned(), md.map_or(0, |m| m.len()))
        })
        .collect();
    rows.sort_by_key(|(d, n, _)| (!*d, n.to_lowercase()));
    let out: Vec<String> = rows
        .iter()
        .take(200)
        .map(|(d, n, size)| if *d { format!("[dir] {n}") } else { format!("{size:>10}  {n}") })
        .collect();
    Ok(if out.is_empty() { "(empty)".to_string() } else { out.join("\n") })
}

fn read_file(path: &str, offset: usize, cap: usize) -> Result<String> {
    if is_secret_path(path) {
        return Ok("refused: secret path".to_string());
    }
    let raw = fs::read(path)?;
    Ok(String::from_utf8_lossy(&raw).chars().skip(offset).take(cap).collect())
}

/// Absolute path with `.` and `..` resolved lexically (`std::path::absolute` keeps
/// `..` on Unix, which would let `root/../elsewhere` pass a prefix check).
pub fn normalize_path(p: &Path) -> std::io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in std::path::absolute(p)?.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

/// Whether `path` is one of `roots` or inside one (case-insensitive on Windows).
/// Lexical only: a symlink inside a root that points elsewhere is not detected.
pub fn in_roots(path: &Path, roots: &[PathBuf]) -> bool {
    let Ok(p) = normalize_path(path) else { return false };
    let fold = |x: &Path| if cfg!(windows) { PathBuf::from(x.to_string_lossy().to_lowercase()) } else { x.to_path_buf() };
    roots.iter().filter_map(|r| normalize_path(r).ok()).any(|r| fold(&p).starts_with(fold(&r)))
}

fn write_file(args: &Map<String, Value>, ctx: &mut BuiltinCtx) -> Result<String> {
    let (path, content) = (str_arg(args, "path")?, str_arg(args, "content")?);
    if !in_roots(Path::new(path), ctx.write_roots) {
        return Ok(format!("refused: outside allowed write folders: {}", roots_label(ctx.write_roots)));
    }
    let target = normalize_path(Path::new(path))?;
    if target.exists() && !(ctx.confirm)("overwrite file", path) {
        return Ok("denied by user".to_string());
    }
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&target, content)?;
    Ok(format!("wrote {} chars to {path}", content.chars().count()))
}

fn workflow_update(args: &Map<String, Value>, wf: &Workflow) -> Result<String> {
    let tasks: Option<Vec<Task>> = args.get("tasks").and_then(Value::as_array).map(|a| a.iter().map(Task::from_value).collect());
    let note = args.get("note").and_then(Value::as_str);
    Ok(wf.update(tasks.as_deref(), note)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn run(name: &str, args: Value, dir: &TempDir, approve: bool) -> Option<Result<String>> {
        let wf = Workflow::open(dir.path().join("wf.md"), "t").unwrap();
        let roots = [dir.path().join("out")];
        let mut asked = |_: &str, _: &str| approve;
        let mut ctx = BuiltinCtx { workflow: &wf, write_roots: &roots, result_cap: 10, confirm: &mut asked };
        run_builtin(name, &obj(args), &mut ctx)
    }

    #[test]
    fn schemas_cover_every_builtin() {
        let tools = builtin_tools(&[PathBuf::from("/w")], 6000);
        let names: Vec<_> = tools.iter().map(|t| t.function.name.as_str()).collect();
        assert_eq!(names, BUILTIN_NAMES);
        assert!(tools[1].function.description.contains("6000"));
        assert!(tools[2].function.description.contains("/w"));
    }

    #[test]
    fn lists_and_reads() {
        let dir = TempDir::new("tools");
        fs::create_dir_all(dir.path().join("Sub")).unwrap();
        fs::write(dir.path().join("b.txt"), "0123456789abcdef").unwrap();
        fs::write(dir.path().join("A.md"), "x").unwrap();
        let p = dir.path().to_string_lossy().into_owned();
        let listing = run("list_dir", json!({"path": p}), &dir, true).unwrap().unwrap();
        let lines: Vec<_> = listing.lines().collect();
        assert_eq!(lines[0], "[dir] Sub");
        assert!(lines[1].ends_with("  A.md") && lines[1].trim_start().starts_with('1'));
        assert!(lines.iter().any(|l| l.ends_with("  b.txt")));
        let empty = dir.path().join("Sub").to_string_lossy().into_owned();
        assert_eq!(run("list_dir", json!({"path": empty}), &dir, true).unwrap().unwrap(), "(empty)");
        let f = dir.path().join("b.txt").to_string_lossy().into_owned();
        assert_eq!(run("read_file", json!({"path": f}), &dir, true).unwrap().unwrap(), "0123456789", "capped");
        assert_eq!(run("read_file", json!({"path": f, "offset": "12"}), &dir, true).unwrap().unwrap(), "cdef");
        assert_eq!(run("read_file", json!({"path": "/x/.env"}), &dir, true).unwrap().unwrap(), "refused: secret path");
        assert!(matches!(run("read_file", json!({"path": "/no/such/file"}), &dir, true), Some(Err(Error::Io(_)))));
        assert!(matches!(run("read_file", json!({}), &dir, true), Some(Err(Error::Tool(_)))));
        assert!(run("not_a_builtin", json!({}), &dir, true).is_none());
    }

    #[test]
    fn writes_only_inside_roots() {
        let dir = TempDir::new("tools");
        let inside = dir.path().join("out").join("new").join("f.txt");
        let p = inside.to_string_lossy().into_owned();
        assert_eq!(run("write_file", json!({"path": p, "content": "héllo"}), &dir, false).unwrap().unwrap(), format!("wrote 5 chars to {p}"));
        assert_eq!(fs::read_to_string(&inside).unwrap(), "héllo");
        // Overwrite asks first; a "no" leaves the file alone.
        assert_eq!(run("write_file", json!({"path": p, "content": "x"}), &dir, false).unwrap().unwrap(), "denied by user");
        assert_eq!(fs::read_to_string(&inside).unwrap(), "héllo");
        run("write_file", json!({"path": p, "content": "x"}), &dir, true).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&inside).unwrap(), "x");
        // `..` cannot climb out of a root, and neighbours sharing a prefix do not count.
        let escape = format!("{}/out/../escaped.txt", dir.path().display());
        let sibling = format!("{}/outside/f.txt", dir.path().display());
        for path in [escape, sibling] {
            let r = run("write_file", json!({"path": path, "content": "x"}), &dir, true).unwrap().unwrap();
            assert!(r.starts_with("refused: outside allowed write folders"), "{r}");
        }
        assert!(!dir.path().join("escaped.txt").exists());
    }

    #[test]
    fn normalizes_paths() {
        let base = std::env::temp_dir();
        assert_eq!(normalize_path(&base.join("a/./b/../c")).unwrap(), normalize_path(&base.join("a/c")).unwrap());
        assert!(in_roots(&base.join("r/x"), &[base.join("r")]));
        assert!(in_roots(&base.join("r"), &[base.join("r/")]));
        assert!(!in_roots(&base.join("rr/x"), &[base.join("r")]));
        assert!(!in_roots(&base.join("x"), &[]));
    }

    #[test]
    fn updates_the_workflow() {
        let dir = TempDir::new("tools");
        let r = run("workflow_update", json!({"tasks": [{"text": "a", "done": true}, "b"], "note": "n1"}), &dir, true);
        assert_eq!(r.unwrap().unwrap(), "workflow updated");
        let t = fs::read_to_string(dir.path().join("wf.md")).unwrap();
        assert!(t.contains("- [x] a\n- [ ] b") && t.contains(" n1\n"));
    }
}
