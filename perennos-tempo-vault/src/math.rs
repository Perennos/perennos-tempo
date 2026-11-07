//! TeX math (`$x^2$`, `$$...$$`) as MathML, which browsers draw natively: no script, no fonts
//! to load, and it works on published pages under their strict Content Security Policy. The
//! common LaTeX of notes is covered: fractions, roots, scripts and limits, Greek, operators and
//! arrows, big operators, `\left ... \right`, accents, `\text` and the math alphabets
//! (`\mathbb{R}`, `\mathcal{L}`, `\mathbf{v}`), matrices, cases and aligned equations. Anything
//! else is shown as it was written, marked, and the TeX is kept for copying.
use crate::render::esc;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Cmd(String),
    Open,
    Close,
    Sup,
    Sub,
    Amp,
    Ch(char),
    /// The `{...}` after `\text` and its kin, spaces kept.
    Text(String),
}

const TEXT_CMDS: &[&str] = &[
    "text",
    "textrm",
    "textnormal",
    "mbox",
    "textit",
    "textbf",
    "textsf",
    "texttt",
    "operatorname",
    "mathrm",
];

fn lex(s: &str) -> Vec<Tok> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' => {
                let start = i + 1;
                let mut j = start;
                while j < c.len() && c[j].is_ascii_alphabetic() {
                    j += 1;
                }
                if j == start && j < c.len() {
                    j += 1; // \, \; \{ \\ ...
                }
                let name: String = c[start..j].iter().collect();
                i = j;
                if TEXT_CMDS.contains(&name.as_str()) {
                    let mut k = i;
                    while k < c.len() && c[k].is_whitespace() {
                        k += 1;
                    }
                    if c.get(k) == Some(&'{') {
                        let (mut depth, mut e) = (0, k);
                        while e < c.len() {
                            match c[e] {
                                '{' => depth += 1,
                                '}' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            e += 1;
                        }
                        out.push(Tok::Cmd(name));
                        out.push(Tok::Text(c[k + 1..e.min(c.len())].iter().collect()));
                        i = (e + 1).min(c.len());
                        continue;
                    }
                }
                out.push(Tok::Cmd(name));
                continue;
            }
            '{' => out.push(Tok::Open),
            '}' => out.push(Tok::Close),
            '^' => out.push(Tok::Sup),
            '_' => out.push(Tok::Sub),
            '&' => out.push(Tok::Amp),
            '%' => {
                // a comment runs to the end of the line
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            ch if ch.is_whitespace() => {}
            ch => out.push(Tok::Ch(ch)),
        }
        i += 1;
    }
    out
}

const GREEK: &[(&str, char)] = &[
    ("alpha", 'α'),
    ("beta", 'β'),
    ("gamma", 'γ'),
    ("delta", 'δ'),
    ("epsilon", 'ϵ'),
    ("varepsilon", 'ε'),
    ("zeta", 'ζ'),
    ("eta", 'η'),
    ("theta", 'θ'),
    ("vartheta", 'ϑ'),
    ("iota", 'ι'),
    ("kappa", 'κ'),
    ("lambda", 'λ'),
    ("mu", 'μ'),
    ("nu", 'ν'),
    ("xi", 'ξ'),
    ("pi", 'π'),
    ("varpi", 'ϖ'),
    ("rho", 'ρ'),
    ("varrho", 'ϱ'),
    ("sigma", 'σ'),
    ("varsigma", 'ς'),
    ("tau", 'τ'),
    ("upsilon", 'υ'),
    ("phi", 'ϕ'),
    ("varphi", 'φ'),
    ("chi", 'χ'),
    ("psi", 'ψ'),
    ("omega", 'ω'),
    ("Gamma", 'Γ'),
    ("Delta", 'Δ'),
    ("Theta", 'Θ'),
    ("Lambda", 'Λ'),
    ("Xi", 'Ξ'),
    ("Pi", 'Π'),
    ("Sigma", 'Σ'),
    ("Upsilon", 'Υ'),
    ("Phi", 'Φ'),
    ("Psi", 'Ψ'),
    ("Omega", 'Ω'),
];

/// Symbols that are identifiers (drawn upright, no operator spacing).
const IDENT: &[(&str, &str)] = &[
    ("infty", "∞"),
    ("partial", "∂"),
    ("nabla", "∇"),
    ("emptyset", "∅"),
    ("varnothing", "∅"),
    ("hbar", "ℏ"),
    ("ell", "ℓ"),
    ("Re", "ℜ"),
    ("Im", "ℑ"),
    ("aleph", "ℵ"),
    ("wp", "℘"),
    ("imath", "ı"),
    ("jmath", "ȷ"),
    ("degree", "°"),
    ("top", "⊤"),
    ("bot", "⊥"),
    ("angle", "∠"),
    ("triangle", "△"),
    ("prime", "′"),
    ("checkmark", "✓"),
    ("dagger", "†"),
    ("S", "§"),
];

