//! Gated models on Hugging Face. Some models (FLUX, LTX) are behind a licence that the owner
//! accepts on the model's page; Hugging Face then sends the file only to a request that carries
//! the owner's token. Stage reads that token if there is one, shows the licence first, and:
//!
//! - sends it to huggingface.co only, over https, as `Authorization: Bearer`, on exactly the
//!   requests that go to huggingface.co. Hugging Face hands the file itself over from a content
//!   server on another name (a redirect); that hop gets no token. Every hop is decided here,
//!   one at a time, never by curl following a redirect with a header on;
//! - never writes it down: not in the output, not in a log, not in a file, not in a command
//!   line (curl is given it on stdin). A copy in an error message is blanked.
//!
//! The token is the owner's: `HF_TOKEN`, else the file `huggingface-cli login` writes
//! (`$HF_HOME/token`, or `~/.cache/huggingface/token`).

use crate::setup::registry::{Fetch, Probe};
use std::path::{Path, PathBuf};

/// An access token. It has no `Display`, and its `Debug` says nothing about what it holds.
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(hidden)")
    }
}

impl Token {
    /// What a token looks like: one word of printable characters, no quote or backslash.
    pub fn new(raw: &str) -> Option<Token> {
        let t = raw.trim();
        let ok = (8..=200).contains(&t.len())
            && t.bytes()
                .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
        ok.then(|| Token(t.to_string()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    /// `text` with any copy of the token blanked.
    pub fn scrub(&self, text: &str) -> String {
        text.replace(self.as_str(), "[token]")
    }
}

/// The owner's token, if they have one. `var` reads an environment variable; `home` is the
/// owner's home folder (for `~/.cache/huggingface/token`).
pub fn read_token(var: &dyn Fn(&str) -> Option<String>, home: Option<&Path>) -> Option<Token> {
    if let Some(t) = var("HF_TOKEN").and_then(|v| Token::new(&v)) {
        return Some(t);
    }
    let dir: PathBuf = match var("HF_HOME").filter(|h| !h.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => home?.join(".cache").join("huggingface"),
    };
    let text = std::fs::read_to_string(dir.join("token")).ok()?;
    Token::new(text.lines().next().unwrap_or(""))
}

/// This computer's own environment and home.
pub fn token_from_env() -> Option<Token> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    read_token(
        &|k| std::env::var(k).ok().filter(|v| !v.is_empty()),
        home.as_deref(),
    )
}

/// Whether `url` is on huggingface.co itself: https, that exact name, the usual port, no login
/// in the address. `huggingface.co.evil.example`, `evil.example/huggingface.co` and
/// `huggingface.co@evil.example` are not.
pub fn is_hf(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.eq_ignore_ascii_case("huggingface.co")
}

/// The token to send with a request to `url`: only for huggingface.co.
fn bearer_for<'a>(url: &str, t: Option<&'a Token>) -> Option<&'a str> {
    t.filter(|_| is_hf(url)).map(Token::as_str)
}

/// The model's page, from a file address (`.../owner/repo/resolve/rev/path`).
pub fn page_of(url: &str) -> String {
    url.split("/resolve/").next().unwrap_or(url).to_string()
}

/// What the licence says, in a few plain words, for the families the catalogue has. Upstream's
/// own words, as read on the model's page; the page is the licence.
pub fn summary(licence: &str) -> String {
    let l = licence.to_ascii_lowercase();
    let what = if l.contains("flux") {
        "free for non-commercial use; commercial use is allowed under a revenue cap, per upstream"
    } else if l.contains("ltx") {
        "free to use, including commercially, under about $10M a year in revenue; larger companies need a paid licence, per upstream"
    } else {
        "read it on the model's page before you accept it"
    };
    format!("{licence}: {what}.")
}

/// What Stage says before it fetches a gated model.
pub fn gated_notice(name: &str, licence: &str, url: &str, have_token: bool) -> String {
    let mut s = format!(
        "{name} is gated. Its licence: {}\n  Hugging Face sends it only after you accept that licence on the model's page: {}",
        summary(licence),
        page_of(url)
    );
    if have_token {
        s.push_str("\n  Stage found your Hugging Face token. It is sent to huggingface.co only, as a bearer header over https, and is never printed, logged or kept.");
    } else {
        s.push_str("\n  To let Stage fetch it: accept the licence there, make a token at https://huggingface.co/settings/tokens, and put it in HF_TOKEN (or run `huggingface-cli login`). Without one, you put the file in place yourself.");
    }
    s
}

