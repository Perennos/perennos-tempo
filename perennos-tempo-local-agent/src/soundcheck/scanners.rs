//! The checks that never ask a model. Each one is a plain function from the change to a list
//! of findings: the same input always gives the same answer, so a pre-push hook can trust it.

use super::diff::{DLine, FileDiff, FileStatus};
use super::{Finding, Severity};
use crate::util::{self, head_chars};
use regex::Regex;
use std::sync::OnceLock;

macro_rules! rx {
    ($pat:expr) => {{
        static R: OnceLock<Regex> = OnceLock::new();
        R.get_or_init(|| Regex::new($pat).expect("scanner pattern"))
    }};
}

/// Run every scanner over the change.
pub fn scan_all(files: &[FileDiff]) -> Vec<Finding> {
    let mut all = secrets(files);
    all.extend(dangerous_calls(files));
    all.extend(dependency_changes(files));
    all.extend(weak_tests(files));
    all
}

/// What goes in the report as the line of code: one line, secrets blanked.
fn evidence(line: &str) -> String {
    head_chars(util::redact(line.trim()).trim(), 200)
}

/// Keep the first four characters of a secret so a person can recognise it, hide the rest.
fn masked(line: &str, start: usize, end: usize) -> String {
    let secret = &line[start..end];
    let shown: String = secret.chars().take(4).collect();
    let hidden = format!("{}{shown}...[hidden]{}", &line[..start], &line[end..]);
    head_chars(hidden.trim(), 200)
}

struct Hit<'a> {
    file: &'a FileDiff,
    line: usize,
    severity: Severity,
    rule: &'a str,
    category: &'a str,
    problem: String,
    scenario: &'a str,
    fix: &'a str,
    evidence: String,
}

fn finding(h: Hit) -> Finding {
    Finding::from_scanner(
        &h.file.path,
        h.line,
        h.severity,
        h.rule,
        h.category,
        &h.problem,
        h.scenario,
        h.fix,
        &h.evidence,
    )
}

// ---------------------------------------------------------------- secrets

const SECRET_FIX: &str = "Remove it from the change, rotate the secret (it is exposed once committed), and read it from the environment or a secrets store instead.";

fn is_placeholder(value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    const WORDS: [&str; 20] = [
        "example",
        "changeme",
        "change_me",
        "placeholder",
        "your",
        "xxxx",
        "****",
        "<",
        "dummy",
        "sample",
        "todo",
        "redacted",
        "fake",
        "env",
        "null",
        "none",
        "true",
        "false",
        "password",
        "secret",
    ];
    v.starts_with('$')
        || v.contains("${")
        || v.contains("{{")
        || v.contains("%(")
        || WORDS.iter().any(|w| v.contains(w))
}

/// Shannon entropy in bits per character: random keys score high, words and names score low.
fn entropy(s: &str) -> f64 {
    let n = s.chars().count() as f64;
    let mut counts = std::collections::HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
    }
    counts
        .values()
        .map(|&k| {
            let p = k as f64 / n;
            -p * p.log2()
        })
        .sum()
}

fn secret_file_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".example") || n.ends_with(".sample") || n.ends_with(".template") {
        return false;
    }
    n == ".env"
        || n.starts_with(".env.")
        || matches!(
            n.as_str(),
            "id_rsa"
                | "id_dsa"
                | "id_ecdsa"
                | "id_ed25519"
                | ".npmrc"
                | ".pypirc"
                | ".netrc"
                | "credentials.json"
        )
        || [".pem", ".p12", ".pfx", ".key"]
            .iter()
            .any(|e| n.ends_with(e))
}

/// Keys, tokens, private keys and passwords written into the change.
pub fn secrets(files: &[FileDiff]) -> Vec<Finding> {
    // built in two pieces so this file never contains the marker the repository leak scan hunts for
    let pem = rx!(concat!("-----BEGIN [A-Z0-9 ]*", "PRIVATE KEY"));
    let named = rx!(
        r#"(?i)(api[_-]?key|secret|token|passw(?:or)?d|passwd|pwd|auth[_-]?key|private[_-]?key|access[_-]?key)\w*["']?\s*[:=]\s*["']([^"'\s]{8,})["']"#
    );
    let mut out = vec![];
    for f in files {
        if f.status != FileStatus::Deleted && secret_file_name(f.file_name()) {
            out.push(finding(Hit {
                file: f,
                line: 0,
                severity: Severity::High,
                rule: "secret-file",
                category: "secret",
                problem: format!("{} looks like a file that holds secrets and is part of this change.", f.file_name()),
                scenario: "Once it is committed and pushed, everyone with read access to the repository (and every clone) has the secrets.",
                fix: "Keep it out of git: add it to .gitignore, commit a .example file with fake values, and rotate anything that was in it.",
                evidence: f.path.clone(),
            }));
        }
        for l in f.added() {
            if let Some(h) = secret_in_line(f, l, pem, named) {
                out.push(h);
            }
        }
    }
    out
}

fn secret_in_line(f: &FileDiff, l: &DLine, pem: &Regex, named: &Regex) -> Option<Finding> {
    let hit = |sev, rule, problem: &str, ev: String| {
        finding(Hit {
            file: f,
            line: l.no,
            severity: sev,
            rule,
            category: "secret",
            problem: problem.to_string(),
            scenario: "Anyone who can read the repository, a log or a build artifact can use it to act as the owner of the key.",
            fix: SECRET_FIX,
            evidence: ev,
        })
    };
    if let Some(m) = pem.find(&l.text) {
        return Some(hit(
            Severity::Critical,
            "private-key",
            "A private key is written into the change.",
            masked(&l.text, m.start(), m.end()),
        ));
    }
    if let Some(m) = util::secret_tok_rx().find(&l.text) {
        return Some(hit(
            Severity::High,
            "token-shape",
            "A string shaped like an access token or API key is written into the change.",
            masked(&l.text, m.start(), m.end()),
        ));
    }
    let c = named.captures(&l.text)?;
    let (name, value) = (c[1].to_ascii_lowercase(), c.get(2)?);
    if is_placeholder(value.as_str()) {
        return None;
    }
    let (start, end) = (value.start(), value.end());
    if value.as_str().len() >= 16 && entropy(value.as_str()) >= 3.3 {
        return Some(hit(
            Severity::High,
            "high-entropy",
            &format!("A random-looking value is assigned to `{name}`."),
            masked(&l.text, start, end),
        ));
    }
    let password_like = ["passw", "pwd", "secret"].iter().any(|w| name.contains(w));
    (password_like && value.as_str().len() >= 4).then(|| {
        hit(
            Severity::Medium,
            "hard-coded-credential",
            &format!("A literal value is assigned to `{name}`."),
            masked(&l.text, start, end),
        )
    })
}

