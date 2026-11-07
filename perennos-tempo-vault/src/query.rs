//! Views over notes, as Obsidian does them: Bases (`.base` files and ```` ```base ```` blocks) and
//! Dataview-style ```` ```dataview ```` queries (`LIST`, `TABLE`, `TASK` with `FROM`, `WHERE`,
//! `SORT`, `GROUP BY`, `LIMIT`), on one small expression language:
//!
//! - values: text, numbers, `true`/`false`, `null`, lists `[a, b]`
//! - a note's properties by name (`status`, `note.status`, `note["due date"]`), `formula.x`,
//!   and the file: `file.name`, `file.path`, `file.folder`, `file.ext`, `file.size`,
//!   `file.mtime`, `file.tags`, `file.links`, `file.backlinks`, `file.words`, `file.tasks`
//! - `file.hasTag("a", "b")`, `file.inFolder("x")`, `file.hasLink("Note")`, `file.hasProperty("p")`
//! - `== != < <= > >=` (and Dataview's `=`), `&& || !` (and `and or not`), `+ - * / %`
//! - functions: `if(c, a, b)`, `today()`, `now()`, `date(x)`, `number(x)`, `string(x)`,
//!   `list(x)`, `length(x)`, `contains(a, b)`, `lower(x)`, `upper(x)`, `default(x, y)`,
//!   `min(..)`, `max(..)`; methods `.contains() .containsAll() .containsAny() .startsWith()
//!   .endsWith() .lower() .upper() .trim() .isEmpty() .toFixed(n) .round() .join(sep)
//!   .split(sep) .length`
//!
//! Dates are ISO text (`2026-11-06`, `2026-11-06 14:05`), so they sort and compare as written.
use crate::parse::{Prop, Task};
use crate::vault::{Note, Vault};
use crate::yaml::{self, Y};
use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    List(Vec<Val>),
}

impl Val {
    pub fn truthy(&self) -> bool {
        match self {
            Val::Null => false,
            Val::Bool(b) => *b,
            Val::Num(n) => *n != 0.0 && !n.is_nan(),
            Val::Str(s) => !s.is_empty(),
            Val::List(l) => !l.is_empty(),
        }
    }

    /// How the value reads in a cell.
    pub fn text(&self) -> String {
        match self {
            Val::Null => String::new(),
            Val::Bool(b) => b.to_string(),
            Val::Num(n) => num_text(*n),
            Val::Str(s) => s.clone(),
            Val::List(l) => l.iter().map(Val::text).collect::<Vec<_>>().join(", "),
        }
    }

    fn num(&self) -> Option<f64> {
        match self {
            Val::Num(n) => Some(*n),
            Val::Bool(b) => Some(f64::from(u8::from(*b))),
            Val::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    fn items(&self) -> Vec<Val> {
        match self {
            Val::List(l) => l.clone(),
            Val::Null => vec![],
            v => vec![v.clone()],
        }
    }
}

fn num_text(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// A property value as the expression language sees it.
pub fn prop_val(p: &Prop) -> Val {
    fn one(t: &str) -> Val {
        let t = t.trim();
        if t.is_empty() {
            return Val::Null;
        }
        if t.eq_ignore_ascii_case("true") {
            return Val::Bool(true);
        }
        if t.eq_ignore_ascii_case("false") {
            return Val::Bool(false);
        }
        let numeric = t
            .trim_start_matches(['-', '+'])
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.')
            && t.chars().any(|c| c.is_ascii_digit())
            && t.matches('.').count() <= 1;
        if numeric {
            if let Ok(n) = t.parse::<f64>() {
                return Val::Num(n);
            }
        }
        Val::Str(t.to_string())
    }
    match p {
        Prop::Text(t) => one(t),
        Prop::List(l) => Val::List(l.iter().map(|x| one(x)).collect()),
    }
}

/// Equality as people mean it: numbers by value, text exactly, lists item by item.
fn equal(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Null, Val::Null) => true,
        (Val::Null, _) | (_, Val::Null) => false,
        (Val::List(x), Val::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| equal(p, q))
        }
        (Val::Str(x), Val::Str(y)) => x == y,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        _ => match (a.num(), b.num()) {
            (Some(x), Some(y)) => x == y,
            _ => a.text() == b.text(),
        },
    }
}

/// Order for sorting and `<`: numbers by value, everything else as text; nothing sorts last.
fn order(a: &Val, b: &Val) -> Option<Ordering> {
    match (a, b) {
        (Val::Null, Val::Null) => Some(Ordering::Equal),
        (Val::Null, _) => Some(Ordering::Greater),
        (_, Val::Null) => Some(Ordering::Less),
        (Val::Str(x), Val::Str(y)) => Some(x.to_lowercase().cmp(&y.to_lowercase())),
        _ => match (a.num(), b.num()) {
            (Some(x), Some(y)) => x.partial_cmp(&y),
            _ => Some(a.text().to_lowercase().cmp(&b.text().to_lowercase())),
        },
    }
}

/// `contains` for a list of tags: `#book` and `book` are the same tag.
fn same_item(a: &Val, b: &Val) -> bool {
    if equal(a, b) {
        return true;
    }
    match (a, b) {
        (Val::Str(x), Val::Str(y)) if x.starts_with('#') || y.starts_with('#') => x
            .trim_start_matches('#')
            .eq_ignore_ascii_case(y.trim_start_matches('#')),
        _ => false,
    }
}

// ---- the language

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Id(String),
    Op(&'static str),
}

