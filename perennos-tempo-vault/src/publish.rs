//! `tempo-vault publish`: a static site from the vault (the notes marked `publish: true`, or all
//! of them), for sharing without a server. Pages link to each other with relative paths, so the
//! folder works from disk, a USB stick or any static host. Only the attachments the published
//! notes use are copied. Nothing outside the output folder is touched, and nothing is deleted.
use crate::render::{self, esc, Opts};
use crate::util::{parent_of, relative_path, strip_md};
use crate::vault::Vault;
use std::collections::BTreeSet;
use std::path::Path;

pub const SITE_CSS: &str = include_str!("../assets/publish.css");

#[derive(Debug, Default, PartialEq)]
pub struct Published {
    pub pages: usize,
    pub files: usize,
}

fn is_published(v: &Vault, p: &str, all: bool) -> bool {
    all || v
        .note(p)
        .and_then(|n| n.parsed.prop("publish"))
        .is_some_and(|x| {
            x.items()
                .first()
                .is_some_and(|s| matches!(s.to_lowercase().as_str(), "true" | "yes" | "1"))
        })
}

fn html_path(p: &str) -> String {
    format!("{}.html", strip_md(p))
}

fn page(title: &str, css_href: &str, home_href: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'self'; img-src 'self' https: data:; media-src 'self'; style-src 'self'; script-src 'none'; base-uri 'none'; form-action 'none'\">\
<title>{}</title><link rel=\"stylesheet\" href=\"{css_href}\"></head><body><header><a class=\"home\" href=\"{home_href}\">Perennos Tempo Vault</a></header><main>{body}</main></body></html>\n",
        esc(title)
    )
}

/// Where `out` will be, with links resolved, before anything is created.
fn resolve_target(out: &Path) -> Result<std::path::PathBuf, String> {
    let abs = std::path::absolute(out).map_err(|e| e.to_string())?;
    let mut tail = vec![];
    let mut cur = abs.as_path();
    loop {
        if let Ok(c) = cur.canonicalize() {
            return Ok(tail.iter().rev().fold(c, |p, s| p.join(s)));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(p), Some(n)) => {
                tail.push(n.to_owned());
                cur = p;
            }
            _ => return Err(format!("cannot place the site at {}", out.display())),
        }
    }
}

/// An attachment that would run script if opened on its own from the site: HTML-like files, and
/// SVG that carries script, event handlers or embedded pages. Images are only ever shown as
/// `<img>`, but the copied file can be opened directly.
fn is_active(v: &Vault, rel: &str) -> bool {
    let ext = render::ext(rel);
    if matches!(
        ext.as_str(),
        "html" | "htm" | "xhtml" | "xht" | "xml" | "js" | "mjs"
    ) {
        return true;
    }
    if ext != "svg" {
        return false;
    }
    let Some(b) = v.safe_abs(rel).and_then(|p| std::fs::read(p).ok()) else {
        return true;
    };
    let t = String::from_utf8_lossy(&b).to_lowercase();
    let on_attr = t.match_indices("on").any(|(i, _)| {
        let before = t[..i].chars().next_back();
        let rest = t[i + 2..].trim_start_matches(|c: char| c.is_ascii_alphabetic());
        before.is_some_and(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '/')
            && rest.len() < t.len() - i - 2
            && rest.trim_start().starts_with('=')
    });
    on_attr
        || [
            "<script",
            "javascript:",
            "<foreignobject",
            "<iframe",
            "<embed",
            "<object",
            "<!entity",
        ]
        .iter()
        .any(|n| t.contains(n))
}

