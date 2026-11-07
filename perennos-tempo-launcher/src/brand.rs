//! The one Perennos look for every page the launcher writes (welcome, setup plan, Conductor, the
//! problem page): the shared tokens, the official wordmark and insignia (with its LED circuit) and a script-free theme switch.
//! The pages carry no script, so Light / Dark / System is a radio group that the stylesheet reads
//! with `:has()`; it follows the system until you pick, and a pick is not remembered.
//! `assets/tempo-theme.css` and the artwork and fonts beside it are the same bytes in every Tempo crate.

/// Tokens, base styles and motion shared by every Tempo surface.
pub const THEME_CSS: &str = include_str!("../assets/tempo-theme.css");

// The official artwork and the site's own fonts (SIL OFL 1.1). The pages are single files that must
// work from disk with nothing fetched, so each is embedded as a data address, once per page.
// To swap the artwork, replace the files in assets/ (the same names in every crate).
const INSIGNIA: &[u8] = include_bytes!("../assets/perennos-insignia.webp");
const WORDMARK: &[u8] = include_bytes!("../assets/perennos-wordmark-cream.png");
const FAVICON_PNG: &[u8] = include_bytes!("../assets/favicon-64.png");
const FONT_SANS: &[u8] = include_bytes!("../assets/fonts/archivo-latin-variable.woff2");
const FONT_SERIF: &[u8] = include_bytes!("../assets/fonts/instrument-serif-italic-latin.woff2");
const FONT_LABEL: &[u8] = include_bytes!("../assets/fonts/jetbrains-mono-latin-700.woff2");

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The one place the pages name their artwork and fonts: the --wm-src / --ins-src the shared sheet
/// reads, and the @font-face rules.
fn brand_css() -> String {
    format!(
        "@font-face{{font-family:\"Archivo\";src:url(data:font/woff2;base64,{sans}) format(\"woff2\");font-weight:100 900;font-style:normal;font-display:swap}}\n\
@font-face{{font-family:\"Instrument Serif\";src:url(data:font/woff2;base64,{serif}) format(\"woff2\");font-weight:400;font-style:italic;font-display:swap}}\n\
@font-face{{font-family:\"JetBrains Mono\";src:url(data:font/woff2;base64,{label}) format(\"woff2\");font-weight:700;font-style:normal;font-display:swap}}\n\
:root{{--wm-src:url(data:image/png;base64,{wm});--ins-src:url(data:image/webp;base64,{ins})}}\n",
        sans = b64(FONT_SANS),
        serif = b64(FONT_SERIF),
        label = b64(FONT_LABEL),
        wm = b64(WORDMARK),
        ins = b64(INSIGNIA),
    )
}

/// The insignia as the pages show it: the stone P and, laid over its lit channel, the LED circuit
/// (CSS only). `data-insignia` marks the one element a live version can replace.
const INSIGNIA_EL: &str = r#"<span class="tt-ins" data-insignia><i class="tt-led"></i></span>"#;