/// What Hugging Face's "no" means for the owner.
fn denied(code: u16, page: &str, had_token: bool) -> String {
    if had_token {
        format!("Hugging Face turned your token away ({code}) for this model. Open {page}, accept its licence with the account the token belongs to, then run this again.")
    } else {
        format!("Hugging Face wants a token for this model ({code}). Accept its licence at {page}, set HF_TOKEN (or run `huggingface-cli login`), then run this again.")
    }
}

/// Fetch `url` into `part`, following redirects here, one hop at a time: the token goes on a
/// hop only when that hop is on huggingface.co, over https. A redirect that leaves https is not
/// followed.
pub fn fetch(
    f: &dyn Fetch,
    url: &str,
    token: Option<&Token>,
    part: &Path,
    max_secs: u64,
    progress: &mut dyn FnMut(u64),
) -> Result<(), String> {
    let scrub = |e: String| token.map_or(e.clone(), |t| t.scrub(&e));
    let page = page_of(url);
    let mut at = url.to_string();
    for _ in 0..6 {
        if !at.starts_with("https://") {
            return Err("an address that is not https was not followed".into());
        }
        let bearer = bearer_for(&at, token);
        match f.probe(&at, bearer).map_err(scrub)? {
            Probe::Moved(next) => at = next,
            Probe::Here => {
                return f
                    .download_hop(&at, part, max_secs, progress, bearer)
                    .map_err(scrub)
            }
            Probe::Denied(c) => return Err(denied(c, &page, token.is_some())),
            Probe::NotFound => return Err(format!("Hugging Face has no file at {page} (404)")),
            Probe::Other(c) => return Err(format!("Hugging Face answered {c} for {page}")),
        }
    }
    Err("too many redirects".into())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A made-up token, built when the test runs so no source line holds one.
    pub fn fake_token() -> String {
        format!("{}_{}", "hf", "abcdefghij".repeat(3))
    }

    /// A fetcher with a map of redirects that records every request: address, and whether a
    /// token went with it.
    pub struct Hops {
        pub log: Mutex<Vec<(String, Option<String>)>>,
        pub moves: Vec<(String, String)>,
        pub deny: Option<u16>,
        pub body: Vec<u8>,
    }

    impl Fetch for Hops {
        fn get(&self, _: &str, _: Option<&str>, _: bool) -> crate::setup::registry::Fetched {
            crate::setup::registry::Fetched::Failed
        }
        fn probe(&self, url: &str, bearer: Option<&str>) -> Result<Probe, String> {
            self.log
                .lock()
                .unwrap()
                .push((url.to_string(), bearer.map(String::from)));
            if let Some(c) = self.deny.filter(|_| bearer.is_none()) {
                return Ok(Probe::Denied(c));
            }
            Ok(match self.moves.iter().find(|(from, _)| from == url) {
                Some((_, to)) => Probe::Moved(to.clone()),
                None => Probe::Here,
            })
        }
        fn download_hop(
            &self,
            url: &str,
            part: &Path,
            _: u64,
            _: &mut dyn FnMut(u64),
            bearer: Option<&str>,
        ) -> Result<(), String> {
            self.log
                .lock()
                .unwrap()
                .push((format!("GET {url}"), bearer.map(String::from)));
            std::fs::write(part, &self.body).map_err(|e| e.to_string())
        }
    }

    pub fn hops(moves: &[(&str, &str)]) -> Hops {
        Hops {
            log: Mutex::new(vec![]),
            moves: moves
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            deny: None,
            body: b"model".to_vec(),
        }
    }

    #[test]
    fn only_huggingface_co_over_https_counts() {
        for yes in [
            "https://huggingface.co/a/b/resolve/main/f.safetensors",
            "https://HuggingFace.co/a/b",
            "https://huggingface.co",
            "https://huggingface.co?x=1",
        ] {
            assert!(is_hf(yes), "{yes}");
        }
        for no in [
            "http://huggingface.co/a/b",
            "https://huggingface.co.evil.example/a",
            "https://evil.example/huggingface.co/a",
            "https://huggingface.co@evil.example/a",
            "https://user:pw@huggingface.co/a",
            "https://huggingface.co:8443/a",
            "https://cdn-lfs.huggingface.co/a",
            "https://hf.co/a",
            "https://xhuggingface.co/a",
            "//huggingface.co/a",
            "",
        ] {
            assert!(!is_hf(no), "{no}");
        }
    }

    #[test]
    fn the_token_comes_from_hf_token_then_the_standard_file_and_must_look_like_one() {
        let tok = fake_token();
        let home = crate::bank::detect::tests::tmp("hf-home");
        let dir = home.join(".cache").join("huggingface");
        std::fs::create_dir_all(&dir).unwrap();
        let none = |_: &str| None;
        assert!(
            read_token(&none, Some(&home)).is_none(),
            "no token anywhere"
        );
        std::fs::write(dir.join("token"), format!("{tok}\n")).unwrap();
        let from_file = read_token(&none, Some(&home)).unwrap();
        assert_eq!(from_file.as_str(), tok);
        // HF_TOKEN wins over the file
        let other = format!("{}_{}", "hf", "zyxwvutsrq".repeat(3));
        let env = |k: &str| (k == "HF_TOKEN").then(|| other.clone());
        assert_eq!(read_token(&env, Some(&home)).unwrap().as_str(), other);
        // HF_HOME moves the file
        let h2 = crate::bank::detect::tests::tmp("hf-home2");
        std::fs::write(h2.join("token"), format!("{other}\n")).unwrap();
        let hh = |k: &str| (k == "HF_HOME").then(|| h2.to_string_lossy().into_owned());
        assert_eq!(read_token(&hh, Some(&home)).unwrap().as_str(), other);
        // something that is not a token (spaces, a quote, too short) is not used
        for bad in ["", "short", "has a space in it 12345", "quote\"in-it-12345"] {
            assert!(Token::new(bad).is_none(), "{bad:?}");
        }
        // Debug says nothing, and a copy in a message is blanked
        assert_eq!(format!("{from_file:?}"), "Token(hidden)");
        assert_eq!(
            from_file.scrub(&format!("failed with {tok} in it")),
            "failed with [token] in it"
        );
    }

    #[test]
    fn the_token_goes_to_huggingface_co_and_not_to_the_server_it_hands_the_file_over_from() {
        let t = Token::new(&fake_token()).unwrap();
        let f = hops(&[(
            "https://huggingface.co/a/b/resolve/main/m.safetensors",
            "https://cdn-lfs.huggingface.co/repos/xx/m.safetensors?sig=1",
        )]);
        let part = crate::bank::detect::tests::tmp("hf-part").join("m.part");
        fetch(
            &f,
            "https://huggingface.co/a/b/resolve/main/m.safetensors",
            Some(&t),
            &part,
            60,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), b"model");
        let log = f.log.lock().unwrap().clone();
        assert_eq!(log.len(), 3, "{log:?}");
        // the first request, to huggingface.co, carried it
        assert_eq!(log[0].1.as_deref(), Some(fake_token().as_str()));
        // the content server saw none, on either of its requests
        assert!(
            log[1..]
                .iter()
                .all(|(u, b)| u.contains("cdn-lfs") && b.is_none()),
            "{log:?}"
        );
    }

    #[test]
    fn a_redirect_to_another_host_or_off_https_never_gets_the_token() {
        let t = Token::new(&fake_token()).unwrap();
        let part = crate::bank::detect::tests::tmp("hf-part2").join("m.part");
        let start = "https://huggingface.co/a/b/resolve/main/m";
        for to in [
            "https://evil.example/steal",
            "https://huggingface.co.evil.example/m",
            "https://evil.example/huggingface.co/m",
        ] {
            let f = hops(&[(start, to)]);
            fetch(&f, start, Some(&t), &part, 60, &mut |_| {}).unwrap();
            let log = f.log.lock().unwrap().clone();
            assert!(
                log.iter()
                    .filter(|(u, _)| u.contains(to))
                    .all(|(_, b)| b.is_none()),
                "{to}: {log:?}"
            );
            let _ = std::fs::remove_file(&part);
        }
        // a redirect to plain http is not followed at all
        let f = hops(&[(start, "http://huggingface.co/a")]);
        let e = fetch(&f, start, Some(&t), &part, 60, &mut |_| {}).unwrap_err();
        assert!(e.contains("not https"), "{e}");
        assert_eq!(
            f.log.lock().unwrap().len(),
            1,
            "nothing was sent to the http address"
        );
        // a redirect loop ends
        let f = hops(&[(start, start)]);
        assert!(fetch(&f, start, Some(&t), &part, 60, &mut |_| {})
            .unwrap_err()
            .contains("too many"));
        // and a token is never offered to a model that is not on huggingface.co at all
        let f = hops(&[]);
        fetch(
            &f,
            "https://example.org/x.bin",
            Some(&t),
            &part,
            60,
            &mut |_| {},
        )
        .ok();
        assert!(f.log.lock().unwrap().iter().all(|(_, b)| b.is_none()));
    }

    #[test]
    fn a_no_from_huggingface_says_what_to_do_and_never_shows_the_token() {
        let tok = fake_token();
        let t = Token::new(&tok).unwrap();
        let part = crate::bank::detect::tests::tmp("hf-part3").join("m.part");
        let url = "https://huggingface.co/black-forest-labs/x/resolve/main/m.safetensors";
        let mut f = hops(&[]);
        f.deny = Some(401);
        let e = fetch(&f, url, None, &part, 60, &mut |_| {}).unwrap_err();
        assert!(
            e.contains("wants a token")
                && e.contains("HF_TOKEN")
                && e.contains("black-forest-labs/x"),
            "{e}"
        );
        let mut f = hops(&[]);
        f.deny = Some(403);
        // a token that was sent and still got a 403: the licence is not accepted by that account
        struct Denies;
        impl Fetch for Denies {
            fn get(&self, _: &str, _: Option<&str>, _: bool) -> crate::setup::registry::Fetched {
                crate::setup::registry::Fetched::Failed
            }
            fn probe(&self, _: &str, _: Option<&str>) -> Result<Probe, String> {
                Ok(Probe::Denied(403))
            }
        }
        let e = fetch(&Denies, url, Some(&t), &part, 60, &mut |_| {}).unwrap_err();
        assert!(
            e.contains("turned your token away")
                && e.contains("accept its licence")
                && !e.contains(&tok),
            "{e}"
        );
        // an error that echoes the token is blanked
        struct Echoes(String);
        impl Fetch for Echoes {
            fn get(&self, _: &str, _: Option<&str>, _: bool) -> crate::setup::registry::Fetched {
                crate::setup::registry::Fetched::Failed
            }
            fn probe(&self, _: &str, b: Option<&str>) -> Result<Probe, String> {
                Err(format!("curl said {} {}", b.unwrap_or(""), self.0))
            }
        }
        let e = fetch(&Echoes(tok.clone()), url, Some(&t), &part, 60, &mut |_| {}).unwrap_err();
        assert!(!e.contains(&tok) && e.contains("[token]"), "{e}");
        // the notice names the licence, says gated, and never holds a token
        let n = gated_notice("m.safetensors", "FLUX Kommunity License", url, true);
        assert!(
            n.contains("gated")
                && n.contains("non-commercial")
                && n.contains("black-forest-labs/x")
                && !n.contains(&tok)
        );
        let n = gated_notice("m.safetensors", "LTX-2.x Community License", url, false);
        assert!(n.contains("$10M") && n.contains("HF_TOKEN"));
    }

    /// The real curl wrapper, with a program that only writes down how it was called: the token
    /// is on stdin as a config line and nowhere on the command line, and a redirect is not followed.
    #[cfg(unix)]
    #[test]
    fn curl_gets_the_token_on_stdin_and_never_on_its_command_line() {
        use std::os::unix::fs::PermissionsExt;
        let d = crate::bank::detect::tests::tmp("hf-curl");
        let log = d.join("calls.txt");
        let prog = d.join("curl-like");
        std::fs::write(
            &prog,
            format!(
                "#!/bin/sh\nprintf 'ARGS: %s\\n' \"$*\" >> {log}\nprintf 'STDIN: ' >> {log}\ncat >> {log}\nprintf '\\n' >> {log}\nprintf '200 '\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
        let curl = crate::setup::registry::Curl {
            program: prog.to_string_lossy().into_owned(),
        };
        let tok = fake_token();
        let r = curl
            .probe("https://huggingface.co/a/b/resolve/main/m", Some(&tok))
            .unwrap();
        assert_eq!(r, Probe::Here);
        let part = d.join("m.part");
        curl.download_hop(
            "https://huggingface.co/a/b/resolve/main/m",
            &part,
            60,
            &mut |_| {},
            Some(&tok),
        )
        .unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        let args: Vec<&str> = text.lines().filter(|l| l.starts_with("ARGS:")).collect();
        assert_eq!(args.len(), 2);
        assert!(args.iter().all(|a| !a.contains(&tok)), "{args:?}");
        assert!(
            args.iter()
                .all(|a| a.contains("--max-redirs 0") || a.contains("--max-redirs"))
                && args
                    .iter()
                    .all(|a| !a.contains("--location") && !a.contains("-L")),
            "{args:?}"
        );
        assert!(
            text.contains(&format!("header = \"Authorization: Bearer {tok}\"")),
            "{text}"
        );
        // without a token nothing is written to stdin
        let _ = std::fs::remove_file(&log);
        curl.probe("https://cdn.example/x", None).unwrap();
        assert!(!std::fs::read_to_string(&log).unwrap().contains("Bearer"));
    }
}