/// Write the site into `out`, which must not be inside the vault. Only the published notes'
/// own text reaches the site.
pub fn publish(v: &Vault, out: &Path, all: bool) -> Result<Published, String> {
    let root = v.root.canonicalize().map_err(|e| e.to_string())?;
    if resolve_target(out)?.starts_with(&root) {
        return Err(
            "publish into a folder outside the vault (the site would be indexed as attachments)"
                .into(),
        );
    }
    std::fs::create_dir_all(out).map_err(|e| format!("cannot make {}: {e}", out.display()))?;
    let notes: Vec<String> = v
        .notes()
        .filter(|n| is_published(v, &n.path, all))
        .map(|n| n.path.clone())
        .collect();
    let set: BTreeSet<String> = notes.iter().cloned().collect();
    // pages are rendered from a vault of the published notes alone: an embed, a query, a tasks
    // list or a base on a published page cannot pull in text from a note that is not published
    let v = &v.only(&set);
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut r = Published::default();
    for p in &notes {
        let dir = parent_of(p).to_string();
        let set2 = set.clone();
        let d2 = dir.clone();
        let note_href = move |t: &str, anchor: Option<&str>| -> String {
            if set2.contains(t) {
                let mut h = relative_path(&d2, &html_path(t));
                if let Some(a) = anchor {
                    h.push('#');
                    h.push_str(&render::slug(a.trim_start_matches('^')));
                }
                crate::util::pct_encode_path(&h).replace("%23", "#")
            } else {
                "#not-published".into()
            }
        };
        let d3 = dir.clone();
        let file_src = move |f: &str| crate::util::pct_encode_path(&relative_path(&d3, f));
        let tag_href = |_: &str| "#".to_string();
        let o = Opts {
            note_href: &note_href,
            file_src: &file_src,
            tag_href: &tag_href,
            interactive: false,
            max_depth: 2,
        };
        let n = v.note(p).ok_or("note vanished")?;
        let body = render::note_html(v, p, &o).unwrap_or_default();
        let props = render::props_html(
            Some(v),
            p,
            &n.parsed
                .props
                .iter()
                .filter(|(k, _)| !k.eq_ignore_ascii_case("publish"))
                .cloned()
                .collect::<Vec<_>>(),
            &o,
        );
        let back: Vec<String> = v
            .backlinks(p)
            .into_iter()
            .map(|b| b.source)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|s| set.contains(s))
            .map(|s| {
                format!(
                    "<li><a href=\"{}\">{}</a></li>",
                    esc(&note_href(&s, None)),
                    esc(&v.note(&s).map(|n| n.title.clone()).unwrap_or_default())
                )
            })
            .collect();
        let back_html = if back.is_empty() {
            String::new()
        } else {
            format!(
                "<section class=\"backlinks\"><h2>Linked from</h2><ul>{}</ul></section>",
                back.join("")
            )
        };
        for (l, t) in v.links_of(p) {
            if let Some(t) = t.filter(|t| v.has_file(t) && !is_active(v, t)) {
                let _ = l;
                files.insert(t);
            }
        }
        let depth = p.matches('/').count();
        let up = "../".repeat(depth);
        let html = page(
            &n.title,
            &format!("{up}style.css"),
            &format!("{up}index.html"),
            &format!(
                "<article><h1 class=\"title\">{}</h1>{props}{body}</article>{back_html}",
                esc(&n.title)
            ),
        );
        let dest = out.join(html_path(p).replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(d) = dest.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        std::fs::write(&dest, html).map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
        r.pages += 1;
    }
    for f in &files {
        let dest = out.join(f.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(d) = dest.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        std::fs::copy(
            v.safe_abs(f).ok_or(format!("{f} goes through a link"))?,
            &dest,
        )
        .map_err(|e| format!("cannot copy {f}: {e}"))?;
        r.files += 1;
    }
    let mut list = String::from("<h1 class=\"title\">Notes</h1><ul class=\"index\">");
    for p in &notes {
        let t = v.note(p).map(|n| n.title.clone()).unwrap_or_default();
        list.push_str(&format!(
            "<li><a href=\"{}\">{}</a> <span class=\"path\">{}</span></li>",
            esc(&crate::util::pct_encode_path(&html_path(p))),
            esc(&t),
            esc(p)
        ));
    }
    list.push_str("</ul>");
    std::fs::write(
        out.join("index.html"),
        page("Perennos Tempo Vault", "style.css", "index.html", &list),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(out.join("style.css"), SITE_CSS).map_err(|e| e.to_string())?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::tests::{put, sample, tmp};

    #[test]
    fn publishes_marked_notes_with_relative_links() {
        let (d, mut v) = sample();
        put(
            &d,
            "Projects/Tempo.md",
            "---\npublish: true\n---\n# Tempo\nSee [[Ideas]] and [[Home]] ![[img/logo.png]]\n",
        );
        put(
            &d,
            "Ideas.md",
            "---\npublish: yes\n---\n# Ideas\nBack to [[Projects/Tempo|the project]].\n",
        );
        v.refresh();
        let out = tmp("site");
        let r = publish(&v, &out, false).unwrap();
        assert_eq!(r, Published { pages: 2, files: 1 });
        let page = std::fs::read_to_string(out.join("Projects/Tempo.html")).unwrap();
        assert!(
            page.contains("href=\"../Ideas.html\"") && page.contains("href=\"#not-published\""),
            "{page}"
        );
        assert!(page.contains("src=\"../img/logo.png\"") && out.join("img/logo.png").is_file());
        assert!(page.contains("<h2>Linked from</h2>") && page.contains("script-src 'none'"));
        assert!(
            !page.contains("<th>publish</th>"),
            "the publish flag is not shown"
        );
        assert!(std::fs::read_to_string(out.join("index.html"))
            .unwrap()
            .contains("Projects/Tempo.html"));
        assert!(
            !out.join("Home.html").exists(),
            "unmarked notes stay private"
        );
        assert!(
            publish(&v, &d.join("site"), true).is_err(),
            "never inside the vault"
        );
    }

    #[test]
    fn a_published_page_never_shows_an_unpublished_note() {
        let (d, mut v) = sample();
        put(
            &d,
            "Diary.md",
            "---\nmood: secret-mood\n---\n# Diary\nsecret-diary-text\n- [ ] secret-task\n",
        );
        put(
            &d,
            "Public.md",
            "---\npublish: true\n---\n# Public\n![[Diary]]\n\n```query\nsecret\n```\n\n```tasks\nnot done\n```\n\n```dataview\nTABLE mood FROM \"\"\n```\n\n```base\nviews:\n  - type: table\n    order: [file.name, mood, file.backlinks]\n```\n",
        );
        v.refresh();
        let out = tmp("site-private");
        publish(&v, &out, false).unwrap();
        let page = std::fs::read_to_string(out.join("Public.html")).unwrap();
        // the page keeps its own words (the link text "Diary"), never the other note's
        for leak in ["secret-diary-text", "secret-task", "secret-mood", "Home"] {
            assert!(!page.contains(leak), "{leak} leaked into {page}");
        }
        assert!(page.contains("Public"), "{page}");
    }
}
