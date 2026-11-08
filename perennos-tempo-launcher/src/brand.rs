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
const FONT_DIDONE: &[u8] = include_bytes!("../assets/fonts/bodoni-moda-latin-variable.woff2");
const FONT_DIDONE_IT: &[u8] =
    include_bytes!("../assets/fonts/bodoni-moda-italic-latin-variable.woff2");

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
        "@font-face{{font-family:\"Archivo\";src:url(data:font/woff2;base64,{sans}) format(\"woff2\");font-weight:100 900;font-stretch:62% 125%;font-style:normal;font-display:swap}}\n\
@font-face{{font-family:\"Instrument Serif\";src:url(data:font/woff2;base64,{serif}) format(\"woff2\");font-weight:400;font-style:italic;font-display:swap}}\n\
@font-face{{font-family:\"JetBrains Mono\";src:url(data:font/woff2;base64,{label}) format(\"woff2\");font-weight:700;font-style:normal;font-display:swap}}\n\
@font-face{{font-family:\"Bodoni Moda\";src:url(data:font/woff2;base64,{didone}) format(\"woff2\");font-weight:400 700;font-style:normal;font-display:swap}}\n\
@font-face{{font-family:\"Bodoni Moda\";src:url(data:font/woff2;base64,{didone_it}) format(\"woff2\");font-weight:400 700;font-style:italic;font-display:swap}}\n\
:root{{--wm-src:url(data:image/png;base64,{wm});--ins-src:url(data:image/webp;base64,{ins})}}\n",
        sans = b64(FONT_SANS),
        serif = b64(FONT_SERIF),
        label = b64(FONT_LABEL),
        didone = b64(FONT_DIDONE),
        didone_it = b64(FONT_DIDONE_IT),
        wm = b64(WORDMARK),
        ins = b64(INSIGNIA),
    )
}

/// The insignia as the pages show it: the stone P and, laid over its lit channel, the LED circuit
/// (CSS only). `data-insignia` marks the one element a live version can replace.
const INSIGNIA_EL: &str = r#"<span class="tt-ins" data-insignia><i class="tt-led"></i></span>"#;

/// What the pages need on top of the shared sheet: the header bar, the page, panels, cards, code.
const PAGE_CSS: &str = r#"
body{min-height:100vh;font:var(--fs-body)/1.6 var(--sans)}
.bar{display:flex;align-items:center;justify-content:space-between;gap:var(--s4);max-width:1120px;margin:0 auto;padding:var(--s5) var(--s6);position:relative;view-transition-name:bar;animation:tt-fade .8s var(--expo) both}
.bar::after{content:"";position:absolute;left:var(--s6);right:var(--s6);bottom:0;height:var(--hair);background:var(--line2);transform-origin:0 50%;animation:tt-draw 1.4s var(--expo) both}
.bar .tt-brand .tt-ins{height:34px}.bar .tt-brand .tt-wm{height:17px}.bar .tt-brand b{font:500 16px/1 var(--sans);letter-spacing:.04em}
main{position:relative;width:min(1040px,100%);margin:0 auto;padding:var(--s8) var(--s6) var(--s9);counter-reset:sec}
/* the hero plate: layered gradients, a drifting lattice, a slow light, live grain, the stone P above its LED strip */
@property --a{syntax:"<angle>";initial-value:0deg;inherits:false}
.hero{--pw:clamp(240px,32vw,360px);position:relative;float:right;width:var(--pw);margin:0 0 var(--s6) var(--s8);line-height:0;animation:tt-fade 1.4s var(--expo) both}
.plate{position:relative;display:grid;place-items:center;width:100%;aspect-ratio:4/5;overflow:hidden;isolation:isolate;border:var(--hair) solid var(--line2);
  background:radial-gradient(72% 54% at 50% 38%,var(--warm-soft),transparent 74%),radial-gradient(130% 80% at 50% 118%,var(--bg3),transparent 62%),linear-gradient(172deg,var(--bg2),var(--bg))}