// ------------------------------------------------------------ dangerous calls

struct Rule {
    id: &'static str,
    /// File extensions the rule applies to; empty means any code file.
    langs: &'static [&'static str],
    pattern: &'static str,
    /// A line that also matches this is fine (for example `yaml.load` with a safe loader).
    unless: Option<&'static str>,
    severity: Severity,
    problem: &'static str,
    scenario: &'static str,
    fix: &'static str,
}

const JS: &[&str] = &["js", "jsx", "ts", "tsx", "mjs", "cjs"];

static RULES: &[Rule] = &[
    Rule { id: "eval", langs: &["js", "jsx", "ts", "tsx", "mjs", "cjs", "py", "php", "rb"], pattern: r"(^|[^.\w])eval\s*\(", unless: None, severity: Severity::High,
        problem: "eval() runs text as code.",
        scenario: "If any part of that text comes from a user, a file or the network, the sender can run their own code on this machine.",
        fix: "Parse the data instead (JSON.parse, ast.literal_eval) or call the function directly." },
    Rule { id: "python-exec", langs: &["py"], pattern: r"(^|[^.\w])exec\s*\(", unless: None, severity: Severity::High,
        problem: "exec() runs text as code.",
        scenario: "Text that reaches exec() from outside becomes code running with this program's permissions.",
        fix: "Call the function you mean directly, or look it up in a fixed table of allowed names." },
    Rule { id: "js-new-function", langs: JS, pattern: r"new\s+Function\s*\(", unless: None, severity: Severity::High,
        problem: "new Function() builds code from a string, like eval.",
        scenario: "A string that includes outside input turns into code that runs in this process.",
        fix: "Replace it with a normal function and pass the varying part as an argument." },
    Rule { id: "shell-true", langs: &["py"], pattern: r"shell\s*=\s*True", unless: None, severity: Severity::High,
        problem: "A command runs through the shell (shell=True).",
        scenario: "A file name or argument containing `;` or `$(...)` runs extra commands.",
        fix: "Pass the command as a list of arguments and leave shell off." },
    Rule { id: "os-system", langs: &["py"], pattern: r"\bos\.(system|popen)\s*\(", unless: None, severity: Severity::High,
        problem: "os.system/os.popen hands a string to the shell.",
        scenario: "Any special character in that string changes which commands run.",
        fix: "Use subprocess.run with a list of arguments." },
    Rule { id: "node-exec-string", langs: JS, pattern: r"\b(exec|execSync)\s*\(\s*(`[^`]*\$\{|[^)]*[\x22']\s*\+)", unless: None, severity: Severity::High,
        problem: "A shell command is built by joining strings.",
        scenario: "A value with a quote or `;` in it ends the intended command and starts another.",
        fix: "Use execFile/spawn with an argument array." },
    Rule { id: "python-command-string", langs: &["py"], pattern: r#"subprocess\.\w+\(\s*(f["']|["'][^"']*["']\s*(\+|%)|[^,)]*\.format\()"#, unless: None, severity: Severity::High,
        problem: "A subprocess command is built from a string with values pasted in.",
        scenario: "A value with spaces, quotes or `;` changes the command that runs.",
        fix: "Give subprocess a list of arguments, one value per item." },
    Rule { id: "rust-command-string", langs: &["rs"], pattern: r#"Command::new\(\s*&?format!|\.args?\(\s*\[?\s*"(-c|/C|-Command)"\s*,\s*&?format!"#, unless: None, severity: Severity::High,
        problem: "A command or shell script is built with format!.",
        scenario: "A value containing shell syntax runs as part of the command.",
        fix: "Call Command::new with a fixed program and add each value with .arg()." },
    Rule { id: "go-shell-string", langs: &["go"], pattern: r#"exec\.Command(Context)?\(.*"(sh|bash)".*"-c".*(\+|Sprintf)"#, unless: None, severity: Severity::High,
        problem: "A shell script is built from strings in exec.Command.",
        scenario: "A value with shell syntax runs extra commands.",
        fix: "Pass the program and each argument separately, with no shell." },
    Rule { id: "rust-unsafe", langs: &["rs"], pattern: r"\bunsafe\s*(\{|fn\b|impl\b|trait\b)", unless: None, severity: Severity::Medium,
        problem: "New `unsafe` code switches off the compiler's memory checks here.",
        scenario: "A wrong assumption in this block can become a crash or memory corruption that safe code could not cause.",
        fix: "Use a safe API if one exists; otherwise add a SAFETY comment that states what makes it sound and test the edge cases." },
    Rule { id: "unsafe-deserialize", langs: &[], pattern: r"\b(c?[Pp]ickle|marshal)\.loads?\(|\bjsonpickle\.decode\(|\bunserialize\s*\(|\bMarshal\.load\b|ObjectInputStream\s*\(|yaml\.unsafe_load", unless: None, severity: Severity::High,
        problem: "Data is turned back into objects with a format that can run code while loading.",
        scenario: "If an attacker can supply or alter that data, loading it runs their code.",
        fix: "Use a data-only format (JSON) and validate it, or only load data you wrote and signed." },
    Rule { id: "yaml-load", langs: &["py"], pattern: r"yaml\.load\s*\(", unless: Some(r"Safe|safe_load"), severity: Severity::High,
        problem: "yaml.load without a safe loader can build arbitrary objects.",
        scenario: "A crafted YAML file runs code when it is read.",
        fix: "Use yaml.safe_load." },
    Rule { id: "tls-check-off", langs: &[], pattern: r"verify\s*=\s*False|rejectUnauthorized\s*:\s*false|InsecureSkipVerify\s*:\s*true|danger_accept_invalid_(certs|hostnames)\s*\(\s*true|NODE_TLS_REJECT_UNAUTHORIZED\W+0|CURLOPT_SSL_VERIFY(PEER|HOST)\W+(false|0)|\bcurl\b.*\s(-k|--insecure)\b", unless: None, severity: Severity::High,
        problem: "Certificate checking is turned off for a connection.",
        scenario: "Anyone on the network path can pose as the server and read or change the traffic.",
        fix: "Leave verification on; for a private CA, add that CA to the trust store instead." },
    Rule { id: "permissive-cors", langs: &[], pattern: r#"(?i)access-control-allow-origin\W{0,12}\*|allow_origins?\s*[=(]\s*\[?\s*["']\*["']|origin\s*:\s*["']\*["']|CorsLayer::permissive|allow_any_origin|AllowOrigin::any|\bcors\(\s*\)"#, unless: None, severity: Severity::Medium,
        problem: "Any website is allowed to call this service from a browser (CORS open to all origins).",
        scenario: "A page the user happens to visit can make requests as the user and read the answers.",
        fix: "List the origins that need access." },
    Rule { id: "chmod-777", langs: &[], pattern: r"chmod\s+(-\w+\s+)*0?777|chmod\([^)]*0o?777|set_mode\(\s*0o?777|\bmode\(\s*0o?777|FileMode\(0?777", unless: None, severity: Severity::High,
        problem: "A file or folder is made writable by everyone (mode 777).",
        scenario: "Any other account on the machine can replace the file with its own content.",
        fix: "Use the narrowest mode that works, such as 755 for programs or 600 for private files." },
    Rule { id: "pipe-to-shell", langs: &[], pattern: r"\b(curl|wget)\b[^|]*\|\s*(sudo\s+)?(ba|z)?sh\b", unless: None, severity: Severity::High,
        problem: "A script is downloaded and run in one step.",
        scenario: "If the server or the connection is compromised, its content runs here without anyone seeing it first.",
        fix: "Download to a file, check its checksum or signature, then run it." },
];

