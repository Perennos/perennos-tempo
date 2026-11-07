//! Custom nodes are Python that runs inside ComfyUI with the owner's own rights, so Stage treats
//! them as risky. Before a node pack is put in place its code gets a look: Stage's own quick
//! scan (it knows what ComfyUI nodes do and what they should not) and Soundcheck, the agent's
//! scanners, when the agent is installed. A high finding blocks the install until the owner
//! types "yes" after reading it. A node that calls a paid or cloud service is refused unless
//! the owner turned that on for that workflow. Nothing leaves the computer by default.

use crate::setup::json::{parse, J};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sev {
    Low,
    Medium,
    High,
    Critical,
}

impl Sev {
    pub fn name(self) -> &'static str {
        match self {
            Sev::Low => "low",
            Sev::Medium => "medium",
            Sev::High => "high",
            Sev::Critical => "critical",
        }
    }

    fn parse(s: &str) -> Option<Sev> {
        match s {
            "low" => Some(Sev::Low),
            "medium" => Some(Sev::Medium),
            "high" => Some(Sev::High),
            "critical" => Some(Sev::Critical),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub sev: Sev,
    pub rule: String,
    pub file: String,
    pub line: u32,
    pub what: String,
    /// Who found it: Stage's own scan or Soundcheck.
    pub by: &'static str,
}

/// The rule name of a finding that means "this calls a paid or cloud service".
pub const CLOUD_RULE: &str = "cloud-api";

// ---- partner nodes in a graph

/// Partner nodes are ComfyUI's wrappers around paid services (Kling, OpenAI, Veo and so on).
/// When ComfyUI is there it says so itself (`api_node` in `/object_info`); this list is for the
/// check before there is a ComfyUI to ask.
pub fn is_api_class(class: &str) -> bool {
    const STARTS: &[&str] = &[
        "Kling",
        "OpenAI",
        "Gemini",
        "Runway",
        "Veo",
        "Luma",
        "Pika",
        "Pixverse",
        "Vidu",
        "Ideogram",
        "Recraft",
        "Stability",
        "BFL",
        "FluxPro",
        "FluxUltra",
        "Tripo",
        "Rodin",
        "Moonvalley",
        "ByteDance",
        "Sora",
        "Hunyuan3D",
        "MinimaxText",
        "MinimaxImage",
        "MinimaxVideo",
        "Magnific",
        "Topaz",
        "Grok",
        "Anthropic",
    ];
    STARTS.iter().any(|s| class.starts_with(s)) || class.contains("ApiNode")
}

/// The classes of a graph that call a paid or cloud service, by name.
pub fn api_classes_in(graph: &J) -> Vec<String> {
    let mut v: Vec<String> = match graph {
        J::Obj(o) => o
            .iter()
            .filter_map(|(_, n)| n.str("class_type"))
            .filter(|c| is_api_class(c))
            .map(String::from)
            .collect(),
        _ => vec![],
    };
    v.sort();
    v.dedup();
    v
}

pub fn classes_in(graph: &J) -> Vec<String> {
    let mut v: Vec<String> = match graph {
        J::Obj(o) => o
            .iter()
            .filter_map(|(_, n)| n.str("class_type"))
            .map(String::from)
            .collect(),
        _ => vec![],
    };
    v.sort();
    v.dedup();
    v
}

// ---- Stage's own scan of node code

const MAX_FILES: usize = 600;
const MAX_FILE: u64 = 1 << 20;

fn py_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        // a link could point out of the pack: never followed
        let Ok(t) = e.file_type() else { continue };
        if t.is_symlink() {
            continue;
        }
        if t.is_dir() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n != ".git" && n != "__pycache__" && n != "node_modules" {
                py_files(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "py") && out.len() < MAX_FILES {
            out.push(p);
        }
    }
}

fn word_at(line: &str, pat: &str) -> bool {
    // `eval(` is a call, `model.eval()` and `my_eval(` are not
    line.match_indices(pat).any(|(i, _)| {
        line[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '.'))
    })
}

const NET: &[&str] = &[
    "requests.get(",
    "requests.post(",
    "requests.put(",
    "requests.patch(",
    "requests.delete(",
    "requests.request(",
    "requests.Session(",
    "urlopen(",
    "urlretrieve(",
    "httpx.",
    "aiohttp.ClientSession",
    "http.client",
    "create_connection(",
    "websocket.WebSocket(",
    "hf_hub_download(",
    "snapshot_download(",
];

const KEYS: &[&str] = &[
    "api_key",
    "API_KEY",
    "apikey",
    "Bearer ",
    "X-API-Key",
    "x-api-key",
    "Authorization",
];