.plate::before{content:"";position:absolute;inset:-32px;z-index:-1;background-image:linear-gradient(var(--line2) var(--hair),transparent var(--hair)),linear-gradient(90deg,var(--line2) var(--hair),transparent var(--hair));background-size:32px 32px;-webkit-mask-image:radial-gradient(62% 58% at 50% 46%,#000,transparent 82%);mask-image:radial-gradient(62% 58% at 50% 46%,#000,transparent 82%);animation:tt-drift 40s linear infinite}
.plate::after{content:"";position:absolute;inset:-64px;z-index:3;pointer-events:none;background:var(--grain) 0 0/200px 200px;opacity:.4;animation:tt-grain .9s steps(1) infinite}
.plate .sweep{position:absolute;inset:0;z-index:-1;background:conic-gradient(from var(--a) at 50% 44%,transparent 0 72%,var(--warm-soft) 88%,transparent 100%);animation:plate-turn 20s linear infinite}
@keyframes plate-turn{to{--a:360deg}}
.plate .tt-ins.hero{float:none;height:calc(var(--pw)*.74);width:auto;margin-top:-4%}
.plate .cap{position:absolute;z-index:2;font:700 9px/1 var(--label);letter-spacing:.3em;text-transform:uppercase;color:var(--mute)}
.plate .cap.tl{left:var(--s4);top:var(--s4)}.plate .cap.br{right:var(--s4);bottom:var(--s4);color:var(--warm2)}
.made{margin-top:var(--s9);padding-top:var(--s4);border-top:var(--hair) solid var(--line2);font:700 9.5px/1.5 var(--label);letter-spacing:.28em;text-transform:uppercase;color:var(--mute)}
h1 em{font-style:italic;color:var(--warm2)}
h1{margin:0 0 var(--s5);font:italic 400 clamp(40px,5.2vw + 12px,80px)/.96 var(--display);font-optical-sizing:auto;letter-spacing:-.04em;color:var(--ink);overflow-wrap:anywhere;max-width:10.5em}
h2{clear:both;display:flex;align-items:center;gap:var(--s3);margin:var(--s9) 0 var(--s5);font:700 10.5px/1.4 var(--label);letter-spacing:.3em;text-transform:uppercase;color:var(--silver);counter-increment:sec}
h2::before{content:counter(sec,decimal-leading-zero) "\2002\2014";color:var(--warm2);white-space:pre}
h2::after{content:"";flex:1;min-width:var(--s6);height:var(--hair);background:var(--line2);transform-origin:0 50%;animation:tt-draw 1.6s var(--expo) .3s both}
h3{margin:0 0 var(--s2);font:italic 400 var(--fs-h3)/1.12 var(--serif);letter-spacing:-.012em;color:var(--ink);overflow-wrap:anywhere;text-wrap:balance}
h3.bandname{margin:var(--s6) 0 var(--s1);font:italic 400 clamp(28px,2vw + 20px,40px)/1.05 var(--serif);letter-spacing:-.02em}
h3.sub{margin:var(--s6) 0 var(--s3);font:700 10px/1.4 var(--label);letter-spacing:.26em;text-transform:uppercase;color:var(--silver)}
p,li{color:var(--frost);overflow-wrap:anywhere}
p{margin:0 0 var(--s3)}
.lede{font:300 var(--fs-lead)/1.55 var(--sans);max-width:60ch;margin:var(--s2) 0 var(--s5);color:var(--frost)}
.tt-kicker+h1+.lede::first-letter{float:left;margin:.06em .1em 0 0;font:italic 400 3.4em/.78 var(--display);color:var(--warm2)}
ul{padding-left:var(--s5)}
li::marker{color:var(--warm2)}
li{margin:0 0 var(--s1)}
code{font:12.5px/1.5 var(--mono);color:var(--ink);background:var(--codebg);padding:1px 6px;border-radius:var(--r);overflow-wrap:anywhere}
pre{margin:var(--s2) 0 var(--s4);padding:var(--s4) var(--s5);overflow:auto;background:var(--bg2);border:var(--hair) solid var(--line2);border-left:1.5px solid var(--warm);border-radius:var(--r);font:12.5px/1.6 var(--mono);color:var(--ink);user-select:all;white-space:pre}
pre code{background:none;padding:0}
a{color:var(--warm2);text-decoration:none;background:linear-gradient(currentColor,currentColor) 0 100%/100% var(--hair) no-repeat;transition:background-size .6s var(--expo)}
a:hover{background-size:0 var(--hair);background-position:100% 100%}
.small{font-size:var(--fs-small);color:var(--mute)}
.step{font-size:13px;color:var(--silver);margin:var(--s3) 0 0}
.verdict{margin:0 0 var(--s5);padding:var(--s2) 0 var(--s2) var(--s5);background:none;border:0;border-left:1.5px solid var(--warm);font:italic 400 clamp(22px,1.6vw + 15px,34px)/1.22 var(--serif);letter-spacing:-.012em;color:var(--ink);max-width:30em;text-wrap:balance}
.choices{display:grid;grid-template-columns:1fr 1fr;gap:var(--s6);margin-top:var(--s4);align-items:start}
.card,.flow,.share,.room{position:relative;min-width:0;background:var(--panel);border:var(--hair) solid var(--line2);border-radius:var(--r);padding:var(--s5) var(--s5) var(--s3)}
.card::before,.share::before{content:"";position:absolute;inset:-5px;pointer-events:none;
  --c:linear-gradient(var(--line3),var(--line3));
  background:var(--c) 0 0/var(--tick) var(--hair),var(--c) 0 0/var(--hair) var(--tick),var(--c) 100% 0/var(--tick) var(--hair),var(--c) 100% 0/var(--hair) var(--tick),var(--c) 0 100%/var(--tick) var(--hair),var(--c) 0 100%/var(--hair) var(--tick),var(--c) 100% 100%/var(--tick) var(--hair),var(--c) 100% 100%/var(--hair) var(--tick);
  background-repeat:no-repeat;transition:inset .6s var(--expo)}
.card:hover::before{inset:-8px}
.card,.flow,.runs li{transition:transform .6s var(--expo),border-color .4s var(--expo)}
.card:hover,.flow:hover,.runs li:hover{transform:translateY(-3px);border-color:var(--line3)}
.card p{margin:0 0 var(--s2)}
.cards{display:grid;grid-template-columns:repeat(auto-fill,minmax(min(230px,100%),1fr));gap:var(--s5)}
.card.down{border-style:dashed}
.cards{margin-bottom:var(--s6)}
.inst{display:flex;align-items:center;gap:var(--s2);margin:0 0 var(--s2)!important;font:700 9.5px/1.2 var(--label);letter-spacing:.26em;text-transform:uppercase;color:var(--silver)}
.inst.up{color:var(--warm2)}
.inst.up::before,.inst.down::before{content:"";width:6px;height:6px;border-radius:50%;background:var(--warm);flex:none;animation:tt-breathe 2.4s ease-in-out infinite}
.inst.down::before{background:none;border:1px dashed var(--mute);animation:none}
.inst.down{color:var(--mute)}
.handle{font:12.5px/1.5 var(--mono);color:var(--ink);overflow-wrap:anywhere}
.models{margin:var(--s2) 0 0;padding-left:var(--s5)}.models li{overflow-wrap:anywhere}
.band{margin-bottom:var(--s7)}
.flow{margin-bottom:var(--s4)}.flow p{margin:var(--s2) 0 0}
.chip{display:inline-block;padding:1px 10px;border:var(--hair) solid var(--line3);border-radius:999px;font:500 11px/1.7 var(--mono);letter-spacing:.06em;color:var(--ink)}
.arrow{color:var(--mute)}.steps{overflow-wrap:anywhere}
.room{margin-top:var(--s4);border-left:1.5px solid var(--warm);background:var(--warm-soft)}
.room p{margin:0 0 var(--s2);color:var(--ink)}
.runs{padding-left:0;list-style:none;margin:0}
.runs li{margin-bottom:var(--s3);padding:var(--s3) var(--s4);background:var(--panel);border:var(--hair) solid var(--line2);border-radius:var(--r);overflow-wrap:anywhere}
.dot{display:inline-block;width:7px;height:7px;margin-right:var(--s2);border-radius:50%;background:var(--warm);vertical-align:1px}
.share{padding-bottom:var(--s3)}
/* an empty state says something: one italic line, a hairline above */
.empty{margin:0 0 var(--s5);padding:var(--s4) 0 0;border-top:var(--hair) solid var(--line2);font:italic 400 clamp(20px,1vw + 15px,26px)/1.3 var(--serif);letter-spacing:-.01em;color:var(--silver);max-width:34em}
.lead{min-height:calc(100vh - 140px);display:flex;flex-direction:column;justify-content:center}
@media (max-width:720px){.bar{padding:var(--s3) var(--s4)}.bar::after{left:var(--s4);right:var(--s4)}.bar .tt-brand .tt-wm{height:14px}main{padding:var(--s6) var(--s4) var(--s8)}.choices{grid-template-columns:1fr;gap:var(--s5)}
  .hero{--pw:100%;float:none;width:100%;margin:0 0 var(--s6)}.plate{aspect-ratio:auto;height:210px}.plate .tt-ins.hero{height:158px;margin-top:0}
  h2{margin-top:var(--s8)}.card,.flow,.share,.room{padding:var(--s4) var(--s4) var(--s2)}.tt-seg>label{padding:0 10px;letter-spacing:.12em}.bar .tt-brand b{display:none}}
@media (prefers-reduced-motion:reduce){.plate::after{animation:none}}
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
        "<div class=\"hero tt-ticks\" aria-hidden=\"true\"><div class=\"plate\"><i class=\"sweep\"></i><b class=\"cap tl\">Perennos</b><span class=\"tt-ins hero\" data-insignia><i class=\"tt-led\"></i></span><b class=\"cap br\">Tempo</b></div></div>\n".to_string()
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
