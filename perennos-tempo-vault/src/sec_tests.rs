//! Security regression tests (SEC-VAULT-STUDIO). Each one failed before its fix.
use crate::util::clean_rel;
use crate::vault::tests::{put, tmp};
use crate::vault::Vault;

#[test]
fn sec_windows_reserved_names_are_refused() {
    for bad in [
        "CON.md",
        "con",
        "NUL.txt",
        "a/Aux.md",
        "PRN",
        "COM1.md",
        "com9",
        "LPT1.md",
        "lpt9.x.md",
        "a/CONIN$.md",
        "CONOUT$",
    ] {
        assert_eq!(clean_rel(bad), None, "{bad}");
    }
    for ok in [
        "console.md",
        "Concerto.md",
        "com10.md",
        "a/nullable.md",
        "lpt0x.md",
    ] {
        assert!(clean_rel(ok).is_some(), "{ok}");
    }
}

#[test]
fn sec_history_ids_are_stamps_only() {
    let d = tmp("histid");
    put(&d, "a.md", "one");
    let mut v = Vault::open(&d).unwrap();
    v.write("a.md", "two", None, "test").unwrap();
    let id = v.history("a.md").remove(0);
    assert_eq!(v.history_text("a.md", &id).as_deref(), Some("one"));
    for bad in [
        "CON.md", "NUL.md", "x:y.md", "a b.md", "-1.md", "..md", "AUX.md", "1/2.md",
    ] {
        assert!(!crate::write::snapshot_id_ok(bad), "{bad}");
        assert_eq!(v.history_text("a.md", bad), None, "{bad}");
    }
    assert!(crate::write::snapshot_id_ok("20261105T120000123Z-2.md"));
}

#[test]
fn sec_case_insensitive_collisions_are_refused() {
    let d = tmp("case");
    put(&d, "Notes/Plan.md", "x");
    put(&d, "Other.md", "y");
    let mut v = Vault::open(&d).unwrap();
    assert!(v.create("notes/plan.md", "z", "t").is_err());
    assert!(v.rename("Other.md", "NOTES/PLAN.md", "t").is_err());
    // a case-only rename of the same note is still allowed
    assert!(v.rename("Other.md", "other.md", "t").is_ok());
}

#[cfg(unix)]
mod links {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A vault with `link -> outside/` and `leak.md -> outside/secret.md`.
    fn setup() -> (std::path::PathBuf, std::path::PathBuf, Vault) {
        let d = tmp("symvault");
        let out = tmp("symout");
        put(&d, "Home.md", "# Home\n");
        put(&out, "secret.md", "TOP SECRET");
        put(&out, "f.md", "outside file");
        symlink(&out, d.join("link")).unwrap();
        symlink(out.join("secret.md"), d.join("leak.md")).unwrap();
        let v = Vault::open(&d).unwrap();
        (d, out, v)
    }

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut n: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        n.sort();
        n
    }

    #[test]
    fn sec_writes_never_follow_a_symlinked_folder_out() {
        let (_d, out, mut v) = setup();
        let before = names(&out);
        assert!(v.create("link/new.md", "pwn", "t").is_err());
        assert!(v.write("link/new.md", "pwn", None, "t").is_err());
        assert!(v.write("link/f.md", "pwn", None, "t").is_err());
        assert!(v.append("link/f.md", "pwn", None, "t").is_err());
        assert!(v.add_file("link", "x.bin", b"pwn", "t").is_err());
        assert!(v.rename("Home.md", "link/home.md", "t").is_err());
        assert!(v.trash("link/f.md", "t").is_err());
        assert_eq!(names(&out), before, "nothing was created or moved outside");
        assert_eq!(
            std::fs::read_to_string(out.join("f.md")).unwrap(),
            "outside file"
        );
    }

    #[test]
    fn sec_a_symlinked_note_is_neither_read_nor_overwritten() {
        let (_d, out, mut v) = setup();
        // the conflict answer used to hand back the outside file's text
        let e = v.write("leak.md", "x", Some(0.0), "t").unwrap_err();
        assert!(!format!("{e:?}").contains("TOP SECRET"), "{e:?}");
        assert!(v.append("leak.md", "pwn", None, "t").is_err());
        assert!(v
            .set_prop("leak.md", "k", &crate::parse::Prop::Text("v".into()), "t")
            .is_err());
        assert_eq!(
            std::fs::read_to_string(out.join("secret.md")).unwrap(),
            "TOP SECRET"
        );
        assert!(v.read("leak.md").is_none());
    }

    #[test]
    fn sec_a_planted_temp_file_symlink_is_not_written_through() {
        let (d, out, mut v) = setup();
        symlink(out.join("f.md"), d.join("Home.md.tempo-tmp")).unwrap();
        let _ = v.write("Home.md", "changed", None, "t");
        assert_eq!(
            std::fs::read_to_string(out.join("f.md")).unwrap(),
            "outside file"
        );
    }

    #[test]
    fn sec_served_file_swapped_for_a_symlink_after_indexing_is_refused() {
        use crate::server::{start, ServeOpts};
        use std::io::{Read, Write};
        let d = tmp("symsrv");
        let out = tmp("symsrvout");
        put(&d, "img/a.png", "PNG");
        put(&out, "secret.txt", "TOP SECRET");
        let v = Vault::open(&d).unwrap();
        let r = start(
            v,
            ServeOpts {
                port: 0,
                watch: std::time::Duration::ZERO,
                ..ServeOpts::default()
            },
        )
        .unwrap();
        std::fs::remove_file(d.join("img/a.png")).unwrap();
        symlink(out.join("secret.txt"), d.join("img/a.png")).unwrap();
        let mut s = std::net::TcpStream::connect(r.addr).unwrap();
        write!(
            s,
            "GET /vault/file?path=img/a.png HTTP/1.1\r\nHost: {}\r\n\r\n",
            r.addr
        )
        .unwrap();
        let mut got = String::new();
        let _ = s.read_to_string(&mut got);
        assert!(!got.contains("TOP SECRET"), "{got}");
        assert!(got.starts_with("HTTP/1.1 404"), "{got}");
        r.stop();
    }
}