const CODE_EXTENSIONS: [&str; 26] = [
    "rs", "py", "js", "jsx", "ts", "tsx", "mjs", "cjs", "go", "java", "kt", "rb", "php", "cs", "c",
    "cc", "cpp", "h", "hpp", "swift", "sh", "bash", "zsh", "ps1", "yml", "yaml",
];

fn is_code(f: &FileDiff) -> bool {
    CODE_EXTENSIONS.contains(&f.extension().as_str())
        || matches!(f.file_name(), "Dockerfile" | "Makefile")
}

fn compiled_rules() -> &'static Vec<(Regex, Option<Regex>)> {
    static R: OnceLock<Vec<(Regex, Option<Regex>)>> = OnceLock::new();
    R.get_or_init(|| {
        RULES
            .iter()
            .map(|r| {
                (
                    Regex::new(r.pattern).expect("rule pattern"),
                    r.unless.map(|u| Regex::new(u).expect("rule exception")),
                )
            })
            .collect()
    })
}

fn is_comment(trimmed: &str) -> bool {
    ["//", "#", "*", "/*"]
        .iter()
        .any(|p| trimmed.starts_with(p))
}

/// SQL text with a value pasted in, as opposed to a `?` or `$1` placeholder.
fn sql_with_pasted_values(line: &str) -> bool {
    let sql = rx!(
        r#"(?i)\b(select\s+[\w\*,\s.`"]+\s+from|insert\s+into|delete\s+from|update\s+[\w.`"]+\s+set)\b"#
    );
    let pasted = rx!(
        r#"(["']\s*\+|\+\s*["']|\$\{|\.format\(|format!\(|\bf"[^"]*\{|\bf'[^']*\{|["']\s*%\s*[\w(])"#
    );
    sql.is_match(line) && pasted.is_match(line)
}