/// Hosts a URL in a line names, and whether every one is this computer.
fn only_local(text: &str) -> bool {
    let mut any = false;
    for (i, _) in text.match_indices("://") {
        let rest = &text[i + 3..];
        let host: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
            .collect();
        let h = host.split(':').next().unwrap_or("");
        any = true;
        if !(h == "localhost"
            || h == "127.0.0.1"
            || h == "0.0.0.0"
            || h == "::1"
            || h == "["
            || h.is_empty())
        {
            return false;
        }
    }
    any
}

fn model_download_only(text: &str) -> bool {
    !text.contains("requests.")
        && !text.contains("httpx.")
        && !text.contains("aiohttp")
        && (text.contains("hf_hub_download(") || text.contains("snapshot_download("))
}

/// Read every Python file of a node pack and say what it does that a node should not need to.
pub fn scan_dir(dir: &Path) -> Vec<Finding> {
    let mut files = vec![];
    py_files(dir, &mut files);
    let mut out = vec![];
    for f in files {
        if std::fs::metadata(&f).map_or(true, |m| m.len() > MAX_FILE) {
            out.push(Finding {
                sev: Sev::Low,
                rule: "too-big".into(),
                file: rel(dir, &f),
                line: 0,
                what: "a Python file over 1 MB that was not read".into(),
                by: "stage scan",
            });
            continue;
        }
        let Ok(body) = std::fs::read(&f) else {
            continue;
        };
        let body = String::from_utf8_lossy(&body).into_owned();
        scan_text(&rel(dir, &f), &body, &mut out);
    }
    out
}

fn rel(root: &Path, f: &Path) -> String {
    f.strip_prefix(root)
        .unwrap_or(f)
        .to_string_lossy()
        .replace('\\', "/")
}

fn scan_text(file: &str, body: &str, out: &mut Vec<Finding>) {
    let mut add = |sev: Sev, rule: &str, line: usize, what: &str| {
        out.push(Finding {
            sev,
            rule: rule.into(),
            file: file.into(),
            line: line as u32,
            what: what.into(),
            by: "stage scan",
        })
    };
    let has_key = KEYS.iter().any(|k| body.contains(k));
    let net_line = body
        .lines()
        .enumerate()
        .find(|(_, l)| !l.trim_start().starts_with('#') && NET.iter().any(|p| l.contains(p)));
    if let Some((n, _)) = net_line {
        let reaches_out = !only_local(body);
        if reaches_out && has_key && !model_download_only(body) {
            add(
                Sev::High,
                CLOUD_RULE,
                n + 1,
                "calls a service over the internet and sends a key: a paid or cloud API",
            );
        } else if reaches_out {
            if model_download_only(body) {
                add(
                    Sev::Low,
                    "net-call",
                    n + 1,
                    "downloads model files by itself",
                );
            } else {
                add(Sev::Medium, "net-call", n + 1, "talks to the internet");
            }
        }
    }
    for (i, l) in body.lines().enumerate() {
        let t = l.trim_start();
        if t.starts_with('#') {
            continue;
        }
        let n = i + 1;
        if l.contains("os.system(") || l.contains("os.popen(") || l.contains("pty.spawn(") {
            add(Sev::High, "shell-exec", n, "runs a shell command");
        } else if word_at(l, "eval(") || word_at(l, "exec(") {
            add(
                Sev::High,
                "dynamic-code",
                n,
                "runs text as code (eval or exec)",
            );
        } else if l.contains("subprocess.") {
            add(Sev::Medium, "subprocess", n, "starts another program");
        }
        if l.contains("pip install")
            || l.contains("pip.main(")
            || l.contains("\"-m\", \"pip\"")
            || l.contains("'-m', 'pip'")
        {
            add(
                Sev::High,
                "pip-install",
                n,
                "installs Python packages by itself",
            );
        }
        if [
            ".ssh",
            "id_rsa",
            ".aws/credentials",
            ".gnupg",
            "Login Data",
            "wallet.dat",
        ]
        .iter()
        .any(|s| l.contains(s))
        {
            add(
                Sev::High,
                "secrets",
                n,
                "reaches for files that hold passwords or keys",
            );
        }
        if l.contains("shutil.rmtree(") {
            add(
                Sev::Medium,
                "delete",
                n,
                "deletes folders (Stage never deletes; a node that does is worth a look)",
            );
        }
    }
}

// ---- Soundcheck, the agent's scanners

pub trait Soundcheck {
    fn run(&self, dir: &Path) -> Result<Vec<Finding>, String>;
}

/// `tempo-local-agent soundcheck --all . --scanners-only --json`, in the folder.
pub struct AgentSoundcheck {
    pub exe: Option<PathBuf>,
    pub limit: Duration,
}

