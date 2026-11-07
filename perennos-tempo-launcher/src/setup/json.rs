//! A small JSON reader and writer (std only), for the model catalog, Ollama's answers and the
//! agent's config file. Objects keep their key order, and a number keeps the text it had, so
//! rewriting a user's file changes nothing but what setup adds.

#[derive(Clone, Debug, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

const MAX_DEPTH: usize = 64;

pub fn parse(text: &str) -> Result<J, String> {
    let mut p = P {
        b: text.trim_start_matches('\u{feff}').as_bytes(),
        i: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("unexpected text at byte {}", p.i));
    }
    Ok(v)
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", c as char, self.i))
        }
    }

    fn lit(&mut self, word: &str, v: J) -> Result<J, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(format!("bad value at byte {}", self.i))
        }
    }

    fn value(&mut self, depth: usize) -> Result<J, String> {
        if depth > MAX_DEPTH {
            return Err("nested too deeply".into());
        }
        self.ws();
        match self.b.get(self.i) {
            None => Err("unexpected end".into()),
            Some(b'{') => {
                self.i += 1;
                let mut out = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(J::Obj(out));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.eat(b':')?;
                    out.push((k, self.value(depth + 1)?));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(J::Obj(out));
                        }
                        _ => return Err(format!("expected ',' or '}}' at byte {}", self.i)),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut out = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(J::Arr(out));
                }
                loop {
                    out.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(J::Arr(out));
                        }
                        _ => return Err(format!("expected ',' or ']' at byte {}", self.i)),
                    }
                }
            }
            Some(b'"') => Ok(J::Str(self.string()?)),
            Some(b't') => self.lit("true", J::Bool(true)),
            Some(b'f') => self.lit("false", J::Bool(false)),
            Some(b'n') => self.lit("null", J::Null),
            Some(c) if c.is_ascii_digit() || *c == b'-' => {
                let s = self.i;
                while self.i < self.b.len()
                    && matches!(
                        self.b[self.i],
                        b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'
                    )
                {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.b[s..self.i]).unwrap_or("");
                if t.parse::<f64>().is_ok() {
                    Ok(J::Num(t.to_string()))
                } else {
                    Err(format!("bad number at byte {s}"))
                }
            }
            Some(_) => Err(format!("unexpected character at byte {}", self.i)),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self
            .b
            .get(self.i..self.i + 4)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or("bad \\u escape")?;
        self.i += 4;
        Ok(h)
    }

    fn string(&mut self) -> Result<String, String> {
        if self.b.get(self.i) != Some(&b'"') {
            return Err(format!("expected a string at byte {}", self.i));
        }
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = *self.b.get(self.i).ok_or("unterminated string")?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("unterminated string")?;
                    self.i += 1;
                    let ch = match e {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'/' | b'\\' | b'"' => e as char,
                        b'u' => {
                            let mut u = self.hex4()?;
                            if (0xD800..0xDC00).contains(&u) {
                                if self.b.get(self.i..self.i + 2) == Some(b"\\u") {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    u = 0x10000 + ((u - 0xD800) << 10) + lo.wrapping_sub(0xDC00);
                                } else {
                                    u = 0xFFFD;
                                }
                            }
                            char::from_u32(u).unwrap_or('\u{FFFD}')
                        }
                        _ => return Err("bad escape".into()),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                _ => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| "not UTF-8".into())
    }
}

impl J {
    pub fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(J::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        match self.get(key) {
            Some(J::Num(n)) => n.parse().ok().filter(|v: &f64| v.is_finite()),
            _ => None,
        }
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        match self.get(key) {
            Some(J::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    pub fn arr(&self, key: &str) -> &[J] {
        match self.get(key) {
            Some(J::Arr(a)) => a,
            _ => &[],
        }
    }

    pub fn strs(&self, key: &str) -> Vec<String> {
        self.arr(key)
            .iter()
            .filter_map(|v| match v {
                J::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    /// Set a key on an object (replace in place, or append).
    pub fn set(&mut self, key: &str, v: J) {
        if let J::Obj(o) = self {
            match o.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = v,
                None => o.push((key.to_string(), v)),
            }
        }
    }

    pub fn s(v: &str) -> J {
        J::Str(v.to_string())
    }

    pub fn n(v: f64) -> J {
        J::Num(if v.fract() == 0.0 && v.abs() < 1e15 {
            format!("{}", v as i64)
        } else {
            format!("{v}")
        })
    }

    pub fn strings(v: &[String]) -> J {
        J::Arr(v.iter().map(|s| J::Str(s.clone())).collect())
    }

    pub fn pretty(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, 0);
        s.push('\n');
        s
    }

    fn write(&self, out: &mut String, ind: usize) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => out.push_str(n),
            J::Str(s) => out.push_str(&quote(s)),
            J::Arr(a) if a.iter().all(|v| !matches!(v, J::Arr(_) | J::Obj(_))) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    v.write(out, ind);
                }
                out.push(']');
            }
            J::Arr(a) => {
                out.push_str("[\n");
                for (i, v) in a.iter().enumerate() {
                    out.push_str(&" ".repeat(ind + 2));
                    v.write(out, ind + 2);
                    out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
                }
                out.push_str(&" ".repeat(ind));
                out.push(']');
            }
            J::Obj(o) if o.is_empty() => out.push_str("{}"),
            J::Obj(o) => {
                out.push_str("{\n");
                for (i, (k, v)) in o.iter().enumerate() {
                    out.push_str(&" ".repeat(ind + 2));
                    out.push_str(&quote(k));
                    out.push_str(": ");
                    v.write(out, ind + 2);
                    out.push_str(if i + 1 < o.len() { ",\n" } else { "\n" });
                }
                out.push_str(&" ".repeat(ind));
                out.push('}');
            }
        }
    }
}

fn quote(s: &str) -> String {
    format!("\"{}\"", crate::json_escape(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_keeps_order_and_numbers() {
        let t = r#"{"b":1.50,"a":[1,"x",{"k":null}],"c":true,"s":"q\"\u00e9\ud83d\ude00"}"#;
        let v = parse(t).unwrap();
        assert_eq!(parse(&v.pretty()).unwrap(), v);
        match &v {
            J::Obj(o) => assert_eq!(o[0].0, "b"),
            _ => panic!(),
        }
        assert_eq!(v.f64("b"), Some(1.5));
        assert!(v.pretty().contains("1.50"));
        assert_eq!(v.str("s"), Some("q\"é😀"));
    }

    #[test]
    fn refuses_bad_text() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":}",
            "{} x",
            "\"abc",
            "nul",
            "[\"\\q\"]",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(parse(&"[".repeat(200)).is_err());
    }

    #[test]
    fn set_replaces_or_appends() {
        let mut v = parse(r#"{"a":1}"#).unwrap();
        v.set("a", J::n(2.0));
        v.set("b", J::s("x"));
        assert_eq!(v.f64("a"), Some(2.0));
        assert_eq!(v.str("b"), Some("x"));
    }
}