/// A fixed IPv4 address in quotes or a URL, other than loopback and "any".
fn hard_coded_ip(line: &str) -> Option<String> {
    let ip = rx!(r#"(?:["']|://)(\d{1,3}(?:\.\d{1,3}){3})(?::\d+)?(?:["'/]|$)"#);
    let found = ip.captures(line)?[1].to_string();
    let parts: Vec<u32> = found.split('.').filter_map(|p| p.parse().ok()).collect();
    let skip = parts.len() != 4
        || parts.iter().any(|&p| p > 255)
        || parts[0] == 127
        || parts[0] == 0
        || parts[0] == 255;
    (!skip).then_some(found)
}

/// Calls and settings that tend to become vulnerabilities, on lines the change adds.
pub fn dangerous_calls(files: &[FileDiff]) -> Vec<Finding> {
    let rules = compiled_rules();
    let mut out = vec![];
    for f in files.iter().filter(|f| is_code(f)) {
        let ext = f.extension();
        for l in f.added().filter(|l| !is_comment(l.text.trim())) {
            for (rule, (re, unless)) in RULES.iter().zip(rules) {
                let applies = rule.langs.is_empty() || rule.langs.contains(&ext.as_str());
                let excused = unless.as_ref().is_some_and(|u| u.is_match(&l.text));
                if applies && re.is_match(&l.text) && !excused {
                    out.push(finding(Hit {
                        file: f,
                        line: l.no,
                        severity: rule.severity,
                        rule: rule.id,
                        category: "dangerous-call",
                        problem: rule.problem.to_string(),
                        scenario: rule.scenario,
                        fix: rule.fix,
                        evidence: evidence(&l.text),
                    }));
                }
            }
            if sql_with_pasted_values(&l.text) {
                out.push(finding(Hit {
                    file: f,
                    line: l.no,
                    severity: Severity::High,
                    rule: "sql-concat",
                    category: "dangerous-call",
                    problem: "A SQL statement is built by pasting values into the text.".into(),
                    scenario: "A value such as `x' OR '1'='1` changes what the query does (SQL injection).",
                    fix: "Use query parameters (?, $1, :name) and pass the values separately.",
                    evidence: evidence(&l.text),
                }));
            }
            if let Some(ip) = hard_coded_ip(&l.text) {
                out.push(finding(Hit {
                    file: f,
                    line: l.no,
                    severity: Severity::Low,
                    rule: "hard-coded-ip",
                    category: "dangerous-call",
                    problem: format!("The address {ip} is written into the code."),
                    scenario: "It works on one network and fails (or reaches the wrong machine) on another.",
                    fix: "Read the address from configuration.",
                    evidence: evidence(&l.text),
                }));
            }
        }
    }
    out
}

// ---------------------------------------------------------- dependency changes

struct Dep {
    name: String,
    spec: String,
    line: usize,
    flags: Vec<(Severity, &'static str)>,
}

const WIDENED: &str = "version range widened to * (any release is accepted)";
const FROM_GIT: &str = "comes from git, not a published release";
const FROM_PATH: &str = "comes from a local path";

fn is_lock(name: &str) -> bool {
    matches!(name, "Cargo.lock" | "package-lock.json")
}

fn manifest_kind(f: &FileDiff) -> Option<&'static str> {
    let n = f.file_name();
    match n {
        "Cargo.toml" => Some("cargo"),
        "Cargo.lock" => Some("cargo-lock"),
        "package.json" => Some("npm"),
        "package-lock.json" => Some("npm-lock"),
        "go.mod" => Some("go"),
        _ if n.starts_with("requirements") && n.ends_with(".txt") => Some("pip"),
        _ => None,
    }
}

/// Which packages a change adds or changes, with a flag on the risky ones.
pub fn dependency_changes(files: &[FileDiff]) -> Vec<Finding> {
    let mut out = vec![];
    for f in files {
        let Some(kind) = manifest_kind(f) else {
            continue;
        };
        if is_lock(f.file_name()) {
            out.extend(lock_summary(f, kind));
            continue;
        }
        let (added, removed) = match kind {
            "cargo" => cargo_deps(f),
            "npm" => npm_deps(f),
            "go" => go_deps(f),
            _ => pip_deps(f),
        };
        for d in &added {
            let old = removed.iter().find(|r| r.name == d.name);
            if old.is_some_and(|o| o.spec == d.spec) {
                continue;
            }
            out.push(dep_finding(f, d, old.map(|o| o.spec.as_str())));
        }
    }
    out
}

fn dep_finding(f: &FileDiff, d: &Dep, old: Option<&str>) -> Finding {
    let plain = |s: &str| s.trim().trim_matches('"').to_string();
    let what = match old {
        Some(o) => format!(
            "Dependency changed: {} {} -> {}",
            d.name,
            plain(o),
            plain(&d.spec)
        ),
        None => format!("Dependency added: {} {}", d.name, plain(&d.spec)),
    };
    let severity = d
        .flags
        .iter()
        .map(|(s, _)| *s)
        .max()
        .unwrap_or(Severity::Low);
    let flags: Vec<&str> = d.flags.iter().map(|(_, w)| *w).collect();
    let problem = if flags.is_empty() {
        format!("{what}.")
    } else {
        format!("{what}; it {}.", flags.join(" and "))
    };
    finding(Hit {
        file: f,
        line: d.line,
        severity,
        rule: "dependency",
        category: "dependency",
        problem,
        scenario: "New code from outside the repository runs with this program's permissions; a git, path or unbounded version can change or vanish without a review.",
        fix: "Check the package (maintainers, size, need), pin a released version, and avoid git or path sources in shared code.",
        evidence: head_chars(d.spec.trim(), 200),
    })
}

fn bad_spec_flags(spec: &str) -> Vec<(Severity, &'static str)> {
    let s = spec
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == ',');
    let mut flags = vec![];
    if s == "*"
        || s == "latest"
        || s.contains("version = \"*\"")
        || s.contains("\"version\": \"*\"")
    {
        flags.push((Severity::Medium, WIDENED));
    }
    if s.contains("git =") || s.contains("git+") || s.starts_with("github:") || s.contains("@ git+")
    {
        flags.push((Severity::Medium, FROM_GIT));
    }
    if s.contains("path =") || s.starts_with("file:") || s.starts_with("link:") {
        flags.push((Severity::Low, FROM_PATH));
    }
    flags
}