mod schedule {
    use crate::learn::{crontab_current, win_quote};

    #[cfg(unix)]
    #[test]
    fn sec_cron_line_has_no_shell_injection() {
        use crate::learn::{schedule_plan, CRON_MARK};
        use std::path::Path;
        let d = crate::vault::tests::tmp("cron");
        let marker = d.join("PWNED");
        let nasty = d.join(format!(
            "v$(touch {})`touch {}`\"; touch {}; \"it's 100% $HOME",
            marker.display(),
            marker.display(),
            marker.display()
        ));
        std::fs::create_dir_all(&nasty).unwrap();
        // the program is `echo` so the shell prints what it was given
        let (line, _) = schedule_plan(Path::new("/bin/echo"), &nasty, "hourly").unwrap();
        // what cron hands to sh: drop the schedule, undo cron's own `\%` rule, drop the log redirect
        let cmd = line
            .strip_prefix("17 * * * * ")
            .unwrap()
            .split(" >> ")
            .next()
            .unwrap()
            .replace("\\%", "%");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .unwrap();
        assert!(!marker.exists(), "the vault name ran as shell code: {line}");
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(said.contains(nasty.to_str().unwrap()), "{said} / {line}");
        assert!(line.ends_with(CRON_MARK));
        // an unescaped % ends a cron command line
        assert!(!line.replace("\\%", "").contains('%'), "{line}");
    }

    #[cfg(unix)]
    #[test]
    fn sec_cron_refuses_newlines_in_paths() {
        use crate::learn::schedule_plan;
        use std::path::Path;
        let bad = Path::new("/home/a/v\n* * * * * touch /tmp/pwn #");
        assert!(schedule_plan(Path::new("/opt/tempo-vault"), bad, "hourly").is_err());
        assert!(schedule_plan(bad, Path::new("/home/a/v"), "daily").is_err());
    }