/// What the pages need on top of the shared sheet: the header bar, the page, panels, cards, code.
const PAGE_CSS: &str = r#"
body{min-height:100vh;font:15px/1.6 var(--sans)}
.bar{display:flex;align-items:center;justify-content:space-between;gap:16px;max-width:1040px;margin:0 auto;padding:18px 24px;border-bottom:1px solid var(--line);animation:tt-fade .6s ease both}
.bar .tt-brand .tt-ins{height:34px}.bar .tt-brand .tt-wm{height:17px}.bar .tt-brand b{font-size:16px}
main{position:relative;width:min(960px,100%);margin:0 auto;padding:40px 24px 72px}
.hero{float:right;margin:0 0 14px 28px;line-height:0;animation:tt-fade 1.2s ease both}
.made{margin-top:56px;padding-top:14px;border-top:1px solid var(--line);font:500 10px/1.4 var(--label);letter-spacing:.14em;text-transform:uppercase;color:var(--mute)}
h1{margin:0 0 12px;font:italic 400 clamp(28px,5vw,40px)/1.1 var(--serif);letter-spacing:-.005em;color:var(--ink);overflow-wrap:anywhere;max-width:16em}
h2{clear:both;margin:48px 0 10px;padding-top:14px;border-top:1px solid var(--line);font:500 11px/1.4 var(--mono);letter-spacing:.18em;text-transform:uppercase;color:var(--mute)}
h3{margin:0 0 6px;font:600 15px/1.4 var(--sans);color:var(--ink);overflow-wrap:anywhere}
h3.bandname{margin:22px 0 4px;font:italic 400 24px/1.2 var(--serif)}
h3.sub{margin:22px 0 8px;font:500 10.5px/1.4 var(--mono);letter-spacing:.18em;text-transform:uppercase;color:var(--silver)}
p,li{color:var(--frost);overflow-wrap:anywhere}
p{margin:0 0 12px}
.lede{font-size:16.5px;max-width:62ch;margin:6px 0 12px}
ul{padding-left:20px}
code{font:12.5px/1.5 var(--mono);color:var(--ink);background:var(--codebg);padding:1px 5px;border-radius:var(--r);overflow-wrap:anywhere}
pre{margin:8px 0 14px;padding:12px 14px;overflow:auto;background:var(--bg2);border:1px solid var(--line2);border-left:2px solid var(--line3);border-radius:var(--r);font:12.5px/1.55 var(--mono);color:var(--ink);user-select:all;white-space:pre}
pre code{background:none;padding:0}
a{color:var(--warm2);text-underline-offset:3px}
.small{font-size:12.5px;color:var(--mute)}
.step{font-size:13px;color:var(--silver);margin:10px 0 0}
.verdict{margin:0 0 12px;padding:14px 16px;background:var(--warm-soft);border:1px solid var(--line2);border-left:2px solid var(--warm);border-radius:var(--r);color:var(--ink)}
.choices{display:grid;grid-template-columns:1fr 1fr;gap:16px;margin-top:14px;align-items:start}
.card,.flow,.share,.room{position:relative;min-width:0;background:var(--panel);border:1px solid var(--line2);border-radius:var(--r);padding:16px 18px 8px}
.card::before,.share::before{content:"";position:absolute;left:-5px;top:-5px;width:9px;height:9px;border-left:1px solid var(--line3);border-top:1px solid var(--line3)}
.card::after,.share::after{content:"";position:absolute;right:-5px;bottom:-5px;width:9px;height:9px;border-right:1px solid var(--line3);border-bottom:1px solid var(--line3)}
.card,.flow,.runs li{transition:transform var(--dur) var(--ease),border-color var(--dur) var(--ease)}
.card:hover,.flow:hover,.runs li:hover{transform:translateY(-2px);border-color:var(--line3)}
.card p{margin:0 0 8px}
.cards{display:grid;grid-template-columns:repeat(auto-fill,minmax(min(230px,100%),1fr));gap:16px}
.card.down{border-style:dashed}
.inst{display:flex;align-items:center;gap:8px;margin:0 0 6px!important;font:500 10px/1.2 var(--mono);letter-spacing:.18em;text-transform:uppercase;color:var(--silver)}
.inst.up{color:var(--warm2)}
.inst.up::before,.inst.down::before{content:"";width:6px;height:6px;border-radius:50%;background:var(--warm);flex:none;animation:tt-breathe 2.4s ease-in-out infinite}
.inst.down::before{background:none;border:1px dashed var(--mute);animation:none}
.inst.down{color:var(--mute)}
.handle{font:12.5px/1.5 var(--mono);color:var(--ink);overflow-wrap:anywhere}
.models{margin:8px 0 0;padding-left:18px}.models li{overflow-wrap:anywhere}
.band{margin-bottom:30px}
.flow{margin-bottom:14px}.flow p{margin:6px 0 0}
.chip{display:inline-block;padding:1px 9px;border:1px solid var(--line2);border-radius:var(--r);font:500 11px/1.6 var(--mono);letter-spacing:.06em;color:var(--ink)}
.arrow{color:var(--mute)}.steps{overflow-wrap:anywhere}
.room{margin-top:14px;border-left:2px solid var(--warm);background:var(--warm-soft)}
.room p{margin:0 0 8px;color:var(--ink)}
.runs{padding-left:0;list-style:none;margin:0}
.runs li{margin-bottom:10px;padding:10px 14px;background:var(--panel);border:1px solid var(--line2);border-radius:var(--r);overflow-wrap:anywhere}
.dot{display:inline-block;width:7px;height:7px;margin-right:8px;border-radius:50%;background:var(--warm);vertical-align:1px}
.share{padding-bottom:8px}
.lead{min-height:calc(100vh - 140px);display:flex;flex-direction:column;justify-content:center}
@media (max-width:720px){.bar{padding:12px 16px}.bar .tt-brand .tt-wm{height:14px}main{padding:28px 16px 56px}.choices{grid-template-columns:1fr}.hero{margin-left:14px}.hero .tt-ins{height:150px}h1{font-size:28px}.tt-seg>label{padding:0 8px}h2{margin-top:36px}}
"#;