fn cargo_deps(f: &FileDiff) -> (Vec<Dep>, Vec<Dep>) {
    let section_rx = rx!(r"^\s*\[([^\]]+)\]\s*$");
    let dep_rx = rx!(r"^\s*([A-Za-z0-9_-]+)\s*=\s*(.+?)\s*$");
    let versionish = rx!(r#"^("[\^~=<>*0-9]|\{.*(version|git|path))"#);
    let not_deps = [
        "name",
        "version",
        "edition",
        "authors",
        "description",
        "license",
        "readme",
        "repository",
        "homepage",
        "documentation",
        "keywords",
        "categories",
        "publish",
        "build",
        "links",
        "default-run",
        "resolver",
        "rust-version",
        "include",
        "exclude",
        "features",
    ];
    let (mut added, mut removed) = (vec![], vec![]);
    let mut section = String::new();
    for l in f.lines() {
        if l.kind != '-' {
            if let Some(c) = section_rx.captures(&l.text) {
                section = c[1].to_string();
                continue;
            }
        }
        if l.kind == ' ' {
            continue;
        }
        let Some(c) = dep_rx.captures(&l.text) else {
            continue;
        };
        let (key, value) = (c[1].to_string(), c[2].to_string());
        let in_deps = section.contains("dependenc");
        // `[dependencies.serde]` followed by `version = ".."`, `git = ".."`: the table names the package
        let table_name = section
            .split_once("dependencies.")
            .map(|(_, n)| n.to_string());
        let dep = if let Some(name) = table_name {
            matches!(key.as_str(), "version" | "git" | "path").then(|| Dep {
                name,
                spec: format!("{key} = {value}"),
                line: l.no,
                flags: bad_spec_flags(&format!("{key} = {value}")),
            })
        } else if (in_deps || section.is_empty())
            && !not_deps.contains(&key.as_str())
            && (in_deps || versionish.is_match(&value))
        {
            Some(Dep {
                name: key,
                spec: value.clone(),
                line: l.no,
                flags: bad_spec_flags(&value),
            })
        } else {
            None
        };
        if let Some(d) = dep {
            if l.kind == '+' {
                added.push(d)
            } else {
                removed.push(d)
            }
        }
    }
    (added, removed)
}

fn npm_deps(f: &FileDiff) -> (Vec<Dep>, Vec<Dep>) {
    let open = rx!(
        r#"^\s*"(dependencies|devDependencies|peerDependencies|optionalDependencies)"\s*:\s*\{"#
    );
    let close = rx!(r"^\s*\}");
    let entry = rx!(r#"^\s*"([^"]+)"\s*:\s*"([^"]*)"\s*,?\s*$"#);
    let spec_like =
        rx!(r"^([\^~<>=*]|\d|x$|latest$|git|github:|file:|link:|https?:|[\w.-]+/[\w.-]+(#.*)?$)");
    let meta = [
        "name",
        "version",
        "description",
        "main",
        "module",
        "type",
        "license",
        "author",
        "homepage",
        "types",
        "node",
        "npm",
        "yarn",
        "pnpm",
        "repository",
        "bugs",
        "url",
    ];
    let (mut added, mut removed) = (vec![], vec![]);
    let mut in_deps: Option<bool> = None;
    for l in f.lines() {
        if l.kind != '-' {
            if open.is_match(&l.text) {
                in_deps = Some(true);
                continue;
            }
            if close.is_match(&l.text) {
                in_deps = Some(false);
                continue;
            }
        }
        if l.kind == ' ' {
            continue;
        }
        let Some(c) = entry.captures(&l.text) else {
            continue;
        };
        let (name, spec) = (c[1].to_string(), c[2].to_string());
        let counts = match in_deps {
            Some(inside) => inside,
            None => !meta.contains(&name.as_str()) && spec_like.is_match(&spec),
        };
        if counts {
            let d = Dep {
                flags: bad_spec_flags(&spec),
                name,
                spec,
                line: l.no,
            };
            if l.kind == '+' {
                added.push(d)
            } else {
                removed.push(d)
            }
        }
    }
    (added, removed)
}

fn go_deps(f: &FileDiff) -> (Vec<Dep>, Vec<Dep>) {
    let req = rx!(r"^\s*(?:require\s+)?([\w.~-]+\.[\w.~/-]+)\s+(v[\w.+-]+)");
    let replace = rx!(r"^\s*(?:replace\s+)?([\w.~/-]+)(?:\s+v[\w.+-]+)?\s*=>\s*(\S+)");
    let (mut added, mut removed) = (vec![], vec![]);
    for l in f.lines().filter(|l| l.kind != ' ') {
        let dep = if let Some(c) = replace.captures(&l.text) {
            let target = c[2].to_string();
            let local = target.starts_with('.') || target.starts_with('/');
            let flag = if local {
                (Severity::Low, FROM_PATH)
            } else {
                (Severity::Medium, "is replaced by a different module")
            };
            Dep {
                name: c[1].to_string(),
                spec: format!("=> {target}"),
                line: l.no,
                flags: vec![flag],
            }
        } else if let Some(c) = req.captures(&l.text) {
            Dep {
                name: c[1].to_string(),
                spec: c[2].to_string(),
                line: l.no,
                flags: vec![],
            }
        } else {
            continue;
        };
        if l.kind == '+' {
            added.push(dep)
        } else {
            removed.push(dep)
        }
    }
    (added, removed)
}

fn pip_deps(f: &FileDiff) -> (Vec<Dep>, Vec<Dep>) {
    let pinned = rx!(r"^([A-Za-z0-9_.\[\]-]+)\s*(==|>=|<=|~=|!=|<|>|===)\s*(.+)$");
    let (mut added, mut removed) = (vec![], vec![]);
    for l in f.lines().filter(|l| l.kind != ' ') {
        let t = l.text.split('#').next().unwrap_or("").trim();
        if t.is_empty() || (t.starts_with('-') && !t.starts_with("-e")) {
            continue;
        }
        let dep = if let Some(c) = pinned.captures(t) {
            let spec = format!("{}{}", &c[2], c[3].trim());
            Dep {
                name: c[1].to_string(),
                flags: bad_spec_flags(c[3].trim()),
                spec,
                line: l.no,
            }
        } else if t.starts_with("-e") || t.contains("git+") || t.contains(" @ ") {
            let mut flags = bad_spec_flags(t);
            if flags.is_empty() {
                flags.push((Severity::Low, FROM_PATH));
            }
            Dep {
                name: t.to_string(),
                spec: t.to_string(),
                line: l.no,
                flags,
            }
        } else {
            Dep {
                name: t.to_string(),
                spec: "(no version)".into(),
                line: l.no,
                flags: vec![(Severity::Low, "has no version pin")],
            }
        };
        if l.kind == '+' {
            added.push(dep)
        } else {
            removed.push(dep)
        }
    }
    (added, removed)
}

/// A lock file can change hundreds of lines; say how many packages moved and name the first few,
/// and report each git source on its own.
fn lock_summary(f: &FileDiff, kind: &str) -> Vec<Finding> {
    let (name_rx, ver_rx) = if kind == "cargo-lock" {
        (rx!(r#"^name = "([^"]+)""#), rx!(r#"^version = "([^"]+)""#))
    } else {
        (
            rx!(r#"^\s*"node_modules/([^"]+)"\s*:\s*\{"#),
            rx!(r#"^\s*"version"\s*:\s*"([^"]+)""#),
        )
    };
    let git_rx = rx!(r#"^\s*(source = "git\+|"resolved"\s*:\s*"(git\+|github:|http:))"#);
    let mut current = String::new();
    let mut moved: Vec<(String, String)> = vec![];
    let mut git_hits = vec![];
    let mut first_line = 0;
    for l in f.lines() {
        if let Some(c) = name_rx.captures(&l.text) {
            current = c[1].to_string();
        }
        if l.kind == '+' {
            if let Some(c) = ver_rx.captures(&l.text) {
                if first_line == 0 {
                    first_line = l.no;
                }
                moved.push((current.clone(), c[1].to_string()));
            }
            if git_rx.is_match(&l.text) {
                git_hits.push((current.clone(), l.no, evidence(&l.text)));
            }
        }
    }
    let mut out = vec![];
    if !moved.is_empty() {
        let names: Vec<String> = moved
            .iter()
            .take(12)
            .map(|(n, v)| format!("{n} {v}"))
            .collect();
        let more = moved.len().saturating_sub(12);
        let tail = if more > 0 {
            format!(", and {more} more")
        } else {
            String::new()
        };
        out.push(finding(Hit {
            file: f,
            line: first_line,
            severity: Severity::Low,
            rule: "lock-file",
            category: "dependency",
            problem: format!("{} locked package(s) added or changed: {}{tail}.", moved.len(), names.join(", ")),
            scenario: "The exact code that gets installed changed, even if no manifest line did.",
            fix: "Skim the list for packages you did not mean to add; update lock files on purpose, not as a side effect.",
            evidence: names.first().cloned().unwrap_or_default(),
        }));
    }
    for (name, line, ev) in git_hits {
        out.push(finding(Hit {
            file: f,
            line,
            severity: Severity::Medium,
            rule: "lock-git-source",
            category: "dependency",
            problem: format!("{name} is locked to a git or plain-http source."),
            scenario: "A moving branch or an unencrypted download can change what gets installed.",
            fix: "Depend on a published release over https.",
            evidence: ev,
        }));
    }
    out
}

// ------------------------------------------------------------------ weak tests

fn is_test_file(f: &FileDiff) -> bool {
    let test_path = rx!(
        r"(?i)(^|/)(tests?|__tests__|specs?)/|[._-]test\.|[._]spec\.|(^|/)test_[^/]*\.py$|_test\.(go|py|rs)$|Tests?\.(java|kt|cs)$"
    );
    // a Rust file with inline tests: any line of the change that opens or sits in a test module
    let rust_inline = f.extension() == "rs"
        && f.lines().any(|l| {
            l.text.contains("#[test]")
                || l.text.contains("#[cfg(test)]")
                || l.text.trim() == "mod tests {"
        });
    test_path.is_match(&f.path) || rust_inline
}

fn is_assertion(text: &str) -> bool {
    let call = rx!(
        r"\bassert\w*!?\s*[(\[]|\bexpect\s*\(|self\.assert\w+\s*\(|\.should\b|\bt\.(Error|Errorf|Fatal|Fatalf)\(|\brequire\.\w+\("
    );
    call.is_match(text) || rx!(r"^\s*assert\s").is_match(text)
}

fn is_print(text: &str) -> bool {
    rx!(r"\b(println!|print!|eprintln!|dbg!|print\s*\(|console\.(log|info)\s*\(|fmt\.Print\w*\(|System\.out\.print\w*\()")
        .is_match(text)
}

fn is_skip_marker(text: &str) -> bool {
    rx!(r"#\[ignore\b|\b(it|test|describe)\.skip\b|\b(xit|xdescribe|xtest)\s*\(|@pytest\.mark\.(skip|xfail)|@unittest\.skip|\bpytest\.skip\s*\(|\bt\.Skip(f|Now)?\(|@Disabled\b|@Ignore\b")
        .is_match(text)
}

/// Tests that got weaker in the same change: assertions deleted, tests skipped or ignored,
/// an assertion swapped for a print.
pub fn weak_tests(files: &[FileDiff]) -> Vec<Finding> {
    let mut out = vec![];
    for f in files.iter().filter(|f| is_test_file(f)) {
        let weak = |line: usize, sev, rule: &str, problem: String, ev: &str| {
            finding(Hit {
                file: f,
                line,
                severity: sev,
                rule,
                category: "weak-test",
                problem,
                scenario: "The suite stays green while the behaviour it used to prove is no longer checked, so a later bug ships unnoticed.",
                fix: "Keep the assertion (fix the code or the expectation), or delete the test on purpose and say why in the commit.",
                evidence: evidence(ev),
            })
        };
        for h in &f.hunks {
            let gone: Vec<&DLine> = h
                .lines
                .iter()
                .filter(|l| l.kind == '-' && is_assertion(&l.text))
                .collect();
            let new_asserts = h
                .lines
                .iter()
                .filter(|l| l.kind == '+' && is_assertion(&l.text))
                .count();
            let prints = h.lines.iter().find(|l| l.kind == '+' && is_print(&l.text));
            if let (Some(first), Some(p)) = (gone.first(), prints) {
                if new_asserts < gone.len() {
                    out.push(weak(
                        p.no,
                        Severity::High,
                        "assert-became-print",
                        format!(
                            "An assertion near line {} was replaced by a print.",
                            first.no
                        ),
                        &p.text,
                    ));
                }
            }
        }
        let removed: Vec<&DLine> = f.removed().filter(|l| is_assertion(&l.text)).collect();
        let added = f.added().filter(|l| is_assertion(&l.text)).count();
        if let Some(first) = removed.first().filter(|_| removed.len() > added) {
            out.push(weak(
                first.no,
                Severity::High,
                "assertions-removed",
                format!(
                    "{} assertion(s) removed and {added} added in {}.",
                    removed.len(),
                    f.path
                ),
                &first.text,
            ));
        }
        for l in f.added().filter(|l| is_skip_marker(&l.text)) {
            out.push(weak(
                l.no,
                Severity::Medium,
                "test-skipped",
                "A test is skipped or ignored in this change.".to_string(),
                &l.text,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::diff::{parse, whole_file};
    use super::*;

    /// A one-file diff whose added lines are `added` and removed lines are `removed`.
    fn change(path: &str, removed: &[&str], added: &[&str]) -> Vec<FileDiff> {
        let mut d = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n");
        d.push_str(&format!(
            "@@ -1,{} +1,{} @@\n",
            removed.len().max(1),
            added.len().max(1)
        ));
        for r in removed {
            d.push_str(&format!("-{r}\n"));
        }
        for a in added {
            d.push_str(&format!("+{a}\n"));
        }
        parse(&d)
    }

    fn rules(found: &[Finding]) -> Vec<&str> {
        found.iter().map(|f| f.rule.as_str()).collect()
    }

    #[test]
    fn finds_tokens_keys_and_random_assignments() {
        let token = format!("sk-{}", "a1B2c3".repeat(5));
        let key_block = format!("-----BEGIN {} KEY-----", "RSA PRIVATE");
        let random = "Zk3p9Qx7Lm2Vb8Nc4Rt6";
        let files = change(
            "src/x.py",
            &[],
            &[
                &format!("client = Client(\"{token}\")"),
                &key_block,
                &format!("api_key = \"{random}\""),
                "db_password = \"hunter2x\"",
            ],
        );
        let found = secrets(&files);
        assert_eq!(
            rules(&found),
            [
                "token-shape",
                "private-key",
                "high-entropy",
                "hard-coded-credential"
            ]
        );
        assert_eq!(found[1].severity, Severity::Critical);
        // evidence never carries the whole secret
        assert!(found
            .iter()
            .all(|f| !f.evidence.contains(&token) && !f.evidence.contains(random)));
        assert!(found[0].evidence.contains("sk-a"));
    }

    #[test]
    fn secrets_ignores_placeholders_names_and_removed_lines() {
        let files = change(
            "src/x.py",
            &["old_key = \"Zk3p9Qx7Lm2Vb8Nc4Rt6\""],
            &[
                "api_key = os.environ[\"API_KEY\"]",
                "api_key = \"your-api-key-here\"",
                "token = \"${TOKEN}\"",
                "password_field = \"\"",
                "token_count = 123456789",
            ],
        );
        assert!(secrets(&files).is_empty());
    }

    #[test]
    fn flags_a_committed_env_file_but_not_its_example() {
        let mut env = whole_file(".env", "A=1\n", FileStatus::Added);
        env.unread = true;
        let example = whole_file(".env.example", "A=\n", FileStatus::Added);
        let found = secrets(&[env, example]);
        assert_eq!(rules(&found), ["secret-file"]);
        assert_eq!(found[0].file, ".env");
    }

    #[test]
    fn finds_each_dangerous_call() {
        let cases: [(&str, &str, &str); 14] = [
            ("a.py", "x = eval(user_text)", "eval"),
            ("a.py", "exec(code)", "python-exec"),
            ("a.py", "subprocess.run(cmd, shell=True)", "shell-true"),
            ("a.py", "os.system(\"ls \" + name)", "os-system"),
            (
                "a.py",
                "subprocess.run(f\"ls {name}\")",
                "python-command-string",
            ),
            ("a.py", "cfg = yaml.load(text)", "yaml-load"),
            ("a.py", "obj = pickle.loads(blob)", "unsafe-deserialize"),
            ("a.js", "execSync(`rm ${dir}`)", "node-exec-string"),
            ("a.rs", "let r = unsafe { *p };", "rust-unsafe"),
            (
                "a.rs",
                "Command::new(\"sh\").args([\"-c\", &format!(\"ls {d}\")]);",
                "rust-command-string",
            ),
            ("a.py", "requests.get(u, verify=False)", "tls-check-off"),
            (
                "a.js",
                "res.setHeader('Access-Control-Allow-Origin', '*')",
                "permissive-cors",
            ),
            ("a.sh", "chmod -R 777 /srv/data", "chmod-777"),
            (
                "a.sh",
                "curl https://x.example/i.sh | sudo bash",
                "pipe-to-shell",
            ),
        ];
        for (path, line, rule) in cases {
            let found = dangerous_calls(&change(path, &[], &[line]));
            assert!(
                rules(&found).contains(&rule),
                "{line} should trigger {rule}, got {:?}",
                rules(&found)
            );
        }
    }

    #[test]
    fn finds_sql_built_by_pasting_and_a_hard_coded_ip() {
        let sql = [
            "q = \"SELECT * FROM users WHERE id = \" + uid",
            "q = f\"DELETE FROM t WHERE name = '{name}'\"",
            "let q = format!(\"UPDATE t SET a = {}\", a);",
        ];
        for s in sql {
            let found = dangerous_calls(&change("a.py", &[], &[s]));
            assert!(rules(&found).contains(&"sql-concat"), "{s}");
        }
        let ip = dangerous_calls(&change("a.py", &[], &["HOST = \"10.20.30.40\""]));
        assert_eq!(rules(&ip), ["hard-coded-ip"]);
        assert_eq!(ip[0].severity, Severity::Low);
    }

    #[test]
    fn leaves_safe_code_alone() {
        let safe = [
            (
                "a.py",
                "cur.execute(\"SELECT * FROM users WHERE id = ?\", (uid,))",
            ),
            ("a.py", "model.eval()"),
            ("a.py", "cfg = yaml.load(text, Loader=yaml.SafeLoader)"),
            ("a.js", "const m = re.exec(line)"),
            ("a.py", "# eval(x) is dangerous"),
            ("a.py", "requests.get(u, verify=True)"),
            ("a.py", "HOST = \"127.0.0.1\""),
            ("a.rs", "// unsafe { } would be wrong here"),
            ("a.sh", "chmod 755 run.sh"),
        ];
        for (path, line) in safe {
            assert!(
                dangerous_calls(&change(path, &[], &[line])).is_empty(),
                "{line}"
            );
        }
        // files that are not code are not scanned for calls
        assert!(dangerous_calls(&change("notes.md", &[], &["use eval(x) never"])).is_empty());
    }

    #[test]
    fn lists_cargo_changes_and_flags_git_path_and_star() {
        let diff = "diff --git a/Cargo.toml b/Cargo.toml
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -5,6 +5,10 @@
 [dependencies]
 serde = \"1.0\"
-regex = \"1.10\"
+regex = \"1.12\"
+tokio = \"*\"
+mine = { path = \"../mine\" }
+fresh = { git = \"https://example.invalid/fresh\" }
+small = \"0.3\"
";
        let found = dependency_changes(&parse(diff));
        assert_eq!(found.len(), 5);
        let by = |s: &str| found.iter().find(|f| f.problem.contains(s)).unwrap();
        assert!(
            by("regex 1.10").problem.contains("changed") && by("regex").severity == Severity::Low
        );
        assert_eq!(by("tokio").severity, Severity::Medium);
        assert!(by("tokio").problem.contains("widened to *"));
        assert_eq!(by("mine").severity, Severity::Low);
        assert_eq!(by("fresh").severity, Severity::Medium);
        assert!(by("small").problem.starts_with("Dependency added: small"));
    }

    #[test]
    fn manifest_sections_do_not_look_like_dependencies() {
        let diff = "diff --git a/Cargo.toml b/Cargo.toml
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -1,4 +1,4 @@
 [package]
-version = \"1.0.0\"
+version = \"1.1.0\"
 [lib]
+path = \"src/lib.rs\"
";
        assert!(dependency_changes(&parse(diff)).is_empty());
    }

    #[test]
    fn reads_npm_pip_go_and_lock_files() {
        let pkg = change(
            "package.json",
            &[],
            &[
                "  \"dependencies\": {",
                "    \"left-pad\": \"*\",",
                "    \"fork\": \"github:someone/fork\",",
                "  },",
            ],
        );
        let found = dependency_changes(&pkg);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|f| f.severity == Severity::Medium));

        let pip = dependency_changes(&change(
            "requirements.txt",
            &[],
            &[
                "requests==2.31.0",
                "-e git+https://example.invalid/x.git#egg=x",
                "flask",
            ],
        ));
        assert_eq!(pip.len(), 3);
        assert_eq!(pip[0].severity, Severity::Low);
        assert_eq!(pip[1].severity, Severity::Medium);

        let go = dependency_changes(&change(
            "go.mod",
            &[],
            &[
                "require github.com/pkg/errors v0.9.1",
                "replace github.com/a/b => ../b",
            ],
        ));
        assert_eq!(go.len(), 2);

        let lock = change(
            "Cargo.lock",
            &[],
            &[
                "name = \"serde\"",
                "version = \"1.0.1\"",
                "source = \"git+https://example.invalid/serde\"",
            ],
        );
        let found = dependency_changes(&lock);
        assert_eq!(found.len(), 2);
        assert!(
            found[0].problem.contains("1 locked package(s)")
                && found[0].problem.contains("serde 1.0.1")
        );
        assert_eq!(found[1].severity, Severity::Medium);
        // a source file named like nothing in the list is ignored
        assert!(dependency_changes(&change("src/lib.rs", &[], &["serde = \"*\""])).is_empty());
    }

    #[test]
    fn catches_deleted_assertions_prints_and_skips() {
        let removed = change(
            "tests/api_test.py",
            &[
                "    assert resp.status == 200",
                "    assert resp.json()[\"ok\"]",
            ],
            &["    assert resp.status == 200"],
        );
        let found = weak_tests(&removed);
        assert_eq!(rules(&found), ["assertions-removed"]);
        assert_eq!(found[0].severity, Severity::High);

        let print = change(
            "tests/api_test.py",
            &["    assert total == 3"],
            &["    print(total)"],
        );
        assert!(rules(&weak_tests(&print)).contains(&"assert-became-print"));

        for (path, line) in [
            ("tests/a.test.js", "it.skip('works', () => {})"),
            ("tests/test_a.py", "@pytest.mark.skip(reason='later')"),
            ("src/lib.rs", "#[ignore]"),
            ("tests/a.spec.ts", "xit('works', () => {})"),
        ] {
            let mut file = change(path, &[], &[line, "#[test]"]);
            file[0].path = path.to_string();
            assert!(
                rules(&weak_tests(&file)).contains(&"test-skipped"),
                "{path}: {line}"
            );
        }
    }

    #[test]
    fn healthy_test_changes_are_quiet() {
        let more = change(
            "tests/a_test.py",
            &["    assert a == 1"],
            &["    assert a == 1", "    assert b == 2"],
        );
        assert!(weak_tests(&more).is_empty());
        // assertions removed from ordinary code are not a test problem
        let prod = change("src/lib.py", &["    assert x > 0"], &[]);
        assert!(weak_tests(&prod).is_empty());
        // a Rust file with no tests in the change
        let rs = change("src/lib.rs", &["assert!(x)"], &["let y = 1;"]);
        assert!(weak_tests(&rs).is_empty());
    }
}