/// Operators, relations, arrows and fences.
const OPS: &[(&str, &str)] = &[
    ("pm", "±"),
    ("mp", "∓"),
    ("times", "×"),
    ("cdot", "⋅"),
    ("div", "÷"),
    ("ast", "∗"),
    ("star", "⋆"),
    ("circ", "∘"),
    ("bullet", "∙"),
    ("oplus", "⊕"),
    ("ominus", "⊖"),
    ("otimes", "⊗"),
    ("odot", "⊙"),
    ("cup", "∪"),
    ("cap", "∩"),
    ("setminus", "∖"),
    ("wedge", "∧"),
    ("land", "∧"),
    ("vee", "∨"),
    ("lor", "∨"),
    ("neg", "¬"),
    ("lnot", "¬"),
    ("leq", "≤"),
    ("le", "≤"),
    ("geq", "≥"),
    ("ge", "≥"),
    ("neq", "≠"),
    ("ne", "≠"),
    ("approx", "≈"),
    ("equiv", "≡"),
    ("sim", "∼"),
    ("simeq", "≃"),
    ("cong", "≅"),
    ("propto", "∝"),
    ("ll", "≪"),
    ("gg", "≫"),
    ("prec", "≺"),
    ("succ", "≻"),
    ("in", "∈"),
    ("notin", "∉"),
    ("ni", "∋"),
    ("subset", "⊂"),
    ("subseteq", "⊆"),
    ("supset", "⊃"),
    ("supseteq", "⊇"),
    ("mid", "∣"),
    ("nmid", "∤"),
    ("parallel", "∥"),
    ("perp", "⟂"),
    ("forall", "∀"),
    ("exists", "∃"),
    ("nexists", "∄"),
    ("to", "→"),
    ("rightarrow", "→"),
    ("leftarrow", "←"),
    ("gets", "←"),
    ("leftrightarrow", "↔"),
    ("Rightarrow", "⇒"),
    ("Leftarrow", "⇐"),
    ("Leftrightarrow", "⇔"),
    ("implies", "⟹"),
    ("impliedby", "⟸"),
    ("iff", "⟺"),
    ("mapsto", "↦"),
    ("longrightarrow", "⟶"),
    ("longleftarrow", "⟵"),
    ("uparrow", "↑"),
    ("downarrow", "↓"),
    ("updownarrow", "↕"),
    ("nearrow", "↗"),
    ("searrow", "↘"),
    ("hookrightarrow", "↪"),
    ("ldots", "…"),
    ("dots", "…"),
    ("cdots", "⋯"),
    ("vdots", "⋮"),
    ("ddots", "⋱"),
    ("therefore", "∴"),
    ("because", "∵"),
    ("langle", "⟨"),
    ("rangle", "⟩"),
    ("lfloor", "⌊"),
    ("rfloor", "⌋"),
    ("lceil", "⌈"),
    ("rceil", "⌉"),
    ("vert", "|"),
    ("Vert", "‖"),
    ("|", "‖"),
    ("{", "{"),
    ("}", "}"),
    ("lbrace", "{"),
    ("rbrace", "}"),
    ("backslash", "∖"),
    ("colon", ":"),
    ("#", "#"),
    ("$", "$"),
    ("%", "%"),
    ("&", "&"),
    ("_", "_"),
    ("models", "⊨"),
    ("vdash", "⊢"),
    ("dashv", "⊣"),
    ("triangleq", "≜"),
    ("coloneqq", "≔"),
];

/// Big operators: limits go under and over them in display math.
const BIG: &[(&str, &str)] = &[
    ("sum", "∑"),
    ("prod", "∏"),
    ("coprod", "∐"),
    ("int", "∫"),
    ("iint", "∬"),
    ("iiint", "∭"),
    ("oint", "∮"),
    ("bigcup", "⋃"),
    ("bigcap", "⋂"),
    ("bigoplus", "⨁"),
    ("bigotimes", "⨂"),
    ("bigvee", "⋁"),
    ("bigwedge", "⋀"),
];

const FUNCS: &[&str] = &[
    "sin", "cos", "tan", "cot", "sec", "csc", "arcsin", "arccos", "arctan", "sinh", "cosh", "tanh",
    "coth", "log", "ln", "lg", "exp", "det", "dim", "ker", "deg", "gcd", "hom", "arg", "Pr", "mod",
    "bmod", "tr", "rank",
];
/// Functions whose subscript goes underneath in display math.
const LIMFUNCS: &[&str] = &[
    "lim", "limsup", "liminf", "max", "min", "sup", "inf", "argmax", "argmin",
];