impl Soundcheck for AgentSoundcheck {
    fn run(&self, dir: &Path) -> Result<Vec<Finding>, String> {
        let exe = self
            .exe
            .as_ref()
            .ok_or("the Tempo agent is not installed here, so Soundcheck did not run")?;
        let log = dir.with_extension("soundcheck.json");
        let out = std::fs::File::create(&log).map_err(|e| e.to_string())?;
        let mut child = Command::new(exe)
            .args(["soundcheck", "--all", ".", "--scanners-only", "--json"])
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start Soundcheck: {e}"))?;
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() > self.limit => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&log);
                    return Err(format!(
                        "Soundcheck did not finish in {} s",
                        self.limit.as_secs()
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(e.to_string()),
            }
        }
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        let start = text
            .find('{')
            .ok_or("Soundcheck said nothing Stage could read")?;
        read_report(&text[start..])
    }
}

/// Soundcheck's JSON report, as findings.
pub fn read_report(json: &str) -> Result<Vec<Finding>, String> {
    let j = parse(json).map_err(|e| format!("Soundcheck's report could not be read ({e})"))?;
    if j.get("findings").is_none() {
        return Err("Soundcheck's report had no findings list".into());
    }
    Ok(j.arr("findings")
        .iter()
        .filter_map(|f| {
            Some(Finding {
                sev: Sev::parse(f.str("severity")?)?,
                rule: f.str("rule").unwrap_or("soundcheck").to_string(),
                file: f.str("file").unwrap_or("").to_string(),
                line: f.f64("line").unwrap_or(0.0) as u32,
                what: f.str("problem").unwrap_or("").to_string(),
                by: "soundcheck",
            })
        })
        .collect())
}

// ---- the review

pub struct Review {
    pub findings: Vec<Finding>,
    /// Why Soundcheck did not run, if it did not. Stage's own scan always runs.
    pub soundcheck_note: Option<String>,
}

impl Review {
    pub fn worst(&self) -> Option<Sev> {
        self.findings.iter().map(|f| f.sev).max()
    }

    /// High or critical: blocks until the owner types yes.
    pub fn blocks(&self) -> bool {
        self.worst().is_some_and(|s| s >= Sev::High)
    }

    /// Something in the code calls a paid or cloud service.
    pub fn cloud(&self) -> bool {
        self.findings.iter().any(|f| f.rule == CLOUD_RULE)
    }
}

pub fn review(dir: &Path, sc: &dyn Soundcheck) -> Review {
    let mut findings = scan_dir(dir);
    let soundcheck_note = match sc.run(dir) {
        Ok(f) => {
            findings.extend(f);
            None
        }
        Err(e) => Some(e),
    };
    findings.sort_by(|a, b| {
        b.sev
            .cmp(&a.sev)
            .then(a.file.cmp(&b.file))
            .then(a.line.cmp(&b.line))
    });
    findings.dedup_by(|a, b| {
        a.sev == b.sev && a.file == b.file && a.line == b.line && a.rule == b.rule
    });
    Review {
        findings,
        soundcheck_note,
    }
}

