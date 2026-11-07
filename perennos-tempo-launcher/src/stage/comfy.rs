//! ComfyUI's own API: `/system_stats`, `/object_info`, `/upload/image`, `/prompt`, `/history`
//! and `/view`. The `Comfy` trait is what Stage needs from a ComfyUI, so tests play a fake one;
//! `HttpComfy` is the real one, over the private network only.

use super::http::{enc, multipart, text, Http};
use crate::setup::json::{parse, J};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    /// Total graphics memory of the biggest card, in GB. 0 when ComfyUI runs on the processor.
    pub gpu_gb: f64,
    pub free_gb: f64,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutFile {
    pub node: String,
    pub name: String,
    pub subfolder: String,
    /// ComfyUI's `type`: output or temp.
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Done {
    pub files: Vec<OutFile>,
    /// How many nodes ComfyUI reused from an earlier run instead of running again.
    pub cached: usize,
    pub error: Option<String>,
}

/// What Stage asks of a ComfyUI.
pub trait Comfy: Send + Sync {
    fn base(&self) -> String;
    fn stats(&self) -> Result<Stats, String>;
    /// Of these node classes: (the ones ComfyUI does not know, the ones that call a paid service).
    fn classes(&self, classes: &[String]) -> Result<(Vec<String>, Vec<String>), String>;
    /// Put a file in ComfyUI's input folder (never over one); the name it kept.
    fn upload(&self, name: &str, bytes: &[u8]) -> Result<String, String>;
    fn queue(&self, graph: &J, client: &str) -> Result<String, String>;
    /// Wait for the run to end. `tick` is called about once a second while waiting.
    fn wait(&self, id: &str, limit: Duration, tick: &mut dyn FnMut()) -> Result<Done, String>;
    fn fetch(&self, f: &OutFile) -> Result<Vec<u8>, String>;
}

pub struct HttpComfy {
    pub http: Http,
    pub poll: Duration,
}

impl HttpComfy {
    pub fn new(base: &str) -> Result<HttpComfy, String> {
        Ok(HttpComfy {
            http: Http::parse(base.trim_end_matches('/'))?,
            poll: Duration::from_millis(1000),
        })
    }

    fn json(&self, path: &str, secs: u64) -> Result<(u16, J), String> {
        let (c, b) = self.http.get(path, Duration::from_secs(secs))?;
        Ok((c, parse(&text(&b)).unwrap_or(J::Null)))
    }
}

/// What ComfyUI's refusal of a graph means, in words an owner can act on.
pub fn explain_refusal(j: &J) -> String {
    let msg = j
        .get("error")
        .and_then(|e| e.str("message"))
        .or_else(|| j.str("error"))
        .unwrap_or("ComfyUI refused the graph");
    let mut out = msg.to_string();
    if let Some(J::Obj(errs)) = j.get("node_errors") {
        for (node, e) in errs.iter().take(3) {
            let class = e.str("class_type").unwrap_or("a node");
            for r in e.arr("errors").iter().take(2) {
                let m = r.str("message").unwrap_or("");
                let d = r.str("details").unwrap_or("");
                let missing = m.starts_with("Value not in list");
                out.push_str(&format!(
                    "\n  node {node} ({class}): {m}{}{}",
                    if d.is_empty() { String::new() } else { format!(": {d}") },
                    if missing {
                        " (a model file ComfyUI cannot find: `tempo-launch stage check` lists what is missing)"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    out
}

impl Comfy for HttpComfy {
    fn base(&self) -> String {
        format!("http://{}:{}", self.http.host, self.http.port)
    }

    fn stats(&self) -> Result<Stats, String> {
        let (c, j) = self.json("/system_stats", 5)?;
        if c != 200 {
            return Err(format!("ComfyUI answered HTTP {c} to /system_stats"));
        }
        let dev = j.arr("devices");
        let best = dev.iter().max_by(|a, b| {
            a.f64("vram_total")
                .unwrap_or(0.0)
                .total_cmp(&b.f64("vram_total").unwrap_or(0.0))
        });
        let gb = |k: &str| best.and_then(|d| d.f64(k)).unwrap_or(0.0) / 1e9;
        Ok(Stats {
            gpu_gb: (gb("vram_total") * 10.0).round() / 10.0,
            free_gb: (gb("vram_free") * 10.0).round() / 10.0,
            version: j
                .get("system")
                .and_then(|s| s.str("comfyui_version"))
                .unwrap_or("")
                .to_string(),
        })
    }

    fn classes(&self, classes: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
        let (mut missing, mut api) = (vec![], vec![]);
        for c in classes {
            let (code, j) = self.json(&format!("/object_info/{}", enc(c)), 10)?;
            match (code, j.get(c)) {
                (200, Some(info)) => {
                    // ComfyUI marks the partner nodes that call a paid service
                    if info.bool("api_node") == Some(true) {
                        api.push(c.clone());
                    }
                }
                _ => missing.push(c.clone()),
            }
        }
        Ok((missing, api))
    }

    fn upload(&self, name: &str, bytes: &[u8]) -> Result<String, String> {
        let boundary = format!("----tempo{:x}", crate::unix_now());
        let body = multipart(
            &boundary,
            "image",
            name,
            bytes,
            &[("type", "input"), ("overwrite", "false")],
        );
        let (c, b) = self.http.request(
            "POST",
            "/upload/image",
            Some(&format!("multipart/form-data; boundary={boundary}")),
            &body,
            Duration::from_secs(600),
        )?;
        let j = parse(&text(&b)).unwrap_or(J::Null);
        if c != 200 {
            return Err(format!("ComfyUI would not take {name} (HTTP {c})"));
        }
        let kept = j.str("name").unwrap_or(name).to_string();
        let sub = j.str("subfolder").unwrap_or("");
        Ok(if sub.is_empty() {
            kept
        } else {
            format!("{sub}/{kept}")
        })
    }

    fn queue(&self, graph: &J, client: &str) -> Result<String, String> {
        let body = J::Obj(vec![
            ("prompt".into(), graph.clone()),
            ("client_id".into(), J::s(client)),
        ])
        .compact();
        let (c, b) = self
            .http
            .post_json("/prompt", &body, Duration::from_secs(60))?;
        let j = parse(&text(&b)).unwrap_or(J::Null);
        if c != 200 {
            return Err(explain_refusal(&j));
        }
        j.str("prompt_id")
            .map(String::from)
            .ok_or_else(|| "ComfyUI took the graph but gave no run id".into())
    }

    fn wait(&self, id: &str, limit: Duration, tick: &mut dyn FnMut()) -> Result<Done, String> {
        let start = Instant::now();
        loop {
            let (c, j) = self.json(&format!("/history/{}", enc(id)), 15)?;
            if c == 200 {
                if let Some(h) = j.get(id) {
                    return Ok(read_history(h));
                }
            }
            if start.elapsed() > limit {
                return Err(format!(
                    "the run did not finish in {} minutes; ComfyUI may still be working on it",
                    limit.as_secs() / 60
                ));
            }
            tick();
            std::thread::sleep(self.poll);
        }
    }

    fn fetch(&self, f: &OutFile) -> Result<Vec<u8>, String> {
        let (c, b) = self.http.get(
            &format!(
                "/view?filename={}&subfolder={}&type={}",
                enc(&f.name),
                enc(&f.subfolder),
                enc(&f.kind)
            ),
            Duration::from_secs(600),
        )?;
        if c != 200 {
            return Err(format!("ComfyUI would not give {} (HTTP {c})", f.name));
        }
        Ok(b)
    }
}

/// One run's entry in `/history`: its files, how much was reused, and what went wrong.
pub fn read_history(h: &J) -> Done {
    let mut d = Done::default();
    if let Some(J::Obj(outs)) = h.get("outputs") {
        for (node, o) in outs {
            // the key depends on the node: images, gifs (video combine), videos, audio
            for key in ["images", "gifs", "videos", "audio"] {
                for f in o.arr(key) {
                    if let Some(name) = f.str("filename") {
                        d.files.push(OutFile {
                            node: node.clone(),
                            name: name.to_string(),
                            subfolder: f.str("subfolder").unwrap_or("").to_string(),
                            kind: f.str("type").unwrap_or("output").to_string(),
                        });
                    }
                }
            }
        }
    }
    let st = h.get("status");
    for m in st.map(|s| s.arr("messages")).unwrap_or(&[]) {
        if let J::Arr(pair) = m {
            match (pair.first(), pair.get(1)) {
                (Some(J::Str(k)), Some(body)) if k == "execution_cached" => {
                    d.cached += body.arr("nodes").len();
                }
                (Some(J::Str(k)), Some(body)) if k == "execution_error" => {
                    d.error = Some(format!(
                        "{}{}",
                        body.str("exception_message")
                            .unwrap_or("ComfyUI stopped with an error")
                            .trim(),
                        body.str("node_type")
                            .map_or(String::new(), |n| format!(" (in {n})"))
                    ));
                }
                _ => {}
            }
        }
    }
    if d.error.is_none() && st.and_then(|s| s.str("status_str")) == Some("error") {
        d.error = Some("ComfyUI stopped with an error".into());
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_gives_files_cache_hits_and_errors() {
        let h = parse(
            r#"{"outputs":{"56":{"images":[{"filename":"a.mp4","subfolder":"stage","type":"output"}]},
                 "90":{"gifs":[{"filename":"b.mp4","subfolder":"","type":"output"}]}},
                "status":{"status_str":"success","messages":[["execution_start",{}],["execution_cached",{"nodes":["1","2","3"]}]]}}"#,
        )
        .unwrap();
        let d = read_history(&h);
        assert_eq!(d.files.len(), 2);
        assert_eq!(d.files[0].subfolder, "stage");
        assert_eq!(d.cached, 3);
        assert!(d.error.is_none());
        let bad = parse(r#"{"outputs":{},"status":{"status_str":"error","messages":[["execution_error",{"exception_message":"out of memory ","node_type":"KSampler"}]]}}"#).unwrap();
        assert_eq!(
            read_history(&bad).error.as_deref(),
            Some("out of memory (in KSampler)")
        );
    }

    #[test]
    fn a_refused_graph_is_explained() {
        let j = parse(
            r#"{"error":{"message":"Prompt outputs failed validation"},"node_errors":{"37":{"class_type":"UNETLoader","errors":[{"message":"Value not in list","details":"unet_name: 'x.safetensors' not in []"}]}}}"#,
        )
        .unwrap();
        let e = explain_refusal(&j);
        assert!(
            e.contains("node 37 (UNETLoader)") && e.contains("model file ComfyUI cannot find"),
            "{e}"
        );
    }
}