const ACCENTS: &[(&str, &str, bool)] = &[
    ("hat", "^", false),
    ("widehat", "^", true),
    ("bar", "¯", false),
    ("overline", "‾", true),
    ("vec", "→", false),
    ("overrightarrow", "→", true),
    ("dot", "˙", false),
    ("ddot", "¨", false),
    ("tilde", "˜", false),
    ("widetilde", "˜", true),
    ("check", "ˇ", false),
    ("breve", "˘", false),
    ("acute", "´", false),
    ("grave", "`", false),
    ("overbrace", "⏞", true),
];

const SPACES: &[(&str, &str)] = &[
    (",", "0.1667em"),
    (":", "0.2222em"),
    (">", "0.2222em"),
    (";", "0.2778em"),
    ("quad", "1em"),
    ("qquad", "2em"),
    (" ", "0.25em"),
    ("enspace", "0.5em"),
    ("thinspace", "0.1667em"),
];

#[derive(Clone, Copy, PartialEq)]
enum Font {
    Bold,
    Italic,
    BoldItalic,
    Script,
    Fraktur,
    Double,
    Sans,
    Mono,
}

/// A letter or digit in one of the math alphabets (Unicode's Mathematical Alphanumeric Symbols,
/// with the letters that live elsewhere), since browsers draw those directly.
fn styled(ch: char, f: Font) -> char {
    let holes: &[(char, char)] = match f {
        Font::Italic => &[('h', 'ℎ')],
        Font::Script => &[
            ('B', 'ℬ'),
            ('E', 'ℰ'),
            ('F', 'ℱ'),
            ('H', 'ℋ'),
            ('I', 'ℐ'),
            ('L', 'ℒ'),
            ('M', 'ℳ'),
            ('R', 'ℛ'),
            ('e', 'ℯ'),
            ('g', 'ℊ'),
            ('o', 'ℴ'),
        ],
        Font::Fraktur => &[('C', 'ℭ'), ('H', 'ℌ'), ('I', 'ℑ'), ('R', 'ℜ'), ('Z', 'ℨ')],
        Font::Double => &[
            ('C', 'ℂ'),
            ('H', 'ℍ'),
            ('N', 'ℕ'),
            ('P', 'ℙ'),
            ('Q', 'ℚ'),
            ('R', 'ℝ'),
            ('Z', 'ℤ'),
        ],
        _ => &[],
    };
    if let Some((_, h)) = holes.iter().find(|(c, _)| *c == ch) {
        return *h;
    }
    let (upper, lower, digit): (u32, u32, Option<u32>) = match f {
        Font::Bold => (0x1D400, 0x1D41A, Some(0x1D7CE)),
        Font::Italic => (0x1D434, 0x1D44E, None),
        Font::BoldItalic => (0x1D468, 0x1D482, Some(0x1D7CE)),
        Font::Script => (0x1D49C, 0x1D4B6, None),
        Font::Fraktur => (0x1D504, 0x1D51E, None),
        Font::Double => (0x1D538, 0x1D552, Some(0x1D7D8)),
        Font::Sans => (0x1D5A0, 0x1D5BA, Some(0x1D7E2)),
        Font::Mono => (0x1D670, 0x1D68A, Some(0x1D7F6)),
    };
    let code = match ch {
        'A'..='Z' => upper + (ch as u32 - 'A' as u32),
        'a'..='z' => lower + (ch as u32 - 'a' as u32),
        '0'..='9' => match digit {
            Some(d) => d + (ch as u32 - '0' as u32),
            None => return ch,
        },
        _ => return ch,
    };
    char::from_u32(code).unwrap_or(ch)
}

struct P {
    t: Vec<Tok>,
    i: usize,
    display: bool,
    depth: usize,
    font: Option<Font>,
}

const MAX_DEPTH: usize = 48;
const MAX_TOKENS: usize = 6_000;

fn mo(s: &str) -> String {
    format!("<mo>{}</mo>", esc(s))
}

fn group(items: Vec<String>) -> String {
    if items.len() == 1 {
        items.into_iter().next().unwrap_or_default()
    } else {
        format!("<mrow>{}</mrow>", items.concat())
    }
}