    #[test]
    fn sec_windows_task_paths_are_quoted_for_argv() {
        assert_eq!(win_quote(r"C:\Vault\Notes").unwrap(), r#""C:\Vault\Notes""#);
        // a trailing backslash would escape the closing quote
        assert_eq!(
            win_quote(r"C:\Vault\Notes\").unwrap(),
            r#""C:\Vault\Notes""#
        );
        assert_eq!(win_quote(r"C:\").unwrap(), r#""C:\\""#);
        // canonicalize() on Windows gives \\?\ paths; the task should get the plain one
        assert_eq!(win_quote(r"\\?\C:\Vault").unwrap(), r#""C:\Vault""#);
        assert_eq!(win_quote(r"\\?\UNC\nas\notes").unwrap(), r#""\\nas\notes""#);
        assert!(win_quote("C:\\a\"b").is_err());
        assert!(win_quote("C:\\a\nb").is_err());
    }

    #[test]
    fn sec_a_failing_crontab_list_never_becomes_an_empty_crontab() {
        assert_eq!(
            crontab_current(true, "0 1 * * * x\n", "").unwrap(),
            "0 1 * * * x\n"
        );
        assert_eq!(
            crontab_current(false, "", "no crontab for riley").unwrap(),
            ""
        );
        assert!(
            crontab_current(false, "", "crontab: error while loading shared libraries").is_err()
        );
        assert!(crontab_current(false, "", "").is_err());
    }
}

mod publishing {
    use crate::publish::publish;
    use crate::vault::tests::{put, tmp};
    use crate::vault::Vault;

    #[test]
    fn sec_published_site_never_carries_active_attachments() {
        let d = tmp("pubact");
        put(
            &d,
            "A.md",
            "---\npublish: true\n---\n# A\n[page](evil.html) ![[evil.svg]] ![[ok.svg]] ![[ok.png]] [x](x.xhtml)\n",
        );
        put(&d, "evil.html", "<script>alert(1)</script>");
        put(&d, "x.xhtml", "<script>alert(1)</script>");
        put(
            &d,
            "evil.svg",
            "<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>",
        );
        put(
            &d,
            "ok.svg",
            "<svg xmlns='http://www.w3.org/2000/svg'><circle r='1'/></svg>",
        );
        put(&d, "ok.png", "PNG");
        let v = Vault::open(&d).unwrap();
        let out = tmp("pubactout");
        publish(&v, &out, false).unwrap();
        assert!(!out.join("evil.html").exists() && !out.join("x.xhtml").exists());
        assert!(
            !out.join("evil.svg").exists(),
            "a scripted SVG opens in the site's origin"
        );
        assert!(out.join("ok.svg").is_file() && out.join("ok.png").is_file());
    }

    #[test]
    fn sec_refused_publish_into_the_vault_leaves_no_folder_behind() {
        let d = tmp("pubin");
        put(&d, "A.md", "# A\n");
        let v = Vault::open(&d).unwrap();
        assert!(publish(&v, &d.join("site"), true).is_err());
        assert!(!d.join("site").exists());
    }
}

mod hostile_markdown {
    use crate::render::{markdown_html, Opts};
    use crate::vault::tests::{put, tmp};
    use crate::vault::Vault;

    /// Every `<tag ...>` in the output, with the attributes that came with it.
    fn tags(h: &str) -> Vec<String> {
        let mut out = vec![];
        let mut rest = h;
        while let Some(i) = rest.find('<') {
            let after = &rest[i..];
            // attribute values are escaped, so the first `>` ends the tag
            let e = after.find('>').unwrap_or(after.len() - 1);
            out.push(after[..=e].to_string());
            rest = &after[e + 1..];
        }
        out
    }

    #[test]
    fn sec_hostile_notes_render_inert() {
        let d = tmp("xss");
        // Windows cannot hold a `"` in a file name, so there those notes do not exist and the
        // links to them stay unresolved; every payload below must still render inert.
        let can_hold =
            |rel: &str| !cfg!(windows) || !rel.contains(['"', '<', '>', ':', '|', '?', '*']);
        for (rel, body) in [
            ("evil name\" onmouseover=\"alert(1).md", "x"),
            ("a.md", "x"),
            ("img/p\" onerror=\"alert(1).png", "PNG"),
            ("x.html", "<script>alert(1)</script>"),
        ] {
            if can_hold(rel) {
                put(&d, rel, body);
            }
        }
        let v = Vault::open(&d).unwrap();
        let payloads = [
            "<script>alert(1)</script>",
            "<img src=x onerror=alert(1)>",
            "<svg onload=alert(1)>",
            "[x](javascript:alert(1)) [x](JaVa\tScRiPt:alert(1)) [x](  javascript:alert(1))",
            "[x](vbscript:x) [x](data:text/html,<script>alert(1)</script>) [x](file:///etc/passwd)",
            "![x](javascript:alert(1)) ![x](data:image/svg+xml,<svg onload=alert(1)>)",
            "![x](https://e.com/a.png\" onerror=\"alert(1))",
            "[x](https://e.com/\" onmouseover=\"alert(1))",
            "<https://e.com/\"onmouseover=\"alert(1)>",
            "https://e.com/\"onmouseover=\"alert(1)",
            "[[evil name\" onmouseover=\"alert(1)]]",
            "[[a|\" onmouseover=\"alert(1)]] [[a#\"onmouseover=\"x]] [[a#^\"onmouseover=\"x]]",
            "![[img/p\" onerror=\"alert(1).png]] ![[img/p\" onerror=\"alert(1).png|\"onerror=\"x]]",
            "![[x.html]] [[x.html]] ![[evil.svg|100\" onload=\"x]]",
            "#tag\"onmouseover=\"x #a/b\"c",
            "> [!note\" onmouseover=\"alert(1)] t\" onmouseover=\"x\n> body",
            "```js\" onmouseover=\"alert(1)\ncode\n```",
            "```query\n\" onmouseover=\"alert(1)\n```",
            "| a\" onmouseover=\"x | b |\n|:-\"onmouseover=\"x|-:|\n| <b>x</b> | y |",
            "Text[^1\" onmouseover=\"x]\n\n[^1\" onmouseover=\"x]: note\" onmouseover=\"y",
            "# h\" onmouseover=\"alert(1) ^blk\" onmouseover=\"x\npara ^b\"onmouseover=\"x",
            "- [ ] task\" onmouseover=\"x ^id\"onmouseover=\"y",
            "$$\n</div><script>alert(1)</script>\n$$ $</span><script>x</script>$",
            "<a href=\"javascript:alert(1)\">x</a> <iframe src=\"//evil\"></iframe> <style>*{}</style>",
            "---\nk: <script>alert(1)</script>\n\"a\" onmouseover=\"x: [[a]]\ntags: [\"><script>x</script>]\n---\nbody",
        ];
        for p in payloads {
            for from in ["a.md", "evil name\" onmouseover=\"alert(1).md"] {
                let h = markdown_html(Some(&v), from, p, &Opts::app());
                for t in tags(&h) {
                    let low = t.to_lowercase();
                    let name: String = low[1..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '/')
                        .collect();
                    assert!(
                        !matches!(
                            name.as_str(),
                            "script"
                                | "iframe"
                                | "svg"
                                | "style"
                                | "object"
                                | "embed"
                                | "form"
                                | "link"
                                | "meta"
                                | "base"
                                | "input/"
                        ),
                        "{p:?} -> {t}"
                    );
                    // a quoted attribute value never ends early
                    let mut quotes = t.matches('"').count();
                    quotes -= t.matches("&quot;").count().min(quotes);
                    assert_eq!(quotes % 2, 0, "{p:?} -> {t}");
                    // no attribute named on*: `on...=` right after a space, outside quotes
                    let mut in_q = false;
                    let b: Vec<char> = low.chars().collect();
                    for k in 0..b.len() {
                        if b[k] == '"' {
                            in_q = !in_q;
                        }
                        if !in_q
                            && b[k] == ' '
                            && low[low.char_indices().nth(k).unwrap().0..].starts_with(" on")
                        {
                            panic!("event handler attribute: {p:?} -> {t}");
                        }
                    }
                    for attr in ["href=\"", "src=\""] {
                        for (i, _) in low.match_indices(attr) {
                            let val = &low[i + attr.len()..];
                            let val = &val[..val.find('"').unwrap_or(val.len())];
                            let ok = val.starts_with("http://")
                                || val.starts_with("https://")
                                || val.starts_with("mailto:")
                                || val.starts_with('#')
                                || val.starts_with("/vault/file?");
                            assert!(ok, "{p:?} -> {t}");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn sec_fuzz_never_panics() {
    use crate::render::{markdown_html, Opts};
    let pieces = [
        "[[",
        "]]",
        "![[",
        "|",
        "#",
        "^",
        "---\n",
        "%%",
        "```",
        "`",
        "**",
        "~~",
        "==",
        "$$",
        "$",
        "\n",
        "\r\n",
        " ",
        "- [ ] ",
        "> [!x]- ",
        "[",
        "](",
        ")",
        "<",
        ">",
        "&",
        "é",
        "日本",
        "😀",
        "\u{feff}",
        "\u{0}",
        "http://",
        "://",
        "a:b@",
        "Bearer ",
        "password=",
        "sk-ant-",
        // token-shaped pieces are split in the source, so no leak scanner ever matches this file
        concat!("-----BEGIN ", "PRIVATE KEY-----\n"),
        "-----END PRIVATE KEY-----",
        "eyJ",
        ".",
        "token: ",
        "api_key=\"",
        "ghp_",
        "AKIA",
        "| a | b |\n|-|-|\n",
        "[^1]",
        "[^1]: ",
        "📅 2026-01-01",
        "![[",
        "..",
        "/",
        "\\",
    ];
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut rnd = |n: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n as u64) as usize
    };
    let d = crate::vault::tests::tmp("fuzz");
    crate::vault::tests::put(&d, "a.md", "# a\n");
    let v = crate::vault::Vault::open(&d).unwrap();
    for _ in 0..5000 {
        let s: String = (0..rnd(40) + 1)
            .map(|_| pieces[rnd(pieces.len())])
            .collect();
        let _ = crate::util::redact(&s);
        let _ = crate::parse::parse(&s);
        let _ = markdown_html(Some(&v), "a.md", &s, &Opts::app());
        let _ = crate::util::clean_rel(&s);
        let _ = v.search(&s, 5);
    }
}

#[test]
fn sec_non_ascii_names_never_panic() {
    use crate::util::{is_md, strip_md};
    // the last three bytes of these are not on a character boundary
    for s in ["éé", "naïve", "Crème", "日本語é", "a😀", "😀"] {
        assert!(!is_md(s), "{s}");
        assert_eq!(strip_md(s), s);
    }
    assert_eq!(strip_md("Crème.MD"), "Crème");
    let d = crate::vault::tests::tmp("utf");
    crate::vault::tests::put(
        &d,
        "a.md",
        "[[naïve]] [[Crème]] [[éé]] ![[Zoé]] [x](naïve) [[naïve#é]]\n",
    );
    crate::vault::tests::put(&d, "Crème", "x");
    let v = crate::vault::Vault::open(&d).unwrap();
    let h = crate::render::note_html(&v, "a.md", &crate::render::Opts::app()).unwrap();
    assert!(h.contains("naïve"));
}

#[test]
fn sec_model_servers_are_loopback_private_or_tailnet_only() {
    use crate::learn::is_local_addr;
    // the cloud metadata address is link-local, not private
    for ip in [
        "169.254.169.254",
        "0.0.0.0",
        "8.8.8.8",
        "100.63.255.255",
        "100.128.0.0",
        "::ffff:8.8.8.8",
        "2001:db8::1",
    ] {
        assert!(!is_local_addr(ip.parse().unwrap()), "{ip}");
    }
    // the ends of the tailnet range, put together at run time so the public leak scan never matches
    let (low, high) = (format!("100.{}.0.1", 64), format!("100.{}.255.255", 127));
    for ip in [
        "127.0.0.1",
        "10.0.0.1",
        "172.31.255.255",
        "192.168.0.1",
        low.as_str(),
        high.as_str(),
        "::1",
        "fd7a:115c:a1e0::1",
    ] {
        assert!(is_local_addr(ip.parse().unwrap()), "{ip}");
    }
}

#[test]
fn sec_model_reply_is_capped() {
    use std::io::Write;
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let t = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = std::io::Read::read(&mut s, &mut buf);
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n");
        let chunk = vec![b' '; 1 << 20];
        for _ in 0..40 {
            if s.write_all(&chunk).is_err() {
                break;
            }
        }
    });
    let r = crate::learn::post_json(
        &format!("http://127.0.0.1:{port}"),
        "/api/chat",
        &serde_json::json!({}),
        std::time::Duration::from_secs(20),
        false,
    );
    t.join().unwrap();
    // 40 MB of blanks is cut at the cap and is not JSON either way; the point is it returns
    assert!(r.is_err());
}

#[test]
fn sec_server_refuses_rebinding_csrf_and_traversal() {
    use crate::server::{start, ServeOpts};
    use std::io::{Read, Write};
    let d = tmp("srvsec");
    put(&d, "Home.md", "# Home\n");
    put(&d, "img/a.svg", "<svg onload=alert(1)/>");
    put(&d, "page.html", "<script>alert(1)</script>");
    let out = tmp("srvsecout");
    put(&out, "secret.txt", "TOP SECRET");
    let v = Vault::open(&d).unwrap();
    let r = start(
        v,
        ServeOpts {
            port: 0,
            watch: std::time::Duration::ZERO,
            ..ServeOpts::default()
        },
    )
    .unwrap();
    let a = r.addr;
    let raw = |req: String| -> (u16, String) {
        let mut s = std::net::TcpStream::connect(a).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut got = String::new();
        let _ = s.read_to_string(&mut got);
        (
            got.split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0),
            got,
        )
    };
    let get =
        |path: &str, extra: &str| raw(format!("GET {path} HTTP/1.1\r\nHost: {a}\r\n{extra}\r\n"));
    // DNS rebinding: the attacker's name in Host, in any shape
    for host in [
        "evil.example",
        "127.0.0.1.evil.example",
        "127.0.0.1",
        "localhost:1",
        &format!("{a}.evil.example"),
        "",
    ] {
        let (c, _) = raw(format!("GET /vault/tree HTTP/1.1\r\nHost: {host}\r\n\r\n"));
        assert_eq!(c, 403, "Host {host:?}");
    }
    assert_eq!(
        raw("GET /vault/tree HTTP/1.1\r\n\r\n".into()).0,
        400,
        "no Host"
    );
    // CSRF: other origins, null origin, same host other port, cross-site fetch metadata
    for o in [
        "http://evil.example",
        "null",
        "http://127.0.0.1:1",
        "https://127.0.0.1",
        &format!("http://{a}.evil.example"),
    ] {
        assert_eq!(
            get("/vault/tree", &format!("Origin: {o}\r\n")).0,
            403,
            "{o}"
        );
        let body = r#"{"path":"X.md","text":"x"}"#;
        let (c, _) = raw(format!("POST /vault/create HTTP/1.1\r\nHost: {a}\r\nOrigin: {o}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()));
        assert_eq!(c, 403, "{o}");
    }
    assert!(!d.join("X.md").exists());
    assert_eq!(get("/vault/tree", "Sec-Fetch-Site: cross-site\r\n").0, 403);
    assert_eq!(
        raw(format!(
            "OPTIONS /vault/create HTTP/1.1\r\nHost: {a}\r\n\r\n"
        ))
        .0,
        405
    );
    // a form post (the only cross-site POST a page can make without a preflight)
    let (c, _) = raw(format!("POST /vault/create HTTP/1.1\r\nHost: {a}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 3\r\n\r\na=b"));
    assert_eq!(c, 415);
    // traversal through /vault/file in every spelling
    for p in [
        "../srvsecout/secret.txt",
        "..%2fsecret.txt",
        "%2e%2e/secret.txt",
        "/etc/passwd",
        "img/../../x",
        "C:/Windows/win.ini",
        "img%00.svg",
        "..\\secret.txt",
        ".tempo/changes.jsonl",
        ".obsidian/app.json",
    ] {
        let (c, body) = get(&format!("/vault/file?path={p}"), "");
        assert!(c == 400 || c == 404, "{p}: {c}");
        assert!(!body.contains("TOP SECRET") && !body.contains("root:"));
    }
    // an uploaded or committed .svg / .html can never run script in the app's origin
    let (c, head) = get("/vault/file?path=img/a.svg", "");
    assert_eq!(c, 200);
    let low = head.to_lowercase();
    assert!(
        low.contains("content-security-policy: default-src 'none'") && low.contains("sandbox"),
        "{head}"
    );
    assert!(low.contains("x-content-type-options: nosniff"));
    let (c, head) = get("/vault/file?path=page.html", "");
    let low = head.to_lowercase();
    assert!(
        c == 200 && low.contains("content-type: text/plain") && low.contains("sandbox"),
        "{head}"
    );
    // ranges: out of bounds, reversed and multi-range are refused, not served
    for rg in [
        "bytes=999-1000",
        "bytes=5-2",
        "bytes=0-1,3-4",
        "bytes=-0",
        "bytes=a-b",
        "bytes=99999999999999999999-",
    ] {
        let (c, _) = get("/vault/file?path=img/a.svg", &format!("Range: {rg}\r\n"));
        assert_eq!(c, 416, "{rg}");
    }
    r.stop();
}

#[test]
fn sec_mcp_paths_follow_the_same_rules_and_read_only_holds() {
    use crate::mcp::call;
    use serde_json::json;
    let d = tmp("mcpsec");
    put(&d, "Home.md", "# Home\n");
    let mut v = Vault::open(&d).unwrap();
    for tool in [
        "vault_create",
        "vault_append",
        "vault_write",
        "vault_rename",
        "vault_trash",
        "vault_set_property",
        "vault_set_task",
        "vault_daily_append",
    ] {
        let e = call(&mut v, tool, &json!({"path": "X.md", "text": "x", "mtime": 0, "from": "Home.md", "to": "Y.md", "key": "k", "value": "v", "line": 1}), false).unwrap_err();
        assert!(e.contains("read-only"), "{tool}: {e}");
    }
    assert!(!d.join("X.md").exists() && d.join("Home.md").exists());
    let bad = [
        "../x.md",
        "/etc/x.md",
        "C:/x.md",
        "\\\\server\\share\\x.md",
        ".tempo/changes.jsonl",
        ".obsidian/app.json",
        "a/../../x.md",
        "CON.md",
        "NUL",
        "a:stream.md",
        "x.md.",
        "a/.trash/b.md",
    ];
    for p in bad {
        for (tool, args) in [
            ("vault_create", json!({"path": p, "text": "x"})),
            ("vault_append", json!({"path": p, "text": "x"})),
            ("vault_write", json!({"path": p, "text": "x", "mtime": 0})),
            ("vault_trash", json!({"path": p})),
            ("vault_rename", json!({"from": "Home.md", "to": p})),
            ("vault_read", json!({"path": p})),
        ] {
            assert!(call(&mut v, tool, &args, true).is_err(), "{tool} {p}");
        }
    }
    assert!(
        !d.join(".tempo").join("changes.jsonl").exists()
            || std::fs::read_to_string(d.join(".tempo/changes.jsonl"))
                .unwrap()
                .is_empty()
    );
    assert!(d.join("Home.md").exists());
}

#[cfg(unix)]
#[test]
fn sec_a_linked_dot_tempo_folder_does_not_take_history_or_logs_out() {
    let d = tmp("dottempo");
    let out = tmp("dottempoout");
    put(&d, "Home.md", "one");
    std::os::unix::fs::symlink(&out, d.join(".tempo")).unwrap();
    let mut v = Vault::open(&d).unwrap();
    let _ = v.write("Home.md", "two", None, "t");
    let _ = v.create("New.md", "x", "t");
    let _ = v.add_file("", "a.bin", b"x", "t");
    assert_eq!(
        std::fs::read_dir(&out).unwrap().count(),
        0,
        "nothing written through .tempo"
    );
}

#[test]
fn sec_redaction_hides_common_secret_shapes() {
    use crate::util::redact;
    let secrets = [
        (
            "export OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwx",
            "abcdefghijklmnopqrstuvwx",
        ),
        (
            "Authorization: Bearer abcdef123456.ghijkl789",
            "abcdef123456",
        ),
        ("{\"api_key\": \"s3cr3t-value-123\"}", "s3cr3t-value-123"),
        ("password: 'hunter2hunter2'", "hunter2hunter2"),
        (
            concat!("token=gh", "p_abcdefghijklmnopqrstuvwxyz0123456789"),
            concat!("gh", "p_abcdefghijklmnopqrstuvwxyz0123456789"),
        ),
        ("postgres://user:p4ssw0rd@db.local/x", "p4ssw0rd"),
        (
            concat!("AWS key AKIA", "IOSFODNN7EXAMPLE here"),
            concat!("AKIA", "IOSFODNN7EXAMPLE"),
        ),
        (
            concat!(
                "-----BEGIN ",
                "PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----"
            ),
            "MIIEvQIBADANBg",
        ),
        ("slack xoxb-123456789012-abcdefghijkl", "abcdefghijkl"),
        (
            concat!("h", "f_abcdefghijklmnopqrstuvwxyz123456"),
            "abcdefghijklmnopqrstuvwxyz123456",
        ),
        (
            concat!("sk-", "ant-api03-abcdefghijklmnopqrstuvwxyz"),
            "abcdefghijklmnopqrstuvwxyz",
        ),
        (
            "SECRET = \"value_with_quotes_1234\"",
            "value_with_quotes_1234",
        ),
        ("passwd:   spaced-out-secret", "spaced-out-secret"),
        (
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
        ),
    ];
    for (input, secret) in secrets {
        let out = redact(input);
        assert!(!out.contains(secret), "{input:?} -> {out:?}");
    }
}