const OPS: [&str; 22] = [
    "&&", "||", "==", "!=", "<=", ">=", "<", ">", "!", "=", "+", "-", "*", "/", "%", "(", ")", "[",
    "]", ",", ".", "?",
];

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let cs: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && cs.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                i += 1;
            }
            let t: String = cs[start..i].iter().collect();
            out.push(Tok::Num(
                t.parse().map_err(|_| format!("{t} is not a number"))?,
            ));
            continue;
        }
        if c == '"' || c == '\'' {
            let mut s = String::new();
            i += 1;
            loop {
                match cs.get(i) {
                    None => return Err("a text in quotes is not closed".into()),
                    Some(&q) if q == c => break,
                    Some('\\') => {
                        i += 1;
                        match cs.get(i) {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(&o) => s.push(o),
                            None => return Err("a text in quotes is not closed".into()),
                        }
                    }
                    Some(&o) => s.push(o),
                }
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(s));
            continue;
        }
        if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_' || cs[i] == '$') {
                i += 1;
            }
            out.push(Tok::Id(cs[start..i].iter().collect()));
            continue;
        }
        let rest: String = cs[i..cs.len().min(i + 2)].iter().collect();
        match OPS.iter().find(|op| rest.starts_with(**op)) {
            Some(op) => {
                out.push(Tok::Op(op));
                i += op.chars().count();
            }
            None => return Err(format!("\"{c}\" is not understood here")),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Val),
    Id(String),
    Member(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    Bin(&'static str, Box<Expr>, Box<Expr>),
    List(Vec<Expr>),
}

impl Expr {
    /// The note property this reads as it is (`status`, `note.status`, `note["due date"]`), if any.
    pub fn prop_name(&self) -> Option<&str> {
        match self {
            Expr::Id(w)
                if !matches!(
                    w.as_str(),
                    "file" | "note" | "formula" | "this" | "today" | "now"
                ) =>
            {
                Some(w)
            }
            Expr::Member(b, k) if matches!(&**b, Expr::Id(n) if n == "note") => Some(k),
            Expr::Index(b, k) if matches!(&**b, Expr::Id(n) if n == "note") => match &**k {
                Expr::Lit(Val::Str(s)) => Some(s),
                _ => None,
            },
            _ => None,
        }
    }
}

struct P {
    t: Vec<Tok>,
    i: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(o)) if *o == op)
    }
    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Id(x)) if x.eq_ignore_ascii_case(w))
    }
    fn eat(&mut self, op: &str) -> Result<(), String> {
        if self.is_op(op) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected \"{op}\""))
        }
    }
    fn or(&mut self) -> Result<Expr, String> {
        let mut l = self.and()?;
        while self.is_op("||") || self.is_word("or") {
            self.i += 1;
            l = Expr::Bin("||", Box::new(l), Box::new(self.and()?));
        }
        Ok(l)
    }
    fn and(&mut self) -> Result<Expr, String> {
        let mut l = self.not()?;
        while self.is_op("&&") || self.is_word("and") {
            self.i += 1;
            l = Expr::Bin("&&", Box::new(l), Box::new(self.not()?));
        }
        Ok(l)
    }
    fn not(&mut self) -> Result<Expr, String> {
        if self.is_op("!") || self.is_word("not") {
            self.i += 1;
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.cmp()
    }
    fn cmp(&mut self) -> Result<Expr, String> {
        let l = self.add()?;
        for op in ["==", "!=", "<=", ">=", "<", ">", "="] {
            if self.is_op(op) {
                self.i += 1;
                let op = if op == "=" { "==" } else { op };
                return Ok(Expr::Bin(op, Box::new(l), Box::new(self.add()?)));
            }
        }
        Ok(l)
    }
    fn add(&mut self) -> Result<Expr, String> {
        let mut l = self.mul()?;
        loop {
            let op = if self.is_op("+") {
                "+"
            } else if self.is_op("-") {
                "-"
            } else {
                return Ok(l);
            };
            self.i += 1;
            l = Expr::Bin(op, Box::new(l), Box::new(self.mul()?));
        }
    }
    fn mul(&mut self) -> Result<Expr, String> {
        let mut l = self.unary()?;
        loop {
            let op = if self.is_op("*") {
                "*"
            } else if self.is_op("/") {
                "/"
            } else if self.is_op("%") {
                "%"
            } else {
                return Ok(l);
            };
            self.i += 1;
            l = Expr::Bin(op, Box::new(l), Box::new(self.unary()?));
        }
    }
    fn unary(&mut self) -> Result<Expr, String> {
        if self.is_op("-") {
            self.i += 1;
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.postfix()
    }
    fn args(&mut self, close: &str) -> Result<Vec<Expr>, String> {
        let mut a = Vec::new();
        if self.is_op(close) {
            self.i += 1;
            return Ok(a);
        }
        loop {
            a.push(self.or()?);
            if self.is_op(",") {
                self.i += 1;
                continue;
            }
            self.eat(close)?;
            return Ok(a);
        }
    }
    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        loop {
            if self.is_op(".") {
                self.i += 1;
                match self.t.get(self.i).cloned() {
                    Some(Tok::Id(name)) => {
                        self.i += 1;
                        e = Expr::Member(Box::new(e), name);
                    }
                    _ => return Err("a name must follow \".\"".into()),
                }
            } else if self.is_op("(") {
                self.i += 1;
                e = Expr::Call(Box::new(e), self.args(")")?);
            } else if self.is_op("[") {
                self.i += 1;
                let idx = self.or()?;
                self.eat("]")?;
                e = Expr::Index(Box::new(e), Box::new(idx));
            } else {
                return Ok(e);
            }
        }
    }
    fn primary(&mut self) -> Result<Expr, String> {
        let t = self
            .t
            .get(self.i)
            .cloned()
            .ok_or("the expression ends too early")?;
        self.i += 1;
        Ok(match t {
            Tok::Num(n) => Expr::Lit(Val::Num(n)),
            Tok::Str(s) => Expr::Lit(Val::Str(s)),
            Tok::Id(w) if w == "true" => Expr::Lit(Val::Bool(true)),
            Tok::Id(w) if w == "false" => Expr::Lit(Val::Bool(false)),
            Tok::Id(w) if w == "null" => Expr::Lit(Val::Null),
            Tok::Id(w) => Expr::Id(w),
            Tok::Op("(") => {
                let e = self.or()?;
                self.eat(")")?;
                e
            }
            Tok::Op("[") => Expr::List(self.args("]")?),
            Tok::Op(o) => return Err(format!("\"{o}\" is not expected here")),
        })
    }
}

pub fn parse_expr(src: &str) -> Result<Expr, String> {
    let mut p = P { t: lex(src)?, i: 0 };
    if p.t.is_empty() {
        return Err("the expression is empty".into());
    }
    let e = p.or()?;
    if p.i < p.t.len() {
        return Err(format!("unexpected {:?} after the expression", p.t[p.i]));
    }
    Ok(e)
}

// ---- evaluating

pub struct Env<'a> {
    pub v: &'a Vault,
    pub note: &'a Note,
    pub formulas: &'a [(String, Expr)],
    pub task: Option<&'a Task>,
    pub today: &'a str,
    pub now: &'a str,
    depth: usize,
}

fn local_time(unix: f64) -> String {
    let off = crate::util::local_offset_minutes(None);
    let secs = unix as i64 + off * 60;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = crate::util::civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// Days since 1970 of a value that starts with an ISO date (`2026-11-06`, `2026-11-06 14:05`).
fn day_number(v: &Val) -> Option<i64> {
    let t = v.text();
    let t = t.trim().trim_start_matches("[[").trim_end_matches("]]");
    let d = t.get(..10)?;
    if t.len() > 10 && !t[10..].starts_with([' ', 'T']) {
        return None;
    }
    let (y, m, dd) = crate::util::parse_date(d)?;
    Some(crate::util::days_from_civil(y, m, dd))
}

#[derive(Clone, Copy)]
enum Shift {
    Days(i64),
    Months(i64),
}

impl Shift {
    fn neg(self) -> Shift {
        match self {
            Shift::Days(n) => Shift::Days(-n),
            Shift::Months(n) => Shift::Months(-n),
        }
    }
}

/// `3d`, `2w`, `1M` or `1 month`, `1y`, `10 days`.
fn duration(s: &str) -> Option<Shift> {
    let s = s.trim();
    let at = s
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(s.len());
    let n: i64 = s[..at].parse().ok()?;
    Some(match s[at..].trim() {
        "d" | "day" | "days" => Shift::Days(n),
        "w" | "week" | "weeks" => Shift::Days(n * 7),
        "M" | "month" | "months" | "mo" => Shift::Months(n),
        "y" | "year" | "years" => Shift::Months(n * 12),
        _ => return None,
    })
}

fn shift_date(day: i64, by: Shift) -> String {
    let (y, m, d) = match by {
        Shift::Days(n) => crate::util::civil_from_days(day + n),
        Shift::Months(n) => {
            let (y, m, d) = crate::util::civil_from_days(day);
            let total = y * 12 + i64::from(m) - 1 + n;
            let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
            // the 31st of a short month is its last day
            let last = (28..=31)
                .rev()
                .find(|&x| crate::util::parse_date(&format!("{ny:04}-{nm:02}-{x:02}")).is_some())
                .unwrap_or(28);
            (ny, nm, d.min(last))
        }
    };
    format!("{y:04}-{m:02}-{d:02}")
}

fn folder_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(f, _)| f)
}