fn unknown(name: &str) -> String {
    format!("<merror><mtext>\\{}</mtext></merror>", esc(name))
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }

    /// One argument: a `{group}` or a single token.
    fn arg(&mut self) -> String {
        match self.peek() {
            Some(Tok::Open) => {
                self.i += 1;
                let inner = self.list(&[]);
                if self.peek() == Some(&Tok::Close) {
                    self.i += 1;
                }
                if inner.is_empty() {
                    "<mrow></mrow>".into()
                } else {
                    group(inner)
                }
            }
            Some(_) => self
                .atom()
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| "<mrow></mrow>".into()),
            None => "<mrow></mrow>".into(),
        }
    }

    /// The raw text of a `{...}` argument (for `\text`, `\begin`, `\operatorname`).
    fn raw_arg(&mut self) -> String {
        let mut s = String::new();
        if let Some(Tok::Text(t)) = self.peek() {
            let t = t.clone();
            self.i += 1;
            return t;
        }
        if self.peek() != Some(&Tok::Open) {
            return s;
        }
        self.i += 1;
        let mut depth = 1;
        while let Some(t) = self.t.get(self.i).cloned() {
            self.i += 1;
            match t {
                Tok::Open => {
                    depth += 1;
                    s.push('{');
                }
                Tok::Close => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    s.push('}');
                }
                Tok::Ch(c) => s.push(c),
                Tok::Cmd(c) if c == " " || c == "," || c == ";" => s.push(' '),
                Tok::Cmd(c) => {
                    s.push('\\');
                    s.push_str(&c);
                }
                Tok::Sup => s.push('^'),
                Tok::Sub => s.push('_'),
                Tok::Amp => s.push('&'),
                Tok::Text(t) => s.push_str(&t),
            }
        }
        s
    }

    /// Optional `[...]` (the index of `\sqrt[3]{x}`).
    fn opt_arg(&mut self) -> Option<String> {
        if self.peek() != Some(&Tok::Ch('[')) {
            return None;
        }
        self.i += 1;
        let start = self.i;
        let mut depth = 0;
        while let Some(t) = self.peek() {
            match t {
                Tok::Ch(']') if depth == 0 => break,
                Tok::Open => depth += 1,
                Tok::Close => depth -= 1,
                _ => {}
            }
            self.i += 1;
        }
        let toks = self.t[start..self.i].to_vec();
        self.i += 1;
        let mut sub = P {
            t: toks,
            i: 0,
            display: false,
            depth: self.depth + 1,
            font: self.font,
        };
        Some(group(sub.list(&[])))
    }

    /// Items until `}` or one of the stop commands (`\right`, `\end`, `\\`), which is left in place.
    fn list(&mut self, stops: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        if self.depth > MAX_DEPTH {
            self.i = self.t.len();
            return vec![unknown("too deep")];
        }
        self.depth += 1;
        while let Some(t) = self.peek() {
            match t {
                Tok::Close => break,
                Tok::Cmd(c) if stops.contains(&c.as_str()) => break,
                Tok::Amp if stops.contains(&"&") => break,
                _ => {}
            }
            let Some(base) = self.atom() else { continue };
            out.push(self.scripts(base));
        }
        self.depth -= 1;
        out
    }

    /// Sub- and superscripts after `base`; under and over a big operator in display math.
    fn scripts(&mut self, base: String) -> String {
        let (mut sub, mut sup) = (None, None);
        loop {
            match self.peek() {
                Some(Tok::Sub) if sub.is_none() => {
                    self.i += 1;
                    sub = Some(self.arg());
                }
                Some(Tok::Sup) if sup.is_none() => {
                    self.i += 1;
                    sup = Some(self.arg());
                }
                Some(Tok::Ch('\'')) => {
                    self.i += 1;
                    sup = Some(match sup {
                        Some(s) => format!("<mrow>{s}<mo>′</mo></mrow>"),
                        None => "<mo>′</mo>".into(),
                    });
                }
                _ => break,
            }
        }
        let limits =
            self.display && (base.contains("data-limits") || base.contains("movablelimits"));
        match (sub, sup) {
            (None, None) => base,
            (Some(b), None) if limits => format!("<munder>{base}{b}</munder>"),
            (None, Some(p)) if limits => format!("<mover>{base}{p}</mover>"),
            (Some(b), Some(p)) if limits => format!("<munderover>{base}{b}{p}</munderover>"),
            (Some(b), None) => format!("<msub>{base}{b}</msub>"),
            (None, Some(p)) => format!("<msup>{base}{p}</msup>"),
            (Some(b), Some(p)) => format!("<msubsup>{base}{b}{p}</msubsup>"),
        }
    }

    fn ident(&self, ch: char) -> String {
        match self.font {
            Some(f) => format!(
                "<mi mathvariant=\"normal\">{}</mi>",
                esc(&styled(ch, f).to_string())
            ),
            None => format!("<mi>{}</mi>", esc(&ch.to_string())),
        }
    }

    fn atom(&mut self) -> Option<String> {
        let t = self.t.get(self.i).cloned()?;
        self.i += 1;
        Some(match t {
            Tok::Open => {
                let inner = self.list(&[]);
                if self.peek() == Some(&Tok::Close) {
                    self.i += 1;
                }
                format!("<mrow>{}</mrow>", inner.concat())
            }
            Tok::Close => return None,
            Tok::Sup | Tok::Sub => "<mi></mi>".into(), // a script with nothing before it
            Tok::Amp => String::new(),
            Tok::Ch(c) if c.is_ascii_digit() || c == '.' => {
                let mut n = c.to_string();
                while let Some(Tok::Ch(d)) = self.peek() {
                    if d.is_ascii_digit() || (*d == '.' && n.chars().all(|x| x != '.')) {
                        n.push(*d);
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                if n == "." {
                    return Some(mo("."));
                }
                match self.font {
                    Some(f) => format!(
                        "<mn>{}</mn>",
                        esc(&n.chars().map(|x| styled(x, f)).collect::<String>())
                    ),
                    None => format!("<mn>{}</mn>", esc(&n)),
                }
            }
            Tok::Ch(c) if c.is_alphabetic() => self.ident(c),
            Tok::Ch('-') => mo("−"),
            Tok::Ch('*') => mo("∗"),
            Tok::Ch('\'') => mo("′"),
            Tok::Ch('~') => "<mspace width=\"0.2778em\"/>".into(),
            Tok::Ch(c) => mo(&c.to_string()),
            Tok::Text(t) => format!("<mtext>{}</mtext>", esc(&t)),
            Tok::Cmd(name) => self.command(&name),
        })
    }

    fn command(&mut self, name: &str) -> String {
        if let Some((_, ch)) = GREEK.iter().find(|(n, _)| *n == name) {
            return if ch.is_uppercase() {
                format!("<mi mathvariant=\"normal\">{ch}</mi>")
            } else {
                format!("<mi>{ch}</mi>")
            };
        }
        if let Some((_, s)) = IDENT.iter().find(|(n, _)| *n == name) {
            return format!("<mi mathvariant=\"normal\">{}</mi>", esc(s));
        }
        if let Some((_, s)) = OPS.iter().find(|(n, _)| *n == name) {
            return mo(s);
        }
        if let Some((_, s)) = BIG.iter().find(|(n, _)| *n == name) {
            let limits = !name.contains("int");
            return format!(
                "<mo largeop=\"true\"{}>{s}</mo>",
                if limits {
                    " movablelimits=\"true\""
                } else {
                    ""
                }
            );
        }
        if FUNCS.contains(&name) {
            return format!("<mi>{name}</mi><mo>\u{2061}</mo>");
        }
        if LIMFUNCS.contains(&name) {
            let shown = match name {
                "limsup" => "lim sup",
                "liminf" => "lim inf",
                "argmax" => "arg max",
                "argmin" => "arg min",
                n => n,
            };
            return format!("<mi data-limits=\"1\">{shown}</mi>");
        }
        if let Some((_, w)) = SPACES.iter().find(|(n, _)| *n == name) {
            return format!("<mspace width=\"{w}\"/>");
        }
        if let Some((_, a, wide)) = ACCENTS.iter().find(|(n, _, _)| *n == name) {
            let base = self.arg();
            return format!(
                "<mover accent=\"true\">{base}<mo stretchy=\"{wide}\">{}</mo></mover>",
                esc(a)
            );
        }
        let font = match name {
            "mathbf" | "bf" => Some(Font::Bold),
            "mathit" => Some(Font::Italic),
            "boldsymbol" | "bm" => Some(Font::BoldItalic),
            "mathcal" | "mathscr" => Some(Font::Script),
            "mathfrak" => Some(Font::Fraktur),
            "mathbb" => Some(Font::Double),
            "mathsf" => Some(Font::Sans),
            "mathtt" => Some(Font::Mono),
            _ => None,
        };
        if let Some(f) = font {
            let keep = self.font.replace(f);
            let a = self.arg();
            self.font = keep;
            return a;
        }
        match name {
            "frac" | "dfrac" | "tfrac" | "cfrac" => {
                let (a, b) = (self.arg(), self.arg());
                format!("<mfrac>{a}{b}</mfrac>")
            }
            "binom" | "dbinom" | "tbinom" => {
                let (a, b) = (self.arg(), self.arg());
                format!(
                    "<mrow><mo>(</mo><mfrac linethickness=\"0\">{a}{b}</mfrac><mo>)</mo></mrow>"
                )
            }
            "sqrt" => match self.opt_arg() {
                Some(n) => {
                    let x = self.arg();
                    format!("<mroot>{x}{n}</mroot>")
                }
                None => format!("<msqrt>{}</msqrt>", self.arg()),
            },
            "underline" => format!(
                "<munder accentunder=\"true\">{}<mo stretchy=\"true\">_</mo></munder>",
                self.arg()
            ),
            "underbrace" => format!(
                "<munder accentunder=\"true\">{}<mo stretchy=\"true\">⏟</mo></munder>",
                self.arg()
            ),
            "overset" | "stackrel" => {
                let (top, base) = (self.arg(), self.arg());
                format!("<mover>{base}{top}</mover>")
            }
            "underset" => {
                let (bottom, base) = (self.arg(), self.arg());
                format!("<munder>{base}{bottom}</munder>")
            }
            "text" | "textrm" | "textnormal" | "mbox" | "textit" | "textbf" | "textsf"
            | "texttt" => {
                format!("<mtext>{}</mtext>", esc(&self.raw_arg()))
            }
            "mathrm" | "rm" | "operatorname" => {
                let raw = self.raw_arg();
                let raw = raw.trim();
                if name == "operatorname" || raw.chars().count() > 1 {
                    format!("<mi>{}</mi>", esc(raw))
                } else {
                    format!("<mi mathvariant=\"normal\">{}</mi>", esc(raw))
                }
            }
            "left" | "right" | "bigl" | "bigr" | "Bigl" | "Bigr" | "biggl" | "biggr" | "big"
            | "Big" | "bigg" | "Bigg" | "middle" => {
                let fence = self.fence();
                if name == "left" {
                    let inner = self.list(&["right"]);
                    let close = if self.peek() == Some(&Tok::Cmd("right".into())) {
                        self.i += 1;
                        self.fence()
                    } else {
                        String::new()
                    };
                    let f = |s: &str| {
                        if s.is_empty() {
                            String::new()
                        } else {
                            format!("<mo stretchy=\"true\" fence=\"true\">{}</mo>", esc(s))
                        }
                    };
                    return format!("<mrow>{}{}{}</mrow>", f(&fence), inner.concat(), f(&close));
                }
                if name == "right" {
                    return String::new(); // a stray \right
                }
                let size = match name.trim_end_matches(['l', 'r']) {
                    "big" => "1.2em",
                    "Big" => "1.623em",
                    "bigg" => "2.047em",
                    _ => "2.470em",
                };
                if fence.is_empty() {
                    String::new()
                } else {
                    format!(
                        "<mo minsize=\"{size}\" maxsize=\"{size}\">{}</mo>",
                        esc(&fence)
                    )
                }
            }
            "begin" => self.environment(),
            "\\" | "newline" | "cr" => "<mspace linebreak=\"newline\"/>".into(),
            "displaystyle" | "textstyle" | "scriptstyle" | "limits" | "nolimits" | "!"
            | "label" | "nonumber" | "tag" => {
                if name == "label" || name == "tag" {
                    self.raw_arg();
                }
                String::new()
            }
            other => unknown(other),
        }
    }

    /// The fence after `\left`, `\right` and `\big`: a character, `.` for none, or a command.
    fn fence(&mut self) -> String {
        match self.t.get(self.i).cloned() {
            Some(Tok::Ch('.')) => {
                self.i += 1;
                String::new()
            }
            Some(Tok::Ch(c)) => {
                self.i += 1;
                c.to_string()
            }
            Some(Tok::Cmd(c)) => {
                self.i += 1;
                OPS.iter()
                    .find(|(n, _)| *n == c)
                    .map(|(_, s)| s.to_string())
                    .unwrap_or_default()
            }
            _ => String::new(),
        }
    }

    /// `\begin{matrix} a & b \\ c & d \end{matrix}` and its kin, as a table.
    fn environment(&mut self) -> String {
        let env = self.raw_arg();
        let env = env.trim().trim_end_matches('*').to_string();
        if env == "array" {
            self.raw_arg(); // the column spec
        }
        let mut rows: Vec<Vec<String>> = vec![vec![]];
        loop {
            let cell = self.list(&["&", "\\\\", "\\", "end"]);
            if let Some(r) = rows.last_mut() {
                r.push(group(cell));
            }
            match self.t.get(self.i).cloned() {
                Some(Tok::Amp) => self.i += 1,
                Some(Tok::Cmd(c)) if c == "\\" => {
                    self.i += 1;
                    rows.push(vec![]);
                }
                Some(Tok::Cmd(c)) if c == "end" => {
                    self.i += 1;
                    self.raw_arg();
                    break;
                }
                Some(Tok::Close) => {
                    self.i += 1;
                }
                None => break,
                _ => self.i += 1,
            }
        }
        if rows.last().is_some_and(|r| r.iter().all(String::is_empty)) && rows.len() > 1 {
            rows.pop();
        }
        let align = match env.as_str() {
            "aligned" | "align" | "split" | "alignat" | "eqnarray" => {
                " columnalign=\"right left right left\""
            }
            "cases" | "rcases" => " columnalign=\"left left\"",
            _ => "",
        };
        let table = format!(
            "<mtable{align}>{}</mtable>",
            rows.iter()
                .map(|r| format!(
                    "<mtr>{}</mtr>",
                    r.iter()
                        .map(|c| format!("<mtd>{c}</mtd>"))
                        .collect::<String>()
                ))
                .collect::<String>()
        );
        let (l, r) = match env.as_str() {
            "pmatrix" => ("(", ")"),
            "bmatrix" => ("[", "]"),
            "Bmatrix" => ("{", "}"),
            "vmatrix" => ("|", "|"),
            "Vmatrix" => ("‖", "‖"),
            "cases" => ("{", ""),
            "rcases" => ("", "}"),
            _ => ("", ""),
        };
        if l.is_empty() && r.is_empty() {
            return table;
        }
        let f = |s: &str| {
            if s.is_empty() {
                String::new()
            } else {
                format!("<mo stretchy=\"true\" fence=\"true\">{s}</mo>")
            }
        };
        format!("<mrow>{}{table}{}</mrow>", f(l), f(r))
    }
}

/// TeX to a `<math>` element. `display` is for `$$...$$` blocks.
pub fn mathml(tex: &str, display: bool) -> String {
    let toks = lex(tex);
    let cls = if display { "math math-block" } else { "math" };
    let attr = if display { " display=\"block\"" } else { "" };
    if toks.len() > MAX_TOKENS {
        return format!("<code class=\"{cls}\">{}</code>", esc(tex));
    }
    let mut p = P {
        t: toks,
        i: 0,
        display,
        depth: 0,
        font: None,
    };
    let mut items = Vec::new();
    let mut lines: Vec<Vec<String>> = vec![];
    while p.i < p.t.len() {
        items.extend(p.list(&["\\\\", "\\"]));
        match p.t.get(p.i).cloned() {
            Some(Tok::Cmd(c)) if c == "\\" => {
                p.i += 1;
                lines.push(std::mem::take(&mut items));
            }
            Some(Tok::Close) => p.i += 1, // an extra }
            _ => {}
        }
    }
    let body = if lines.is_empty() {
        format!("<mrow>{}</mrow>", items.concat())
    } else {
        // several lines in a block: one centred row each
        lines.push(items);
        format!(
            "<mtable>{}</mtable>",
            lines
                .iter()
                .map(|l| format!("<mtr><mtd>{}</mtd></mtr>", l.concat()))
                .collect::<String>()
        )
    };
    format!(
        "<math class=\"{cls}\"{attr}><semantics>{body}<annotation encoding=\"application/x-tex\">{}</annotation></semantics></math>",
        esc(tex.trim())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inner(tex: &str, display: bool) -> String {
        let m = mathml(tex, display);
        let a = m.find("<semantics>").unwrap() + 11;
        let b = m.find("<annotation").unwrap();
        m[a..b].to_string()
    }

    #[test]
    fn basics() {
        assert_eq!(
            inner("x^2", false),
            "<mrow><msup><mi>x</mi><mn>2</mn></msup></mrow>"
        );
        assert_eq!(
            inner("a_{ij}", false),
            "<mrow><msub><mi>a</mi><mrow><mi>i</mi><mi>j</mi></mrow></msub></mrow>"
        );
        assert_eq!(
            inner("x_1^{2}", false),
            "<mrow><msubsup><mi>x</mi><mn>1</mn><mn>2</mn></msubsup></mrow>"
        );
        assert_eq!(
            inner("3.14 - b", false),
            "<mrow><mn>3.14</mn><mo>−</mo><mi>b</mi></mrow>"
        );
        assert_eq!(
            inner("\\frac{a}{b}", false),
            "<mrow><mfrac><mi>a</mi><mi>b</mi></mfrac></mrow>"
        );
        assert_eq!(
            inner("\\sqrt{x}", false),
            "<mrow><msqrt><mi>x</mi></msqrt></mrow>"
        );
        assert_eq!(
            inner("\\sqrt[3]{x}", false),
            "<mrow><mroot><mi>x</mi><mn>3</mn></mroot></mrow>"
        );
        assert_eq!(
            inner("\\alpha + \\Omega", false),
            "<mrow><mi>α</mi><mo>+</mo><mi mathvariant=\"normal\">Ω</mi></mrow>"
        );
        assert_eq!(
            inner("f'(x)", false),
            "<mrow><msup><mi>f</mi><mo>′</mo></msup><mo>(</mo><mi>x</mi><mo>)</mo></mrow>"
        );
        assert!(mathml("E=mc^2", true)
            .starts_with("<math class=\"math math-block\" display=\"block\"><semantics>"));
        assert!(mathml("a<b", false).contains("<mo>&lt;</mo>") && mathml("a<b", false).ends_with("<annotation encoding=\"application/x-tex\">a&lt;b</annotation></semantics></math>"));
    }

    #[test]
    fn operators_limits_and_fonts() {
        assert_eq!(
            inner("\\sum_{i=1}^n i", true),
            "<mrow><munderover><mo largeop=\"true\" movablelimits=\"true\">∑</mo><mrow><mi>i</mi><mo>=</mo><mn>1</mn></mrow><mi>n</mi></munderover><mi>i</mi></mrow>"
        );
        assert!(
            inner("\\sum_{i=1}^n i", false).contains("<msubsup><mo largeop"),
            "inline sums keep scripts beside"
        );
        assert!(
            inner("\\int_0^1 f", true).contains("<msubsup><mo largeop=\"true\">∫</mo>"),
            "integrals keep limits beside"
        );
        assert!(inner("\\lim_{x \\to 0} f", true)
            .starts_with("<mrow><munder><mi data-limits=\"1\">lim</mi>"));
        assert_eq!(
            inner("\\sin x", false),
            "<mrow><mi>sin</mi><mo>\u{2061}</mo><mi>x</mi></mrow>"
        );
        assert_eq!(
            inner("\\mathbb{R}^n", false),
            "<mrow><msup><mi mathvariant=\"normal\">ℝ</mi><mi>n</mi></msup></mrow>"
        );
        assert_eq!(
            inner("\\mathcal{L}", false),
            "<mrow><mi mathvariant=\"normal\">ℒ</mi></mrow>"
        );
        assert_eq!(
            inner("\\mathbf{v}", false),
            "<mrow><mi mathvariant=\"normal\">𝐯</mi></mrow>"
        );
        assert_eq!(
            inner("\\text{if } x", false),
            "<mrow><mtext>if </mtext><mi>x</mi></mrow>"
        );
        assert_eq!(
            inner("\\hat{x}", false),
            "<mrow><mover accent=\"true\"><mi>x</mi><mo stretchy=\"false\">^</mo></mover></mrow>"
        );
        assert_eq!(
            inner("a \\leq b \\to c", false),
            "<mrow><mi>a</mi><mo>≤</mo><mi>b</mi><mo>→</mo><mi>c</mi></mrow>"
        );
        assert_eq!(inner("\\left( \\frac{1}{2} \\right)", false), "<mrow><mrow><mo stretchy=\"true\" fence=\"true\">(</mo><mfrac><mn>1</mn><mn>2</mn></mfrac><mo stretchy=\"true\" fence=\"true\">)</mo></mrow></mrow>");
        assert!(inner("a\\,b\\quad c", false).contains("<mspace width=\"0.1667em\"/>"));
    }

    #[test]
    fn tables_and_lines() {
        let m = inner("\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}", true);
        assert_eq!(m, "<mrow><mrow><mo stretchy=\"true\" fence=\"true\">(</mo><mtable><mtr><mtd><mi>a</mi></mtd><mtd><mi>b</mi></mtd></mtr><mtr><mtd><mi>c</mi></mtd><mtd><mi>d</mi></mtd></mtr></mtable><mo stretchy=\"true\" fence=\"true\">)</mo></mrow></mrow>");
        let c = inner(
            "|x| = \\begin{cases} x & x \\ge 0 \\\\ -x & \\text{else} \\end{cases}",
            true,
        );
        assert!(
            c.contains("<mtable columnalign=\"left left\">") && c.contains("<mtext>else</mtext>"),
            "{c}"
        );
        let a = inner("\\begin{aligned} a &= b \\\\ &= c \\end{aligned}", true);
        assert!(
            a.contains("columnalign=\"right left right left\"") && a.matches("<mtr>").count() == 2,
            "{a}"
        );
        let lines = inner("a = b \\\\ c = d", true);
        assert!(
            lines.starts_with("<mtable><mtr><mtd><mi>a</mi>")
                && lines.matches("<mtr>").count() == 2,
            "{lines}"
        );
    }

    #[test]
    fn unknown_and_hostile_input_stays_safe() {
        assert!(
            inner("\\frobnicate x", false).contains("<merror><mtext>\\frobnicate</mtext></merror>")
        );
        let x = mathml("\\text{<script>alert(1)</script>}", false);
        assert!(!x.contains("<script>") && x.contains("&lt;script&gt;"));
        let deep = "{".repeat(500) + &"}".repeat(500);
        let _ = mathml(&deep, true); // no stack overflow
        let big = "x+".repeat(5000);
        assert!(mathml(&big, false).starts_with("<code class=\"math\">"));
        let _ = mathml("\\frac{", false);
        let _ = mathml("}}}x^", false);
        let _ = mathml("\\begin{matrix} a & b", true);
        let _ = mathml("\\left( x", false);
        let _ = mathml("\\sqrt[", false);
    }
}