/// Everything from `<!doctype html>` to the opening `<main>`: the head (favicon, fonts, the theme and page
/// styles and, when `csp` is set, a policy that allows nothing but those), the header bar with the
/// wordmark and the Light / Dark / System switch, and the big insignia behind the page when `hero` is set.
pub fn page_top(title: &str, csp: bool, hero: bool) -> String {
    let policy = if csp {
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; base-uri 'none'; form-action 'none'\">\n"
    } else {
        ""
    };
    let hero = if hero {
        "<div class=\"hero\" aria-hidden=\"true\"><span class=\"tt-ins hero\" data-insignia><i class=\"tt-led\"></i></span></div>\n".to_string()
    } else {
        String::new()
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
{policy}<meta name=\"color-scheme\" content=\"dark light\">\n<link rel=\"icon\" type=\"image/png\" href=\"data:image/png;base64,{icon}\">\n<title>{title}</title>\n<style>\n{brand}{THEME_CSS}{PAGE_CSS}</style></head>\n\
<body class=\"tt-lattice\">\n\
<header class=\"bar\"><span class=\"tt-brand\">{INSIGNIA_EL}<span class=\"tt-wm\" role=\"img\" aria-label=\"perennos\"></span><b>tempo</b></span>\n\
<div class=\"tt-seg tt-theme\" role=\"radiogroup\" aria-label=\"Theme\"><input type=\"radio\" name=\"tt-theme\" id=\"tt-system\" checked><label for=\"tt-system\">System</label><input type=\"radio\" name=\"tt-theme\" id=\"tt-light\"><label for=\"tt-light\">Light</label><input type=\"radio\" name=\"tt-theme\" id=\"tt-dark\"><label for=\"tt-dark\">Dark</label></div></header>\n\
<main class=\"tt-rise\">\n{hero}",
        icon = b64(FAVICON_PNG),
        brand = brand_css(),
    )
}

/// The close of every page.
pub const PAGE_END: &str = "<p class=\"small made\">Built and maintained in the Cayman Islands</p>\n</main></body></html>\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_known_answers() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn the_artwork_is_the_official_files() {
        assert_eq!(&INSIGNIA[..4], b"RIFF", "webp");
        assert_eq!(&WORDMARK[1..4], b"PNG");
        assert_eq!(&FAVICON_PNG[1..4], b"PNG");
        assert_eq!(&FONT_SANS[..4], b"wOF2");
    }

    #[test]
    fn a_page_has_the_theme_switch_the_brand_and_no_script() {
        let top = page_top("T", true, true);
        for id in ["tt-system", "tt-light", "tt-dark"] {
            assert!(top.contains(&format!("id=\"{id}\"")), "{id}");
        }
        assert!(
            top.contains("class=\"tt-wm\"")
                && top.contains("<b>tempo</b>")
                && top.contains("tt-led")
        );
        assert!(top.contains("Content-Security-Policy") && top.contains("default-src 'none'"));
        assert!(!top.contains("<script"));
        assert!(
            !top.contains("http://") && !top.contains("https://"),
            "nothing external"
        );
        assert!(!page_top("T", false, false).contains("Content-Security-Policy"));
        assert!(PAGE_END.contains("Cayman Islands"));
    }

    #[test]
    fn the_look_has_no_green_accent() {
        let css = THEME_CSS.to_lowercase();
        for g in ["#b8ff3b", "#4f8a00", "#46b35f", "#8fcf9a", "--learn"] {
            assert!(!css.contains(g), "{g}");
        }
    }
}