impl Env<'_> {
    fn prop(&self, name: &str) -> Val {
        self.note.parsed.prop(name).map_or(Val::Null, prop_val)
    }

    fn file(&self, name: &str) -> Result<Val, String> {
        let n = self.note;
        Ok(match name {
            "name" | "basename" | "title" => Val::Str(crate::util::stem(&n.path).to_string()),
            "path" => Val::Str(n.path.clone()),
            "folder" => Val::Str(folder_of(&n.path).to_string()),
            "ext" => Val::Str(crate::render::ext(&n.path)),
            "size" => Val::Num(n.size as f64),
            "mtime" | "ctime" | "mday" | "cday" => Val::Str(local_time(n.mtime)),
            "tags" | "etags" => {
                Val::List(n.parsed.tags.iter().map(|t| Val::Str(t.clone())).collect())
            }
            "links" | "outlinks" => Val::List(
                self.v
                    .links_of(&n.path)
                    .into_iter()
                    .map(|(l, r)| {
                        Val::Str(r.map_or(l.target, |p| crate::util::stem(&p).to_string()))
                    })
                    .collect(),
            ),
            "backlinks" | "inlinks" => Val::List(
                self.v
                    .backlinks(&n.path)
                    .into_iter()
                    .map(|b| Val::Str(crate::util::stem(&b.source).to_string()))
                    .collect(),
            ),
            "words" => Val::Num(n.parsed.words as f64),
            "tasks" => Val::Num(n.parsed.tasks.len() as f64),
            "link" | "file" => Val::Str(format!("[[{}]]", crate::util::stem(&n.path))),
            "day" => {
                let s = crate::util::stem(&n.path);
                if crate::util::parse_date(s.get(..10).unwrap_or("")).is_some() {
                    Val::Str(s[..10].to_string())
                } else {
                    Val::Null
                }
            }
            other => return Err(format!("file.{other} is not something a file has")),
        })
    }

    fn formula(&self, name: &str) -> Result<Val, String> {
        let f = self
            .formulas
            .iter()
            .find(|(k, _)| k == name)
            .ok_or_else(|| format!("there is no formula \"{name}\""))?;
        if self.depth > 16 {
            return Err(format!("formula \"{name}\" uses itself"));
        }
        let deeper = Env {
            depth: self.depth + 1,
            ..*self
        };
        deeper.eval(&f.1)
    }

    fn name(&self, w: &str) -> Result<Val, String> {
        if let Some(t) = self.task {
            match w {
                "completed" | "checked" => return Ok(Val::Bool(t.done)),
                "text" => return Ok(Val::Str(t.text.clone())),
                "due" => return Ok(t.due.clone().map_or(Val::Null, Val::Str)),
                "status" => return Ok(Val::Str(t.status.to_string())),
                "line" => return Ok(Val::Num(t.line as f64)),
                _ => {}
            }
        }
        Ok(match w {
            "today" => Val::Str(self.today.to_string()),
            "now" => Val::Str(self.now.to_string()),
            _ => self.prop(w),
        })
    }

    pub fn eval(&self, e: &Expr) -> Result<Val, String> {
        match e {
            Expr::Lit(v) => Ok(v.clone()),
            Expr::List(items) => Ok(Val::List(
                items
                    .iter()
                    .map(|x| self.eval(x))
                    .collect::<Result<_, _>>()?,
            )),
            Expr::Id(w) => match w.as_str() {
                // a property that happens to have one of these names, else a hint
                "file" | "note" | "formula" | "this" if self.note.parsed.prop(w).is_some() => {
                    Ok(self.prop(w))
                }
                "file" | "note" | "formula" | "this" => {
                    Err(format!("{w} needs a name after it, like {w}.name"))
                }
                _ => self.name(w),
            },
            Expr::Member(obj, name) => match obj.as_ref() {
                Expr::Id(ns) if ns == "file" => self.file(name),
                Expr::Id(ns) if ns == "note" || ns == "this" => Ok(self.prop(name)),
                Expr::Id(ns) if ns == "formula" => self.formula(name),
                _ => {
                    let v = self.eval(obj)?;
                    match name.as_str() {
                        "length" => Ok(Val::Num(match &v {
                            Val::List(l) => l.len() as f64,
                            Val::Str(s) => s.chars().count() as f64,
                            Val::Null => 0.0,
                            _ => 1.0,
                        })),
                        other => Err(format!(".{other} needs brackets, like .{other}()")),
                    }
                }
            },
            Expr::Index(obj, idx) => {
                let i = self.eval(idx)?;
                if matches!(obj.as_ref(), Expr::Id(ns) if ns == "note" || ns == "this") {
                    return Ok(self.prop(&i.text()));
                }
                let v = self.eval(obj)?;
                Ok(match (&v, i.num()) {
                    (Val::List(l), Some(n)) => {
                        let n = if n < 0.0 { l.len() as f64 + n } else { n };
                        l.get(n as usize).cloned().unwrap_or(Val::Null)
                    }
                    _ => Val::Null,
                })
            }
            Expr::Not(x) => Ok(Val::Bool(!self.eval(x)?.truthy())),
            Expr::Neg(x) => Ok(self.eval(x)?.num().map_or(Val::Null, |n| Val::Num(-n))),
            Expr::Bin(op, a, b) => self.bin(op, a, b),
            Expr::Call(callee, args) => self.call(callee, args),
        }
    }

    fn bin(&self, op: &str, a: &Expr, b: &Expr) -> Result<Val, String> {
        if op == "+" || op == "-" {
            // dates: date - date is a number of days; date ± days (or "2w", "1 month") is a date
            let (x, y) = (self.eval(a)?, self.eval(b)?);
            if let Some(dx) = day_number(&x) {
                if op == "-" {
                    if let Some(dy) = day_number(&y) {
                        return Ok(Val::Num((dx - dy) as f64));
                    }
                }
                if let Some(shift) = y
                    .num()
                    .map(|n| Shift::Days(n as i64))
                    .or_else(|| duration(&y.text()))
                {
                    return Ok(Val::Str(shift_date(
                        dx,
                        if op == "-" { shift.neg() } else { shift },
                    )));
                }
            }
        }
        if op == "&&" {
            return Ok(Val::Bool(self.eval(a)?.truthy() && self.eval(b)?.truthy()));
        }
        if op == "||" {
            return Ok(Val::Bool(self.eval(a)?.truthy() || self.eval(b)?.truthy()));
        }
        let (x, y) = (self.eval(a)?, self.eval(b)?);
        Ok(match op {
            "==" => Val::Bool(equal(&x, &y)),
            "!=" => Val::Bool(!equal(&x, &y)),
            "<" | "<=" | ">" | ">=" => {
                if x == Val::Null || y == Val::Null {
                    return Ok(Val::Bool(false));
                }
                let o = order(&x, &y);
                Val::Bool(match op {
                    "<" => o == Some(Ordering::Less),
                    "<=" => matches!(o, Some(Ordering::Less | Ordering::Equal)),
                    ">" => o == Some(Ordering::Greater),
                    _ => matches!(o, Some(Ordering::Greater | Ordering::Equal)),
                })
            }
            "+" => match (&x, &y) {
                (Val::List(p), Val::List(q)) => Val::List(p.iter().chain(q).cloned().collect()),
                (Val::Str(_), _) | (_, Val::Str(_)) if x.num().is_none() || y.num().is_none() => {
                    Val::Str(format!("{}{}", x.text(), y.text()))
                }
                _ => match (x.num(), y.num()) {
                    (Some(p), Some(q)) => Val::Num(p + q),
                    _ => Val::Null,
                },
            },
            _ => match (x.num(), y.num()) {
                (Some(p), Some(q)) => match op {
                    "-" => Val::Num(p - q),
                    "*" => Val::Num(p * q),
                    "/" if q != 0.0 => Val::Num(p / q),
                    "%" if q != 0.0 => Val::Num(p % q),
                    _ => Val::Null,
                },
                _ => Val::Null,
            },
        })
    }

    fn call(&self, callee: &Expr, args: &[Expr]) -> Result<Val, String> {
        let vals =
            |s: &Self| -> Result<Vec<Val>, String> { args.iter().map(|a| s.eval(a)).collect() };
        match callee {
            Expr::Member(obj, m) if matches!(obj.as_ref(), Expr::Id(ns) if ns == "file") => {
                let a = vals(self)?;
                let n = self.note;
                Ok(Val::Bool(match m.as_str() {
                    "hasTag" => a.iter().flat_map(Val::items).any(|t| {
                        let t = t.text();
                        let t = t.trim_start_matches('#');
                        crate::vault::Vault::has_tag(n, t)
                    }),
                    "inFolder" => a.iter().any(|f| {
                        let f = f.text();
                        let f = f.trim_matches('/');
                        f.is_empty() || n.path.starts_with(&format!("{f}/"))
                    }),
                    "hasLink" => {
                        let want: Vec<String> = a
                            .iter()
                            .map(|x| {
                                x.text()
                                    .trim_start_matches("[[")
                                    .trim_end_matches("]]")
                                    .to_lowercase()
                            })
                            .collect();
                        self.v.links_of(&n.path).into_iter().any(|(l, r)| {
                            let stem = r
                                .as_deref()
                                .map(crate::util::stem)
                                .unwrap_or(&l.target)
                                .to_lowercase();
                            let path = r.unwrap_or_default().to_lowercase();
                            want.iter().any(|w| {
                                *w == stem || *w == path || path.trim_end_matches(".md") == *w
                            })
                        })
                    }
                    "hasProperty" => a.iter().any(|p| n.parsed.prop(&p.text()).is_some()),
                    other => return Err(format!("file.{other}() is not something a file can do")),
                }))
            }
            Expr::Member(obj, m) => {
                let v = self.eval(obj)?;
                let a = vals(self)?;
                self.method(&v, m, &a)
            }
            Expr::Id(f) => {
                if f == "if" || f == "choice" {
                    let c = args.first().ok_or("if() needs a condition")?;
                    return if self.eval(c)?.truthy() {
                        args.get(1).map_or(Ok(Val::Null), |x| self.eval(x))
                    } else {
                        args.get(2).map_or(Ok(Val::Null), |x| self.eval(x))
                    };
                }
                let a = vals(self)?;
                let first = a.first().cloned().unwrap_or(Val::Null);
                Ok(match f.as_str() {
                    "today" => Val::Str(self.today.to_string()),
                    "now" => Val::Str(self.now.to_string()),
                    "date" => match first.text().as_str() {
                        "today" | "" => Val::Str(self.today.to_string()),
                        "now" => Val::Str(self.now.to_string()),
                        d => Val::Str(
                            d.trim()
                                .trim_start_matches("[[")
                                .trim_end_matches("]]")
                                .to_string(),
                        ),
                    },
                    "number" => first.num().map_or(Val::Null, Val::Num),
                    "string" | "text" => Val::Str(first.text()),
                    "list" => Val::List(a.iter().flat_map(Val::items).collect()),
                    "length" | "len" => self.method(&first, "lengthOf", &[])?,
                    "contains" => {
                        let needle = a.get(1).cloned().unwrap_or(Val::Null);
                        self.method(&first, "contains", &[needle])?
                    }
                    "lower" => Val::Str(first.text().to_lowercase()),
                    "upper" => Val::Str(first.text().to_uppercase()),
                    "default" => {
                        if first == Val::Null {
                            a.get(1).cloned().unwrap_or(Val::Null)
                        } else {
                            first
                        }
                    }
                    "min" | "max" => {
                        let all: Vec<Val> = a
                            .iter()
                            .flat_map(Val::items)
                            .filter(|x| *x != Val::Null)
                            .collect();
                        let pick = all.into_iter().reduce(|p, q| {
                            let less = order(&p, &q) == Some(Ordering::Less);
                            if (f == "min") == less {
                                p
                            } else {
                                q
                            }
                        });
                        pick.unwrap_or(Val::Null)
                    }
                    other => return Err(format!("{other}() is not a function here")),
                })
            }
            _ => Err("only names can be called".into()),
        }
    }

    fn method(&self, v: &Val, m: &str, a: &[Val]) -> Result<Val, String> {
        let arg = |i: usize| a.get(i).cloned().unwrap_or(Val::Null);
        let text = v.text();
        Ok(match m {
            "contains" => match v {
                Val::List(l) => Val::Bool(l.iter().any(|x| same_item(x, &arg(0)))),
                Val::Null => Val::Bool(false),
                _ => Val::Bool(text.to_lowercase().contains(&arg(0).text().to_lowercase())),
            },
            "containsAll" => Val::Bool(a.iter().all(|x| {
                self.method(v, "contains", std::slice::from_ref(x))
                    .map(|r| r.truthy())
                    .unwrap_or(false)
            })),
            "containsAny" => Val::Bool(a.iter().any(|x| {
                self.method(v, "contains", std::slice::from_ref(x))
                    .map(|r| r.truthy())
                    .unwrap_or(false)
            })),
            "startsWith" => Val::Bool(text.starts_with(&arg(0).text())),
            "endsWith" => Val::Bool(text.ends_with(&arg(0).text())),
            "lower" => Val::Str(text.to_lowercase()),
            "upper" => Val::Str(text.to_uppercase()),
            "trim" => Val::Str(text.trim().to_string()),
            "isEmpty" => Val::Bool(!v.truthy()),
            "lengthOf" => Val::Num(match v {
                Val::List(l) => l.len() as f64,
                Val::Null => 0.0,
                _ => text.chars().count() as f64,
            }),
            "toFixed" => {
                let d = arg(0).num().unwrap_or(0.0).clamp(0.0, 12.0) as usize;
                v.num().map_or(Val::Null, |n| Val::Str(format!("{n:.d$}")))
            }
            "round" => v.num().map_or(Val::Null, |n| Val::Num(n.round())),
            "join" => {
                let sep = if a.is_empty() {
                    ", ".to_string()
                } else {
                    arg(0).text()
                };
                Val::Str(
                    v.items()
                        .iter()
                        .map(Val::text)
                        .collect::<Vec<_>>()
                        .join(&sep),
                )
            }
            "split" => Val::List(
                text.split(&arg(0).text())
                    .map(|s| Val::Str(s.to_string()))
                    .collect(),
            ),
            other => return Err(format!(".{other}() is not something a value can do")),
        })
    }
}