/// The findings, worst first, a dozen of each kind at most.
pub fn show(r: &Review, who: &str, out: &mut dyn Write) {
    if r.findings.is_empty() {
        let _ = writeln!(out, "  {who}: no findings.");
    }
    for sev in [Sev::Critical, Sev::High, Sev::Medium, Sev::Low] {
        let of: Vec<&Finding> = r.findings.iter().filter(|f| f.sev == sev).collect();
        if of.is_empty() {
            continue;
        }
        let _ = writeln!(out, "  {who}: {} {}", of.len(), sev.name().to_uppercase());
        for f in of.iter().take(12) {
            let at = if f.line > 0 {
                format!("{}:{}", f.file, f.line)
            } else {
                f.file.clone()
            };
            let _ = writeln!(out, "    {at}  [{}] {} ({})", f.rule, f.what, f.by);
        }
        if of.len() > 12 {
            let _ = writeln!(out, "    and {} more", of.len() - 12);
        }
    }
    if let Some(n) = &r.soundcheck_note {
        let _ = writeln!(out, "  {who}: {n}. Stage's own scan ran alone.");
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::bank::detect::tests::tmp;

    pub struct FakeSoundcheck(pub Result<Vec<Finding>, String>);
    impl Soundcheck for FakeSoundcheck {
        fn run(&self, _: &Path) -> Result<Vec<Finding>, String> {
            self.0.clone()
        }
    }

    pub fn node_dir(files: &[(&str, &str)]) -> PathBuf {
        let d = tmp("node");
        for (n, b) in files {
            let p = d.join(n);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b).unwrap();
        }
        d
    }

    #[test]
    fn partner_nodes_are_found_by_name() {
        assert!(
            is_api_class("KlingTextToVideoNode")
                && is_api_class("OpenAIGPTImage1")
                && is_api_class("SomethingApiNode")
        );
        assert!(
            !is_api_class("KSampler")
                && !is_api_class("LTXVConditioning")
                && !is_api_class("UNETLoader")
        );
        let g = parse(r#"{"1":{"class_type":"KSampler","inputs":{}},"2":{"class_type":"VeoVideoGenerationNode","inputs":{}}}"#).unwrap();
        assert_eq!(api_classes_in(&g), ["VeoVideoGenerationNode"]);
    }

    #[test]
    fn a_node_that_sends_a_key_to_the_internet_is_a_cloud_api() {
        let d = node_dir(&[(
            "nodes.py",
            "import requests\nAPI_KEY = os.environ['X']\ndef go():\n    return requests.post('https://api.example.com/v1/run', headers={'Authorization': 'Bearer ' + API_KEY})\n",
        )]);
        let f = scan_dir(&d);
        assert!(
            f.iter()
                .any(|x| x.rule == CLOUD_RULE && x.sev == Sev::High && x.line == 4),
            "{f:?}"
        );
        let r = Review {
            findings: f,
            soundcheck_note: None,
        };
        assert!(r.cloud() && r.blocks());
    }

    #[test]
    fn local_calls_and_model_downloads_are_not_cloud_apis() {
        let local = node_dir(&[(
            "a.py",
            "import requests\ndef f():\n    return requests.get('http://127.0.0.1:8188/queue')\n",
        )]);
        assert!(scan_dir(&local).is_empty(), "{:?}", scan_dir(&local));
        let dl = node_dir(&[("b.py", "from huggingface_hub import hf_hub_download\ndef f():\n    return hf_hub_download('a/b', 'c.safetensors')\n")]);
        let f = scan_dir(&dl);
        assert!(
            f.len() == 1 && f[0].sev == Sev::Low && f[0].rule == "net-call",
            "{f:?}"
        );
        let web = node_dir(&[(
            "c.py",
            "import requests\nrequests.get('https://example.com/x')\n",
        )]);
        let f = scan_dir(&web);
        assert!(
            f.iter()
                .any(|x| x.sev == Sev::Medium && x.rule == "net-call")
                && !f.iter().any(|x| x.rule == CLOUD_RULE)
        );
    }

    #[test]
    fn eval_calls_are_found_and_model_eval_is_not() {
        let d = node_dir(&[(
            "x.py",
            "model.eval()\nmy_eval(1)\n# eval(bad)\nvalue = eval(text)\nos.system('ls')\nsubprocess.run(['ls'])\nos.system('pip install x')\n",
        )]);
        let f = scan_dir(&d);
        let lines: Vec<(u32, &str)> = f.iter().map(|x| (x.line, x.rule.as_str())).collect();
        assert!(
            lines.contains(&(4, "dynamic-code"))
                && lines.contains(&(5, "shell-exec"))
                && lines.contains(&(6, "subprocess")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|(l, _)| *l <= 3), "{lines:?}");
        assert!(lines.contains(&(7, "pip-install")));
    }

    #[test]
    fn the_review_joins_both_and_a_high_finding_blocks() {
        let d = node_dir(&[("ok.py", "def f():\n    return 1\n")]);
        let sc = FakeSoundcheck(Ok(vec![Finding {
            sev: Sev::High,
            rule: "dangerous-call".into(),
            file: "ok.py".into(),
            line: 2,
            what: "bad".into(),
            by: "soundcheck",
        }]));
        let r = review(&d, &sc);
        assert!(r.blocks() && !r.cloud() && r.soundcheck_note.is_none());
        let mut out = Vec::new();
        show(&r, "ComfyUI-Thing", &mut out);
        let t = String::from_utf8(out).unwrap();
        assert!(t.contains("1 HIGH") && t.contains("ok.py:2"), "{t}");
        // no agent: the note says so, and the own scan still ran
        let r = review(
            &d,
            &FakeSoundcheck(Err(
                "the Tempo agent is not installed here, so Soundcheck did not run".into(),
            )),
        );
        assert!(!r.blocks() && r.soundcheck_note.is_some());
    }

    #[test]
    fn soundchecks_json_is_read() {
        let f = read_report(r#"{"findings":[{"file":"a.py","line":3,"severity":"medium","problem":"p","rule":"r"},{"severity":"odd"}]}"#).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].sev, Sev::Medium);
        assert!(read_report("{}").is_err() && read_report("nope").is_err());
    }
}