// ---- filters and views

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    E(Expr),
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Vec<Filter>),
}

fn filter_of(y: &Y) -> Result<Filter, String> {
    match y {
        Y::Str(s) => Ok(Filter::E(
            parse_expr(s).map_err(|e| format!("in \"{s}\": {e}"))?,
        )),
        Y::List(l) => Ok(Filter::And(
            l.iter().map(filter_of).collect::<Result<_, _>>()?,
        )),
        Y::Map(m) if m.len() == 1 => {
            let (k, v) = &m[0];
            let inner = match v {
                Y::List(l) => l.iter().map(filter_of).collect::<Result<_, _>>()?,
                other => vec![filter_of(other)?],
            };
            match k.as_str() {
                "and" => Ok(Filter::And(inner)),
                "or" => Ok(Filter::Or(inner)),
                "not" => Ok(Filter::Not(inner)),
                other => Err(format!(
                    "filters group with and, or or not, not \"{other}\""
                )),
            }
        }
        Y::Map(_) => Err("a filter group has one key: and, or or not".into()),
        Y::Null => Ok(Filter::And(vec![])),
    }
}

fn test(f: &Filter, env: &Env) -> Result<bool, String> {
    Ok(match f {
        Filter::E(e) => env.eval(e)?.truthy(),
        Filter::And(fs) => {
            for x in fs {
                if !test(x, env)? {
                    return Ok(false);
                }
            }
            true
        }
        Filter::Or(fs) => {
            for x in fs {
                if test(x, env)? {
                    return Ok(true);
                }
            }
            fs.is_empty()
        }
        Filter::Not(fs) => {
            for x in fs {
                if test(x, env)? {
                    return Ok(false);
                }
            }
            true
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Table,
    Cards,
    List,
    Task,
    /// Cards in lanes, one lane per value of the `groupBy` property (a Kanban board).
    Board,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Col {
    pub id: String,
    pub label: String,
    pub expr: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub name: String,
    pub kind: Kind,
    pub filters: Option<Filter>,
    pub cols: Vec<Col>,
    pub sort: Vec<(Expr, bool)>,
    pub group: Option<(Expr, bool)>,
    pub limit: Option<usize>,
    pub image: Option<Expr>,
    /// The first column is the note itself (Dataview's `WITHOUT ID` turns it off).
    pub with_id: bool,
    /// Board lanes in order (`lanes: [Todo, Doing, Done]`), shown even when empty.
    pub lanes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Base {
    pub filters: Option<Filter>,
    pub formulas: Vec<(String, Expr)>,
    pub labels: Vec<(String, String)>,
    pub views: Vec<View>,
}

/// `file.name` reads as "Name", `note.status` and `status` as "status", `formula.x` as "x".
fn label_for(id: &str, labels: &[(String, String)]) -> String {
    if let Some((_, l)) = labels
        .iter()
        .find(|(k, _)| k == id || k.strip_prefix("note.") == Some(id))
    {
        return l.clone();
    }
    match id {
        "file.name" | "file.link" | "file.file" => "Name".into(),
        _ => id
            .strip_prefix("note.")
            .or_else(|| id.strip_prefix("formula."))
            .or_else(|| id.strip_prefix("file."))
            .unwrap_or(id)
            .to_string(),
    }
}

/// A column of a `.base` view: `file.name`, `formula.x`, or a note property by its name as
/// written (`status`, `note.due date`), even one named like a keyword (`note`, `file`).
fn col_of(id: &str, labels: &[(String, String)]) -> Result<Col, String> {
    let plain = |s: &str| !s.is_empty() && !s.contains(['.', '(', ')', '[', ']', '"', '\'']);
    let prop = if plain(id) {
        Some(id)
    } else {
        id.strip_prefix("note.").filter(|p| plain(p))
    };
    let expr = match prop {
        Some(p) => Expr::Member(Box::new(Expr::Id("note".into())), p.trim().to_string()),
        None => parse_expr(id).map_err(|e| format!("in the column \"{id}\": {e}"))?,
    };
    Ok(Col {
        id: id.to_string(),
        label: label_for(id, labels),
        expr,
    })
}

/// A `.base` file (or a ```` ```base ```` block).
pub fn base(src: &str) -> Result<Base, String> {
    let y = yaml::parse(src)?;
    if !matches!(y, Y::Map(_)) {
        return Err("a base starts with keys: filters, formulas, properties, views".into());
    }
    let mut b = Base::default();
    if let Some(f) = y.get("filters") {
        b.filters = Some(filter_of(f)?);
    }
    for (k, v) in y.get("formulas").map(Y::entries).unwrap_or(&[]) {
        let src = v
            .str()
            .ok_or_else(|| format!("formula \"{k}\" must be text"))?;
        b.formulas.push((
            k.clone(),
            parse_expr(src).map_err(|e| format!("in formula \"{k}\": {e}"))?,
        ));
    }
    for (k, v) in y.get("properties").map(Y::entries).unwrap_or(&[]) {
        if let Some(name) = v.get("displayName").and_then(Y::str) {
            b.labels.push((k.clone(), name.to_string()));
        }
    }
    let views = y.get("views").map(Y::list).unwrap_or(&[]);
    for (i, vy) in views.iter().enumerate() {
        let kind = match vy.get("type").and_then(Y::str).unwrap_or("table") {
            "table" => Kind::Table,
            "cards" => Kind::Cards,
            "list" => Kind::List,
            "board" | "kanban" => Kind::Board,
            other => {
                return Err(format!(
                    "view type \"{other}\" is not one of table, cards, list, board"
                ))
            }
        };
        let name = vy
            .get("name")
            .and_then(Y::str)
            .map_or_else(|| format!("View {}", i + 1), str::to_string);
        let mut cols = Vec::new();
        for c in vy.get("order").map(Y::list).unwrap_or(&[]) {
            if let Some(id) = c.str() {
                cols.push(col_of(id, &b.labels)?);
            }
        }
        let mut sort = Vec::new();
        for s in vy.get("sort").map(Y::list).unwrap_or(&[]) {
            let (prop, dir) = match s {
                Y::Str(p) => (p.as_str(), "ASC"),
                m => (
                    m.get("property").and_then(Y::str).unwrap_or(""),
                    m.get("direction").and_then(Y::str).unwrap_or("ASC"),
                ),
            };
            if !prop.is_empty() {
                sort.push((parse_expr(prop)?, dir.eq_ignore_ascii_case("desc")));
            }
        }
        let group = match vy.get("groupBy") {
            Some(Y::Str(p)) => Some((parse_expr(p)?, false)),
            Some(m @ Y::Map(_)) => match m.get("property").and_then(Y::str) {
                Some(p) => Some((
                    parse_expr(p)?,
                    m.get("direction")
                        .and_then(Y::str)
                        .is_some_and(|d| d.eq_ignore_ascii_case("desc")),
                )),
                None => None,
            },
            _ => None,
        };
        if kind == Kind::Board && group.is_none() {
            return Err(format!(
                "the board \"{name}\" needs groupBy: the property its lanes come from"
            ));
        }
        let lanes = vy
            .get("lanes")
            .or_else(|| vy.get("columns"))
            .map(Y::list)
            .unwrap_or(&[])
            .iter()
            .filter_map(Y::str)
            .map(str::to_string)
            .collect();
        b.views.push(View {
            name,
            kind,
            filters: vy.get("filters").map(filter_of).transpose()?,
            cols,
            sort,
            group,
            limit: vy
                .get("limit")
                .and_then(Y::str)
                .and_then(|l| l.trim().parse().ok()),
            image: vy
                .get("image")
                .and_then(Y::str)
                .map(parse_expr)
                .transpose()?,
            with_id: true,
            lanes,
        });
    }
    if b.views.is_empty() {
        b.views.push(View {
            name: "Table".into(),
            kind: Kind::Table,
            filters: None,
            cols: vec![],
            sort: vec![],
            group: None,
            limit: None,
            image: None,
            with_id: true,
            lanes: vec![],
        });
    }
    Ok(b)
}

/// Where the Dataview keywords start, outside quotes and brackets.
fn clauses(src: &str) -> Vec<(String, String)> {
    const KEYS: [&str; 6] = ["FROM", "WHERE", "SORT", "GROUP BY", "LIMIT", "FLATTEN"];
    let up = src.to_uppercase();
    let b = src.as_bytes();
    let mut cuts = vec![(0usize, String::new())];
    let (mut q, mut depth) = (None::<u8>, 0i32);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match q {
            Some(o) if c == o => q = None,
            Some(_) => {}
            None => match c {
                b'"' | b'\'' => q = Some(c),
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                _ if depth == 0 && (i == 0 || b[i - 1].is_ascii_whitespace()) => {
                    if let Some(k) = KEYS.iter().find(|k| {
                        up[i..].starts_with(**k)
                            && up
                                .as_bytes()
                                .get(i + k.len())
                                .is_none_or(|n| n.is_ascii_whitespace())
                    }) {
                        cuts.push((i, k.to_string()));
                        i += k.len();
                        continue;
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    let mut out = Vec::new();
    for (n, (at, key)) in cuts.iter().enumerate() {
        let start = at + key.len();
        let end = cuts.get(n + 1).map_or(src.len(), |c| c.0);
        out.push((key.clone(), src[start.min(end)..end].trim().to_string()));
    }
    out
}

/// Split on commas outside quotes and brackets.
fn split_commas(s: &str) -> Vec<String> {
    let (mut out, mut cur) = (Vec::new(), String::new());
    let (mut q, mut depth) = (None::<char>, 0i32);
    for c in s.chars() {
        match q {
            Some(o) if c == o => q = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => q = Some(c),
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    out.push(std::mem::take(&mut cur).trim().to_string());
                    continue;
                }
                _ => {}
            },
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// `#tag`, `"folder"`, `[[Note]]`, joined with and, or, - and brackets, as an expression.
fn from_source(src: &str) -> Result<Expr, String> {
    let mut out = String::new();
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '#' {
            let start = i + 1;
            i = start;
            while i < cs.len() && (cs[i].is_alphanumeric() || matches!(cs[i], '/' | '_' | '-')) {
                i += 1;
            }
            let tag: String = cs[start..i].iter().collect();
            out.push_str(&format!("file.hasTag({tag:?})"));
            continue;
        }
        if c == '"' {
            let end = cs[i + 1..]
                .iter()
                .position(|&x| x == '"')
                .ok_or("a folder name in quotes is not closed")?;
            let folder: String = cs[i + 1..i + 1 + end].iter().collect();
            out.push_str(&format!("file.inFolder({folder:?})"));
            i += end + 2;
            continue;
        }
        if c == '[' && cs.get(i + 1) == Some(&'[') {
            let end = cs[i..]
                .windows(2)
                .position(|w| w == [']', ']'])
                .ok_or("[[ is not closed")?;
            let target: String = cs[i + 2..i + end].iter().collect();
            out.push_str(&format!("file.hasLink({target:?})"));
            i += end + 2;
            continue;
        }
        if c == '-' {
            out.push('!');
        } else {
            out.push(c);
        }
        i += 1;
    }
    parse_expr(&out)
}

/// A ```` ```dataview ```` query.
pub fn dataview(src: &str) -> Result<Base, String> {
    let parts = clauses(src.trim());
    let head = parts.first().map(|p| p.1.clone()).unwrap_or_default();
    let (word, rest) = head
        .split_once(char::is_whitespace)
        .unwrap_or((head.as_str(), ""));
    let mut v = View {
        name: String::new(),
        kind: Kind::List,
        filters: None,
        cols: vec![],
        sort: vec![],
        group: None,
        limit: None,
        image: None,
        with_id: true,
        lanes: vec![],
    };
    let mut rest = rest.trim().to_string();
    match word.to_uppercase().as_str() {
        "TABLE" => {
            v.kind = Kind::Table;
            if rest.to_uppercase().starts_with("WITHOUT ID") {
                v.with_id = false;
                rest = rest[10..].trim().to_string();
            }
            for f in split_commas(&rest) {
                let (e, label) = match f.to_uppercase().rfind(" AS ") {
                    Some(at) => (
                        f[..at].trim().to_string(),
                        f[at + 4..].trim().trim_matches(['"', '\'']).to_string(),
                    ),
                    None => (f.clone(), label_for(&f, &[])),
                };
                v.cols.push(Col {
                    id: e.clone(),
                    label,
                    expr: parse_expr(&e).map_err(|err| format!("in the column \"{e}\": {err}"))?,
                });
            }
        }
        "LIST" => {
            if !rest.is_empty() {
                v.cols.push(Col {
                    id: rest.clone(),
                    label: label_for(&rest, &[]),
                    expr: parse_expr(&rest)?,
                });
            }
        }
        "TASK" => v.kind = Kind::Task,
        "" => return Err("a query starts with LIST, TABLE or TASK".into()),
        other => {
            return Err(format!(
                "{other} is not a query type here (LIST, TABLE or TASK)"
            ))
        }
    }
    let mut filters = Vec::new();
    for (key, body) in &parts[1..] {
        match key.as_str() {
            "FROM" => filters.push(Filter::E(from_source(body)?)),
            "WHERE" => filters.push(Filter::E(
                parse_expr(body).map_err(|e| format!("in WHERE: {e}"))?,
            )),
            "SORT" => {
                for s in split_commas(body) {
                    let up = s.to_uppercase();
                    let (e, desc) = if up.ends_with(" DESC") || up.ends_with(" DESCENDING") {
                        (s[..s.rfind(' ').unwrap_or(s.len())].to_string(), true)
                    } else if up.ends_with(" ASC") || up.ends_with(" ASCENDING") {
                        (s[..s.rfind(' ').unwrap_or(s.len())].to_string(), false)
                    } else {
                        (s.clone(), false)
                    };
                    v.sort.push((parse_expr(e.trim())?, desc));
                }
            }
            "GROUP BY" => v.group = Some((parse_expr(body)?, false)),
            "LIMIT" => {
                v.limit = Some(
                    body.trim()
                        .parse()
                        .map_err(|_| format!("LIMIT {body} is not a number"))?,
                )
            }
            "FLATTEN" => return Err("FLATTEN is not supported yet".into()),
            _ => {}
        }
    }
    if !filters.is_empty() {
        v.filters = Some(Filter::And(filters));
    }
    Ok(Base {
        views: vec![v],
        ..Base::default()
    })
}

// ---- running

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub path: String,
    pub cells: Vec<Val>,
    pub image: Val,
    /// For task views: the task.
    pub task: Option<Task>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewOut {
    pub name: String,
    pub kind: Kind,
    pub cols: Vec<String>,
    pub with_id: bool,
    pub groups: Vec<(Option<String>, Vec<Row>)>,
    pub total: usize,
    pub error: Option<String>,
    /// The note property each column shows as-is, so a cell can be edited in place.
    pub col_props: Vec<Option<String>>,
    /// The note property the groups come from, so a card can move between board lanes.
    pub group_prop: Option<String>,
    /// The columns that are the note itself (`file.name`, `file.link`), shown as a link to it.
    pub name_cols: Vec<bool>,
}

fn now_strings() -> (String, String) {
    let now = local_time(crate::util::unix_now());
    (now[..10].to_string(), now)
}

pub fn run(v: &Vault, b: &Base) -> Vec<ViewOut> {
    let (today, now) = now_strings();
    b.views
        .iter()
        .map(|view| run_view(v, b, view, &today, &now))
        .collect()
}

/// One row worked out: its cells, sort keys, group and picture.
type Evaluated = (Vec<Val>, Vec<Val>, Option<String>, Val);

fn run_view(v: &Vault, b: &Base, view: &View, today: &str, now: &str) -> ViewOut {
    let mut out = ViewOut {
        name: view.name.clone(),
        kind: view.kind,
        cols: vec![],
        with_id: view.with_id,
        groups: vec![],
        total: 0,
        error: None,
        col_props: vec![],
        name_cols: vec![],
        group_prop: view
            .group
            .as_ref()
            .and_then(|(e, _)| e.prop_name())
            .map(str::to_string),
    };
    let mut picked: Vec<(&Note, Option<Task>)> = Vec::new();
    for n in v.notes() {
        let env = Env {
            v,
            note: n,
            formulas: &b.formulas,
            task: None,
            today,
            now,
            depth: 0,
        };
        let keep = (|| -> Result<bool, String> {
            if let Some(f) = &b.filters {
                if !test(f, &env)? {
                    return Ok(false);
                }
            }
            if view.kind == Kind::Task {
                return Ok(true);
            }
            match &view.filters {
                Some(f) => test(f, &env),
                None => Ok(true),
            }
        })();
        match keep {
            Ok(true) if view.kind == Kind::Task => {
                for t in &n.parsed.tasks {
                    let tenv = Env {
                        task: Some(t),
                        ..env
                    };
                    match view
                        .filters
                        .as_ref()
                        .map(|f| test(f, &tenv))
                        .unwrap_or(Ok(true))
                    {
                        Ok(true) => picked.push((n, Some(t.clone()))),
                        Ok(false) => {}
                        Err(e) => {
                            out.error = Some(e);
                            return out;
                        }
                    }
                }
            }
            Ok(true) => picked.push((n, None)),
            Ok(false) => {}
            Err(e) => {
                out.error = Some(e);
                return out;
            }
        }
    }
    // the columns: as given, else the name and the properties these notes have
    let mut cols = view.cols.clone();
    if cols.is_empty() && matches!(view.kind, Kind::Table | Kind::Cards | Kind::Board) {
        let mut seen: Vec<String> = Vec::new();
        for (n, _) in &picked {
            for (k, _) in &n.parsed.props {
                let k = k.to_string();
                if !seen.iter().any(|s| s.eq_ignore_ascii_case(&k))
                    && !matches!(k.as_str(), "tags" | "aliases" | "cssclasses")
                {
                    seen.push(k);
                }
            }
        }
        for k in seen.into_iter().take(6) {
            if let Ok(c) = col_of(&k, &b.labels) {
                cols.push(c);
            }
        }
    }
    let id_first = view.with_id
        && !cols
            .iter()
            .any(|c| matches!(c.id.as_str(), "file.name" | "file.link" | "file.file"));
    out.cols = cols.iter().map(|c| c.label.clone()).collect();
    out.col_props = cols
        .iter()
        .map(|c| c.expr.prop_name().map(str::to_string))
        .collect();
    out.name_cols = cols
        .iter()
        .map(|c| {
            matches!(
                c.id.as_str(),
                "file.name" | "file.link" | "file.file" | "file.basename"
            )
        })
        .collect();
    out.with_id = id_first;
    let mut rows: Vec<(Vec<Val>, Option<String>, Row)> = Vec::new();
    for (n, task) in picked {
        let env = Env {
            v,
            note: n,
            formulas: &b.formulas,
            task: task.as_ref(),
            today,
            now,
            depth: 0,
        };
        let eval_all = || -> Result<Evaluated, String> {
            let cells = cols
                .iter()
                .map(|c| env.eval(&c.expr))
                .collect::<Result<Vec<_>, _>>()?;
            let keys = view
                .sort
                .iter()
                .map(|(e, _)| env.eval(e))
                .collect::<Result<Vec<_>, _>>()?;
            let group = match &view.group {
                Some((e, _)) => Some(env.eval(e)?.text()),
                None => None,
            };
            let image = match &view.image {
                Some(e) => env.eval(e)?,
                None => Val::Null,
            };
            Ok((cells, keys, group, image))
        };
        match eval_all() {
            Ok((cells, keys, group, image)) => rows.push((
                keys,
                group,
                Row {
                    path: n.path.clone(),
                    cells,
                    image,
                    task,
                },
            )),
            Err(e) => {
                out.error = Some(e);
                return out;
            }
        }
    }
    rows.sort_by(|a, b| {
        for (i, (_, desc)) in view.sort.iter().enumerate() {
            let o = order(&a.0[i], &b.0[i]).unwrap_or(Ordering::Equal);
            let o = if *desc && a.0[i] != Val::Null && b.0[i] != Val::Null {
                o.reverse()
            } else {
                o
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        let name = |r: &Row| crate::util::stem(&r.path).to_lowercase();
        name(&a.2).cmp(&name(&b.2)).then_with(|| {
            a.2.task
                .as_ref()
                .map(|t| t.line)
                .cmp(&b.2.task.as_ref().map(|t| t.line))
        })
    });
    out.total = rows.len();
    if let Some(l) = view.limit {
        rows.truncate(l);
    }
    match &view.group {
        None => out
            .groups
            .push((None, rows.into_iter().map(|r| r.2).collect())),
        Some((_, desc)) => {
            let mut keys: Vec<String> = Vec::new();
            for r in &rows {
                let k = r.1.clone().unwrap_or_default();
                if !keys.iter().any(|x| x.eq_ignore_ascii_case(&k)) {
                    keys.push(k);
                }
            }
            keys.sort_by_key(|k| (k.is_empty(), k.to_lowercase()));
            if *desc {
                keys.reverse();
            }
            if !view.lanes.is_empty() {
                // the lanes as listed first (empty ones too), then any other values
                let mut ordered: Vec<String> = view.lanes.clone();
                for k in keys {
                    if !ordered.iter().any(|l| l.eq_ignore_ascii_case(&k)) {
                        ordered.push(k);
                    }
                }
                keys = ordered;
            }
            for k in keys {
                let members = rows
                    .iter()
                    .filter(|r| r.1.as_deref().unwrap_or("").eq_ignore_ascii_case(&k))
                    .map(|r| r.2.clone())
                    .collect();
                out.groups.push((Some(k), members));
            }
        }
    }
    out
}

/// A query as Tempo or the terminal writes it: Dataview (`TABLE ...`, `LIST ...`, `TASK ...`)
/// or a base (the YAML of a `.base` file).
pub fn any(src: &str) -> Result<Base, String> {
    let t = src.trim_start();
    let w = t.split_whitespace().next().unwrap_or("").to_uppercase();
    if matches!(w.as_str(), "TABLE" | "LIST" | "TASK") {
        dataview(t)
    } else {
        base(src)
    }
}

impl Val {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Val::Null => serde_json::Value::Null,
            Val::Bool(b) => (*b).into(),
            Val::Num(n) => serde_json::Number::from_f64(*n)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            Val::Str(s) => s.clone().into(),
            Val::List(l) => l.iter().map(Val::to_json).collect(),
        }
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Table => "table",
        Kind::Cards => "cards",
        Kind::List => "list",
        Kind::Task => "task",
        Kind::Board => "board",
    }
}

/// Results as data: each view with its columns and rows.
pub fn to_json(outs: &[ViewOut]) -> serde_json::Value {
    outs.iter()
        .map(|o| {
            let rows = |rs: &[Row]| -> Vec<serde_json::Value> {
                rs.iter()
                    .map(|r| {
                        let mut m = serde_json::Map::new();
                        m.insert("path".into(), r.path.clone().into());
                        for (c, v) in o.cols.iter().zip(&r.cells) {
                            m.insert(c.clone(), v.to_json());
                        }
                        if let Some(t) = &r.task {
                            m.insert("task".into(), serde_json::json!({"line": t.line, "text": t.text, "done": t.done, "due": t.due}));
                        }
                        serde_json::Value::Object(m)
                    })
                    .collect()
            };
            serde_json::json!({
                "view": o.name,
                "type": kind_name(o.kind),
                "columns": o.cols,
                "total": o.total,
                "error": o.error,
                "groups": o.groups.iter().map(|(k, rs)| serde_json::json!({"group": k, "rows": rows(rs)})).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// Results as Markdown, for Tempo and the terminal: a table per view (lists and tasks as lists).
pub fn markdown(outs: &[ViewOut]) -> String {
    let cell = |v: &Val| v.text().replace('|', "\\|").replace('\n', " ");
    let mut s = String::new();
    for o in outs {
        let shown: usize = o.groups.iter().map(|g| g.1.len()).sum();
        let what = if o.kind == Kind::Task {
            "tasks"
        } else {
            "notes"
        };
        let name = if o.name.is_empty() {
            kind_name(o.kind).to_string()
        } else {
            o.name.clone()
        };
        s.push_str(&format!(
            "## {name} ({shown}{} {what})\n",
            if shown < o.total {
                format!(" of {}", o.total)
            } else {
                String::new()
            }
        ));
        if let Some(e) = &o.error {
            s.push_str(&format!("error: {e}\n\n"));
            continue;
        }
        for (key, rows) in &o.groups {
            if let Some(k) = key {
                s.push_str(&format!(
                    "\n### {}\n",
                    if k.is_empty() { "(no value)" } else { k }
                ));
            }
            match o.kind {
                Kind::Table | Kind::Cards | Kind::Board => {
                    let mut head: Vec<String> = Vec::new();
                    if o.with_id {
                        head.push("note".into());
                    }
                    head.extend(o.cols.iter().cloned());
                    s.push_str(&format!(
                        "| {} |\n|{}\n",
                        head.join(" | "),
                        "---|".repeat(head.len())
                    ));
                    for r in rows {
                        let mut cells: Vec<String> = Vec::new();
                        if o.with_id {
                            cells.push(format!("[[{}]]", crate::util::stem(&r.path)));
                        }
                        cells.extend(r.cells.iter().enumerate().map(|(i, c)| {
                            if o.name_cols.get(i).copied().unwrap_or(false) {
                                format!("[[{}]]", crate::util::stem(&r.path))
                            } else {
                                cell(c)
                            }
                        }));
                        s.push_str(&format!("| {} |\n", cells.join(" | ")));
                    }
                }
                Kind::List => {
                    for r in rows {
                        let extra: Vec<String> =
                            r.cells.iter().map(cell).filter(|x| !x.is_empty()).collect();
                        s.push_str(&format!(
                            "- [[{}]]{}\n",
                            crate::util::stem(&r.path),
                            if extra.is_empty() {
                                String::new()
                            } else {
                                format!(": {}", extra.join(", "))
                            }
                        ));
                    }
                }
                Kind::Task => {
                    for r in rows {
                        if let Some(t) = &r.task {
                            s.push_str(&format!(
                                "- [{}] {} ({} line {}{})\n",
                                if t.done { "x" } else { " " },
                                t.text,
                                r.path,
                                t.line,
                                t.due
                                    .as_deref()
                                    .map(|d| format!(", due {d}"))
                                    .unwrap_or_default()
                            ));
                        }
                    }
                }
            }
        }
        s.push('\n');
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::tests::{put, tmp};

    fn sample() -> Vault {
        let d = tmp("query");
        put(&d, "Books/Dune.md", "---\nauthor: \"[[Frank Herbert]]\"\nstatus: reading\nrating: 5\nyear: 1965\ntags: [book, scifi]\n---\n# Dune\n- [ ] finish part two 📅 2026-11-10\n- [x] buy it\n");
        put(&d, "Books/Emma.md", "---\nauthor: Jane Austen\nstatus: done\nrating: 4\nyear: 1815\ntags: [book]\n---\nRead in [[2026-01-02]].\n");
        put(&d, "Books/Neuromancer.md", "---\nauthor: William Gibson\nstatus: want\nyear: 1984\ntags: [book, scifi/cyberpunk]\n---\nSee [[Dune]].\n");
        put(&d, "Frank Herbert.md", "# Frank Herbert\n#person\n");
        put(
            &d,
            "Daily/2026-01-02.md",
            "Read [[Emma]]. #journal\n- [ ] call mum\n",
        );
        Vault::open(&d).unwrap()
    }

    fn names(o: &ViewOut) -> Vec<String> {
        o.groups
            .iter()
            .flat_map(|g| g.1.iter().map(|r| crate::util::stem(&r.path).to_string()))
            .collect()
    }

    fn env_eval(v: &Vault, path: &str, src: &str) -> Val {
        let n = v.note(path).unwrap();
        let f = vec![("double".to_string(), parse_expr("rating * 2").unwrap())];
        Env {
            v,
            note: n,
            formulas: &f,
            task: None,
            today: "2026-11-06",
            now: "2026-11-06 12:00",
            depth: 0,
        }
        .eval(&parse_expr(src).unwrap())
        .unwrap()
    }

    #[test]
    fn expressions() {
        let v = sample();
        let d = "Books/Dune.md";
        for (src, want) in [
            ("rating > 4", Val::Bool(true)),
            ("rating >= 5 && status == \"reading\"", Val::Bool(true)),
            ("status != 'done' and year < 2000", Val::Bool(true)),
            ("not (rating = 5)", Val::Bool(false)),
            ("file.hasTag(\"scifi\")", Val::Bool(true)),
            ("file.hasTag(\"#book\", \"nope\")", Val::Bool(true)),
            ("file.inFolder(\"Books\")", Val::Bool(true)),
            ("file.inFolder(\"Book\")", Val::Bool(false)),
            ("file.hasLink(\"Frank Herbert\")", Val::Bool(true)),
            ("file.hasProperty(\"rating\")", Val::Bool(true)),
            ("file.name", Val::Str("Dune".into())),
            (
                "file.folder + \"/\" + file.ext",
                Val::Str("Books/md".into()),
            ),
            ("formula.double", Val::Num(10.0)),
            ("(rating / 2).toFixed(1)", Val::Str("2.5".into())),
            (
                "if(rating > 4, \"great\", \"ok\")",
                Val::Str("great".into()),
            ),
            ("tags.contains(\"scifi\")", Val::Bool(true)),
            ("contains(file.tags, \"#book\")", Val::Bool(true)),
            ("file.tags.length", Val::Num(2.0)),
            ("length(file.tags) + 1", Val::Num(3.0)),
            ("author.contains(\"herbert\")", Val::Bool(true)),
            ("note[\"status\"].upper()", Val::Str("READING".into())),
            ("missing", Val::Null),
            ("missing > 3", Val::Bool(false)),
            ("default(missing, \"none\")", Val::Str("none".into())),
            ("max(1, rating, 3)", Val::Num(5.0)),
            ("[1, 2, 3][1] * 10", Val::Num(20.0)),
            ("today", Val::Str("2026-11-06".into())),
            ("date(today) > \"2026-01-01\"", Val::Bool(true)),
            ("-rating + 1", Val::Num(-4.0)),
            ("10 % 4 / 0", Val::Null),
            ("date(\"2026-11-20\") - today()", Val::Num(14.0)),
            ("\"2026-11-06 14:05\" - \"2026-11-01\"", Val::Num(5.0)),
            ("today + 7", Val::Str("2026-11-13".into())),
            ("today - \"2w\"", Val::Str("2026-10-23".into())),
            (
                "date(\"2026-01-31\") + \"1 month\"",
                Val::Str("2026-02-28".into()),
            ),
            (
                "date(\"2024-02-29\") + \"1y\"",
                Val::Str("2025-02-28".into()),
            ),
            ("\"a\" + 1", Val::Str("a1".into())),
        ] {
            assert_eq!(env_eval(&v, d, src), want, "{src}");
        }
        assert!(
            env_eval(&v, "Books/Neuromancer.md", "file.hasTag(\"scifi\")").truthy(),
            "a parent tag matches its children"
        );
        assert!(
            env_eval(
                &v,
                "Books/Neuromancer.md",
                "file.backlinks.contains(\"Dune\")"
            ) == Val::Bool(false)
        );
        assert!(env_eval(&v, d, "file.backlinks.contains(\"Neuromancer\")").truthy());
        for bad in [
            "rating >",
            "(1",
            "\"open",
            "file",
            "file.nope",
            "nope()",
            "rating.what",
            "1 ^ 2",
        ] {
            let e = parse_expr(bad).and_then(|e| {
                let n = v.note(d).unwrap();
                Env {
                    v: &v,
                    note: n,
                    formulas: &[],
                    task: None,
                    today: "",
                    now: "",
                    depth: 0,
                }
                .eval(&e)
            });
            assert!(e.is_err(), "{bad} should fail");
        }
    }

    #[test]
    fn a_base_with_views() {
        let v = sample();
        let b = base(
            r#"
filters:
  and:
    - file.hasTag("book")
formulas:
  age: "2026 - year"
properties:
  formula.age:
    displayName: Years old
views:
  - type: table
    name: Reading list
    filters:
      not:
        - 'status == "done"'
    order: [file.name, author, formula.age]
    sort:
      - property: year
        direction: DESC
  - type: cards
    name: By status
    groupBy: status
    image: note.cover
    limit: 2
"#,
        )
        .unwrap();
        let out = run(&v, &b);
        assert_eq!(out.len(), 2);
        let t = &out[0];
        assert_eq!(t.error, None);
        assert_eq!(t.cols, ["Name", "author", "Years old"]);
        assert_eq!(
            names(t),
            ["Neuromancer", "Dune"],
            "newest first, Emma (done) left out"
        );
        assert_eq!(t.groups[0].1[0].cells[2], Val::Num(42.0));
        let c = &out[1];
        assert_eq!(c.total, 3);
        let groups: Vec<_> = c
            .groups
            .iter()
            .map(|g| (g.0.clone().unwrap(), g.1.len()))
            .collect();
        assert_eq!(
            groups.iter().map(|g| g.1).sum::<usize>(),
            2,
            "limited to two"
        );
        assert!(
            c.cols.contains(&"status".to_string()),
            "columns default to the properties: {:?}",
            c.cols
        );
        let bad = base("views:\n  - type: chart\n").unwrap_err();
        assert!(bad.contains("chart"), "{bad}");
        let bad = base("filters:\n  maybe:\n    - x\n").unwrap_err();
        assert!(bad.contains("and, or or not"), "{bad}");
        let errs = run(&v, &base("filters: 'nope()'\n").unwrap());
        assert!(
            errs[0].error.as_deref().unwrap().contains("nope"),
            "{:?}",
            errs[0].error
        );
    }

    #[test]
    fn dataview_queries() {
        let v = sample();
        let q = dataview("TABLE author, rating AS \"Stars\"\nFROM #book AND \"Books\"\nWHERE rating >= 4\nSORT rating DESC").unwrap();
        let o = &run(&v, &q)[0];
        assert_eq!(o.kind, Kind::Table);
        assert_eq!(o.cols, ["author", "Stars"]);
        assert!(o.with_id);
        assert_eq!(names(o), ["Dune", "Emma"]);
        let l = &run(&v, &dataview("LIST FROM [[Dune]]").unwrap())[0];
        assert_eq!(names(l), ["Neuromancer"]);
        let l = &run(&v, &dataview("list from #book and -#scifi").unwrap())[0];
        assert_eq!(names(l), ["Emma"]);
        let t = &run(&v, &dataview("TASK WHERE !completed").unwrap())[0];
        let tasks: Vec<_> = t.groups[0]
            .1
            .iter()
            .map(|r| r.task.as_ref().unwrap().text.clone())
            .collect();
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        assert!(
            tasks.iter().any(|x| x.contains("finish part two"))
                && tasks.iter().any(|x| x.contains("call mum"))
        );
        let g = &run(
            &v,
            &dataview("TABLE WITHOUT ID file.name, status FROM #book GROUP BY status LIMIT 10")
                .unwrap(),
        )[0];
        assert!(!g.with_id);
        assert_eq!(g.groups.len(), 3);
        assert!(dataview("CALENDAR file.day")
            .unwrap_err()
            .contains("CALENDAR"));
        assert!(dataview("LIST FROM \"open").is_err());
        assert!(dataview("TABLE x LIMIT many").unwrap_err().contains("many"));
        let k = clauses("TABLE \"a FROM b\" FROM #x WHERE y = \"SORT\"");
        assert_eq!(
            k.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            ["", "FROM", "WHERE"],
            "keywords in quotes are text"
        );
    }
}
