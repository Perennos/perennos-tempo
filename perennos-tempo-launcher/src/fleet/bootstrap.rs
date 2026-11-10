//! A read-only plan, then an explicitly approved link to a computer with Tempo already installed.

use super::first_line;
use super::pair;
use super::scan;
use super::ssh;
use super::store::{self, Fleet, Machine, Paths};
use super::{controls, crypto};
use crate::setup::json::parse as parse_json;
use crate::updates::Version;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

const BEGIN: &str = "__TEMPO_BOOTSTRAP_BEGIN__";
const END: &str = "__TEMPO_BOOTSTRAP_END__";
const FLEET_SELF_KEY: &str = "self";
const PROBE: &str = r#"printf '__TEMPO_BOOTSTRAP_BEGIN__\n'
printf 'os=%s\n' "$(uname -s 2>/dev/null)"
printf 'arch=%s\n' "$(uname -m 2>/dev/null)"
printf 'name=%s\n' "$(hostname 2>/dev/null)"
printf 'user=%s\n' "$(id -un)"
printf 'user_home=%s\n' "$HOME"
launcher=$(command -v tempo-launch 2>/dev/null)
if [ -z "$launcher" ] && [ -x "$HOME/.local/bin/tempo-launch" ]; then launcher="$HOME/.local/bin/tempo-launch"; fi
if [ -z "$launcher" ]; then
  case "$(uname -s)-$(uname -m)" in Linux-x86_64) target=x86_64-unknown-linux-gnu;; Darwin-arm64) target=aarch64-apple-darwin;; *) target=unsupported;; esac
  candidate="$HOME/.perennos-tempo-controls/__VERSION__-$target/bin/tempo-launch"
  if [ -x "$candidate" ]; then launcher=$candidate; fi
fi
printf 'launcher=%s\n' "$launcher"
if [ -n "$launcher" ]; then
  printf 'version=%s\n' "$("$launcher" version 2>/dev/null | head -n 1)"
  printf 'home_json=%s\n' "$("$launcher" home --json 2>/dev/null)"
fi
legacy=${TEMPO_LEGACY_HOME:-${XDG_STATE_HOME:-$HOME/.local/state}/perennos-tempo}
home=${TEMPO_HOME:-${PERENNOS_TEMPO_DATA:-}}
if [ -z "$home" ]; then if [ -d "$legacy" ]; then home=$legacy; else home=$HOME/.tempo; fi; fi
printf 'default_home=%s\n' "$home"
printf 'fleet_port=%s\n' "${TEMPO_FLEET_PORT:-8770}"
printf '__TEMPO_BOOTSTRAP_END__\n'
"#;

const WINDOWS_PROBE: &str = r#"$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)
function tempoHash($p){$h=[Security.Cryptography.SHA256]::Create();$s=[IO.File]::OpenRead($p);try{[BitConverter]::ToString($h.ComputeHash($s)).Replace('-','').ToLowerInvariant()}finally{$s.Dispose();$h.Dispose()}}
'__TEMPO_BOOTSTRAP_BEGIN__'
'os=Windows'
'arch='+$env:PROCESSOR_ARCHITECTURE
'name='+$env:COMPUTERNAME
'user='+[Security.Principal.WindowsIdentity]::GetCurrent().Name
'user_home='+$env:USERPROFILE
$cmd=Get-Command tempo-launch.exe -ErrorAction SilentlyContinue
$bin=if($cmd){$cmd.Source}else{''}
if(!$bin){$candidate=Join-Path $env:USERPROFILE '.perennos-tempo-controls/__VERSION__-x86_64-pc-windows-msvc/bin/tempo-launch.exe'; if(Test-Path -LiteralPath $candidate){$bin=$candidate}}
'launcher='+$bin
$homeDir=if($env:TEMPO_HOME){$env:TEMPO_HOME}else{Join-Path $env:LOCALAPPDATA 'Perennos Tempo'}
'default_home='+$homeDir
if($bin){'version='+(& $bin version | Select-Object -First 1); $j=& $bin home --json; 'home_json='+$j; $homeDir=($j|ConvertFrom-Json).home}
'fleet_port='+$(if($env:TEMPO_FLEET_PORT){$env:TEMPO_FLEET_PORT}else{'8770'})
'__TEMPO_BOOTSTRAP_END__'
"#;

// OpenSSH's Windows shell quoting is avoided with the native UTF-16 encoded-command format.
fn ps_command(script: &str) -> Vec<String> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut encoded = String::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        encoded.push(TABLE[(n >> 18) as usize & 63] as char);
        encoded.push(TABLE[(n >> 12) as usize & 63] as char);
        encoded.push(if c.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        encoded.push(if c.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    vec![
        "powershell.exe".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-EncodedCommand".into(),
        encoded,
    ]
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn controls_root(plan: &Plan) -> String {
    let target = controls::target(&plan.os, &plan.arch).unwrap_or("unsupported");
    let version = crate::updates::current_version().tag();
    format!(
        "{}/.perennos-tempo-controls/{version}-{target}",
        plan.user_home.trim_end_matches(['/', '\\'])
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub target: String,
    pub name: String,
    pub private_addr: String,
    pub port: u16,
    pub fleet_port: u16,
    pub controller_port: u16,
    pub installed_version: Version,
    pub home: String,
    pub launcher: String,
    pub os: String,
    pub arch: String,
    pub user: String,
    pub user_home: String,
    pub key_hash: String,
    pub fleet_json: String,
    pub artifact: Option<controls::Artifact>,
    id: String,
}

impl Plan {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn approval(&self) -> Approval {
        Approval(self.id.clone())
    }

    pub fn summary(&self) -> crate::setup::json::J {
        use crate::setup::json::J;
        J::Obj(vec![
            ("name".into(), J::s(&self.name)), ("target".into(), J::s(&self.target)),
            ("address".into(), J::s(&format!("{}:{}", self.private_addr, self.port))),
            ("platform".into(), J::s(&format!("{} {}", self.os, self.arch))),
            ("home".into(), J::s(&self.home)), ("launcher".into(), J::s(&self.launcher)),
            ("version".into(), J::s(&self.installed_version.tag())),
            ("controller_port".into(), J::n(f64::from(self.controller_port))),
            ("download_bytes".into(), J::n(self.artifact.as_ref().map_or(0, |a| a.size) as f64)),
            ("sha256".into(), J::s(self.artifact.as_ref().map_or("", |a| a.sha256.as_str()))),
            ("changes".into(), J::s("Install missing controls, start this computer's messenger for this session if needed, pair with this fleet, and enable a per-user background messenger on the other computer. Models and their settings stay as they are.")),
        ])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval(String);

pub trait Actions {
    fn recheck(&mut self, plan: &Plan) -> Result<(), String>;
    fn install(&mut self, _plan: &Plan, _progress: &mut dyn FnMut(&str)) -> Result<(), String> {
        Ok(())
    }
    fn controller(&mut self, _plan: &Plan) -> Result<(), String> {
        Ok(())
    }
    fn pair(&mut self, plan: &Plan) -> Result<String, String>;
    fn start_messenger(&mut self, plan: &Plan) -> Result<(), String>;
    fn register(&mut self, plan: &Plan, identity: &str) -> Result<(), String>;
}

pub fn prepare(target: &str) -> Result<Plan, String> {
    probe(target)
}

pub fn apply(p: &Paths, plan: &Plan, approval: Option<&Approval>) -> Result<(), String> {
    apply_progress(p, plan, approval, &mut |_| {})
}

pub fn apply_progress(
    p: &Paths,
    plan: &Plan,
    approval: Option<&Approval>,
    progress: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let mut native = Native { paths: p };
    apply_steps(plan, approval, &mut native, progress)
}

pub fn apply_with(
    plan: &Plan,
    approval: Option<&Approval>,
    actions: &mut dyn Actions,
) -> Result<(), String> {
    apply_steps(plan, approval, actions, &mut |_| {})
}

fn apply_steps(
    plan: &Plan,
    approval: Option<&Approval>,
    actions: &mut dyn Actions,
    progress: &mut dyn FnMut(&str),
) -> Result<(), String> {
    if plan_id(plan) != plan.id || !approval.is_some_and(|a| a.0 == plan.id) {
        return Err("Nothing was changed; approval did not match this plan.".into());
    }
    progress("Rechecking the approved computer");
    actions.recheck(plan)?;
    actions.install(plan, progress)?;
    progress("Starting this computer's messenger");
    actions.controller(plan)?;
    progress("Pairing");
    let identity = actions.pair(plan)?;
    progress("Starting the background messenger");
    actions.start_messenger(plan)?;
    progress("Checking the signed connection");
    actions.register(plan, &identity)?;
    progress("Connected");
    Ok(())
}

fn probe(target: &str) -> Result<Plan, String> {
    if !ssh::target_ok(target) {
        return Err("That is not a valid SSH host name or private address.".into());
    }
    let (dest, port) = ssh::private_target(target)?;
    let destination = dest
        .parse::<IpAddr>()
        .map_err(|_| "SSH returned an invalid private address")?;
    if destination.is_loopback()
        || matches!(destination, IpAddr::V4(ip) if scan::local_addrs().contains(&ip))
    {
        return Err(
            "The SSH destination is this computer. Choose a different private node.".into(),
        );
    }
    let args = ssh::strict_args_for(target, &dest, port, &["sh".into(), "-s".into()])?;
    let mut said = ssh::run(
        &args,
        Some(&PROBE.replace("__VERSION__", &crate::updates::current_version().tag())),
        Duration::from_secs(15),
    )
    .map_err(|e| ssh_problem(&e))?;
    if !said.ok {
        // OpenSSH on Windows has no POSIX shell. Probe its native shell without weakening SSH.
        if said.err.to_ascii_lowercase().contains("permission denied")
            || said.err.to_ascii_lowercase().contains("host key")
        {
            return Err(ssh_problem(&said.err));
        }
        let args = ssh::strict_args_for(
            target,
            &dest,
            port,
            &ps_command(
                &WINDOWS_PROBE.replace("__VERSION__", &crate::updates::current_version().tag()),
            ),
        )?;
        said = ssh::run(&args, None, Duration::from_secs(15)).map_err(|e| ssh_problem(&e))?;
        if !said.ok {
            return Err(ssh_problem(&said.err));
        }
    }
    let mut plan = parse_probe(&said.out, target, &dest, port)?;
    // The launcher's JSON home may contain escaped characters. Read identity facts at the
    // decoded path, not at a path reconstructed by a shell text substitution.
    let (key_hash, fleet_json) = read_fleet_state(&plan)?;
    plan.key_hash = key_hash;
    plan.fleet_json = fleet_json;
    if !plan.fleet_json.is_empty() {
        let json = parse_json(&plan.fleet_json)
            .map_err(|_| "The existing fleet metadata could not be read.")?;
        if let Some(name) = json.str(FLEET_SELF_KEY).filter(|n| !n.is_empty()) {
            plan.name = store::clean_name(name);
        }
    }
    if plan.launcher.is_empty() {
        let fetch = crate::setup::registry::real_fetcher(&crate::setup::probe::RealHost)
            .ok_or("curl is needed to check the official controls release.")?;
        plan.artifact = Some(controls::release(
            fetch.as_ref(),
            crate::updates::current_version(),
            controls::target(&plan.os, &plan.arch)?,
        )?);
        let root = controls_root(&plan);
        plan.launcher = format!(
            "{root}/bin/tempo-launch{}",
            if plan.os == "Windows" { ".exe" } else { "" }
        );
    }
    if plan.artifact.is_none() && plan.controller_port != store::FLEET_PORT {
        return Err("This existing launcher cannot prove support for a custom controller fleet port. Use the default controller port or upgrade its controls separately; nothing was changed.".into());
    }
    plan.id = plan_id(&plan);
    Ok(plan)
}

fn read_fleet_state(plan: &Plan) -> Result<(String, String), String> {
    let key = format!("{}/fleet.key", plan.home.trim_end_matches(['/', '\\']));
    let fleet = format!("{}/fleet.json", plan.home.trim_end_matches(['/', '\\']));
    let (command, script) = if plan.os == "Windows" {
        let hash = WINDOWS_PROBE
            .lines()
            .find(|line| line.starts_with("function tempoHash("))
            .ok_or("Windows SHA-256 helper is missing.")?;
        let script = format!("$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); {hash}; $k={}; $f={}; foreach($p in @($k,$f)){{$item=Get-Item -Force -LiteralPath $p -ErrorAction SilentlyContinue; if($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)){{throw 'Fleet path is a reparse point'}}}}; if(Test-Path -LiteralPath $k){{if((Get-Item -LiteralPath $k).Length -ne 32){{throw 'Fleet key has invalid length'}}; 'key_hash='+(tempoHash $k)}}else{{'key_hash='}}; if(Test-Path -LiteralPath $f){{$stream=[IO.File]::OpenRead($f); try{{$buf=New-Object byte[] 262145; $used=0; while($used -lt $buf.Length){{$n=$stream.Read($buf,$used,$buf.Length-$used); if($n -eq 0){{break}}; $used+=$n}}; if($used -gt 262144){{throw 'Fleet metadata is too large'}}; 'fleet_hex='+[BitConverter]::ToString($buf,0,$used).Replace('-','').ToLowerInvariant()}}finally{{$stream.Dispose()}}}}else{{'fleet_hex=__TEMPO_ABSENT__'}}", ps_quote(&key), ps_quote(&fleet));
        (ps_command(&script), None)
    } else {
        let script = format!("set -eu\nk={}\nf={}\nprintf 'key_hash='\nif [ -e \"$k\" ]; then test -f \"$k\" && test ! -L \"$k\" && test \"$(wc -c < \"$k\")\" -eq 32 || exit 43; if command -v sha256sum >/dev/null; then sha256sum < \"$k\" | cut -d' ' -f1; else shasum -a 256 < \"$k\" | cut -d' ' -f1; fi; else printf '\\n'; fi\nprintf 'fleet_hex='\nif [ -e \"$f\" ]; then test -f \"$f\" && test ! -L \"$f\" && test \"$(wc -c < \"$f\")\" -le 262144 || exit 44; head -c 262145 \"$f\" | od -An -v -tx1 | tr -d ' \\r\\n'; else printf '__TEMPO_ABSENT__'; fi\nprintf '\\n'\n", sh_quote(&key), sh_quote(&fleet));
        (vec!["sh".into(), "-s".into()], Some(script))
    };
    let args = ssh::strict_args_for(&plan.target, &plan.private_addr, plan.port, &command)?;
    let said =
        ssh::run(&args, script.as_deref(), Duration::from_secs(10)).map_err(|e| ssh_problem(&e))?;
    if !said.ok {
        return Err(
            "Could not read the target's existing fleet identity; nothing was changed.".into(),
        );
    }
    let mut key_hash = None;
    let mut fleet_hex = None;
    for line in said.out.lines() {
        if let Some(v) = line.strip_prefix("key_hash=") {
            if key_hash.replace(v.to_string()).is_some() {
                return Err("Repeated fleet identity field.".into());
            }
        }
        if let Some(v) = line.strip_prefix("fleet_hex=") {
            if fleet_hex.replace(v.to_string()).is_some() {
                return Err("Repeated fleet metadata field.".into());
            }
        }
    }
    let fleet_hex = fleet_hex.ok_or("The fleet metadata is missing.")?;
    let fleet_json = if fleet_hex == "__TEMPO_ABSENT__" {
        String::new()
    } else {
        let bytes =
            crypto::unhex(&fleet_hex).ok_or("The target's fleet metadata encoding is invalid.")?;
        if bytes.len() > 262144 {
            return Err("The target's fleet metadata exceeded its read limit.".into());
        }
        let fleet_json =
            String::from_utf8(bytes).map_err(|_| "The target's fleet metadata is not UTF-8.")?;
        let json = store::parse_registry_text(&fleet_json)?;
        store::validate_registry(&json)?;
        fleet_json
    };
    Ok((
        key_hash.ok_or("The fleet key fingerprint is missing.")?,
        fleet_json,
    ))
}

fn parse_probe(text: &str, target: &str, dest: &str, port: u16) -> Result<Plan, String> {
    let body = text
        .split_once(BEGIN)
        .and_then(|(_, s)| s.split_once(END).map(|x| x.0))
        .ok_or("The SSH probe ended early; no changes were made.")?;
    let mut v = HashMap::new();
    for (key, value) in body.lines().filter_map(|line| line.trim().split_once('=')) {
        if v.insert(key, value).is_some() {
            return Err("The SSH probe repeated a field; no changes were made.".into());
        }
    }
    let get = |key| v.get(key).copied().unwrap_or("");
    let (os, arch) = (get("os"), get("arch"));
    controls::target(os, arch)?;
    let launcher = get("launcher");
    let version = if launcher.is_empty() {
        crate::updates::current_version()
    } else {
        parse_version(get("version"))
            .ok_or("The existing launcher's version could not be read; no changes were made.")?
    };
    if !launcher.is_empty() && version < Version([2, 1, 0]) {
        return Err("The existing controls predate fleet enrollment. Upgrade that installation separately; it was not replaced.".into());
    }
    let user_home = get("user_home");
    let user = get("user");
    if user_home.is_empty()
        || user.is_empty()
        || user_home.chars().any(char::is_control)
        || user.chars().any(char::is_control)
    {
        return Err("The remote user and home could not be bound to approval.".into());
    }
    let home = if launcher.is_empty() {
        get("default_home").to_string()
    } else {
        parse_json(get("home_json"))
        .ok()
        .and_then(|j| j.str("home").filter(|s| !s.is_empty()).map(str::to_string))
        .ok_or(
            "tempo-launch home --json did not return the active Tempo home; no changes were made.",
        )?
    };
    if home.is_empty() || home.chars().any(char::is_control) {
        return Err(
            "The active Tempo home contains a control character; no changes were made.".into(),
        );
    }
    let name = store::clean_name(get("name"));
    Ok(Plan {
        target: target.into(),
        name: if name.is_empty() {
            store::clean_name(target)
        } else {
            name
        },
        private_addr: dest.into(),
        port,
        fleet_port: get("fleet_port")
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or("The SSH probe returned an invalid fleet port.")?,
        controller_port: store::fleet_port(),
        installed_version: version,
        home,
        launcher: launcher.into(),
        os: os.into(),
        arch: arch.into(),
        user: user.into(),
        user_home: user_home.into(),
        key_hash: get("key_hash").into(),
        fleet_json: get("fleet_json").into(),
        artifact: None,
        id: String::new(),
    })
}

fn parse_version(s: &str) -> Option<Version> {
    s.split_whitespace()
        .find_map(|part| Version::parse(&format!("v{}", part.trim_start_matches('v'))))
}

fn plan_id(plan: &Plan) -> String {
    let material = format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
        plan.target,
        plan.name,
        plan.private_addr,
        plan.port,
        plan.fleet_port,
        plan.installed_version.tag(),
        plan.home,
        plan.launcher
    );
    let mut h = crate::setup::sha256::Sha256::new();
    h.update(material.as_bytes());
    h.update(
        format!(
            "\0{:?}",
            (
                &plan.os,
                &plan.arch,
                plan.controller_port,
                &plan.user,
                &plan.user_home,
                &plan.key_hash,
                &plan.fleet_json,
                &plan.artifact
            )
        )
        .as_bytes(),
    );
    h.finish().iter().map(|b| format!("{b:02x}")).collect()
}

fn ssh_problem(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    if lower.contains("host key verification failed") || lower.contains("unknown host") {
        return "SSH does not trust this host key yet. Check its fingerprint with the computer's owner, add it to known_hosts, then prepare again. Nothing was sent.".into();
    }
    if lower.contains("remote host identification has changed") {
        return "The SSH host key changed. Check it with the computer's owner; no script or key was sent.".into();
    }
    if lower.contains("permission denied") {
        return "SSH key or agent authentication is missing. Add your own key or agent access, then prepare again. Tempo never asks for a password.".into();
    }
    format!(
        "SSH could not read the target: {}. No script or key was sent.",
        first_line(s)
    )
}

struct Native<'a> {
    paths: &'a Paths,
}

impl Native<'_> {
    fn remote(&self, plan: &Plan, script: &str, seconds: u64) -> Result<ssh::Said, String> {
        if plan.os == "Windows" {
            let args = ssh::strict_args_for(
                &plan.target,
                &plan.private_addr,
                plan.port,
                &ps_command(script),
            )?;
            return ssh::run(&args, None, Duration::from_secs(seconds));
        }
        let args = ssh::strict_args_for(
            &plan.target,
            &plan.private_addr,
            plan.port,
            &["sh".into(), "-s".into()],
        )?;
        ssh::run(&args, Some(script), Duration::from_secs(seconds)).map_err(|e| ssh_problem(&e))
    }

    fn input(&self, plan: &Plan, script: &str, bytes: &[u8]) -> Result<ssh::Said, String> {
        let command = if plan.os == "Windows" {
            ps_command(script)
        } else {
            vec!["sh".into(), "-c".into(), sh_quote(script)]
        };
        let args = ssh::strict_args_for(&plan.target, &plan.private_addr, plan.port, &command)?;
        ssh::run_bytes(&args, bytes, Duration::from_secs(45))
    }
}

impl Actions for Native<'_> {
    fn recheck(&mut self, plan: &Plan) -> Result<(), String> {
        check_local_key(self.paths)?;
        let now = probe(&plan.target)?;
        if now != *plan {
            return Err(
                "The target or installed launcher changed after this plan. Prepare again.".into(),
            );
        }
        check_existing_fleet(self.paths, plan)?;
        let fleet = store::load(self.paths);
        if let Some(i) = fleet
            .find(&plan.name)
            .or_else(|| fleet.find(&plan.private_addr))
        {
            let pinned = &fleet.machines[i];
            if !pinned.id.is_empty() {
                signed_probe(self.paths, plan, &pinned.id)?;
            }
        }
        Ok(())
    }

    fn install(&mut self, plan: &Plan, progress: &mut dyn FnMut(&str)) -> Result<(), String> {
        let Some(artifact) = &plan.artifact else {
            return Ok(());
        };
        progress("Downloading controls");
        let staging = self
            .paths
            .file(&format!("controls-cache/{}", artifact.sha256));
        for path in [&self.paths.file("controls-cache"), &staging] {
            match std::fs::symlink_metadata(path) {
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(path)
                    .map_err(|e| format!("Could not create controls staging: {e}"))?,
                Err(e) => return Err(e.to_string()),
            }
            let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
            if meta.file_type().is_symlink() {
                return Err("The controls cache must not be a link.".into());
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    return Err("The controls cache must not be a reparse point.".into());
                }
            }
        }
        let archive = staging.join(&artifact.name);
        if !archive.exists() {
            let part = staging.join(format!("{}.part", crypto::hex(&crypto::random(12))));
            controls::download(artifact, &part)?;
            std::fs::rename(&part, &archive)
                .map_err(|e| format!("Could not retain verified controls: {e}"))?;
        }
        progress("Verifying controls");
        let files = controls::inspect_bundle(artifact, &archive)?;
        // The read-only target facts must still match after a slow network download.
        self.recheck(plan)?;
        let stage = format!(
            "{}.stage-{}",
            controls_root(plan),
            crypto::hex(&crypto::random(12))
        );
        progress("Installing controls");
        let said = self.remote(plan, &stage_script(plan, &stage), 15)?;
        if !said.ok {
            return Err("Controls staging was refused; the existing installation was kept.".into());
        }
        for file in &files {
            let said = self.input(plan, &transfer_script(plan, &stage, file), &file.bytes)?;
            if !said.ok {
                return Err(format!("Controls transfer stopped at {}; staging is retained at {stage}. Prepare again to retry.", file.path));
            }
        }
        let said = self.remote(plan, &promote_script(plan, &stage), 20)?;
        if !said.ok {
            return Err(format!("Controls promotion or native version check failed; inspect retained staging {stage} and versioned controls {} before retrying.", controls_root(plan)));
        }
        if plan.os != "Windows"
            && parse_json(said.out.trim())
                .ok()
                .and_then(|j| j.str("home").map(str::to_string))
                .as_deref()
                != Some(plan.home.as_str())
        {
            return Err("The installed controls reported a different native home; no pairing was attempted.".into());
        }
        Ok(())
    }

    fn controller(&mut self, _: &Plan) -> Result<(), String> {
        check_local_key(self.paths)?;
        initialize_local_key(self.paths)?;
        pair::identity(self.paths)
            .map_err(|e| format!("Could not read the local fleet identity: {e}"))?;
        if !super::ensure_invite_messenger(store::fleet_port()) {
            return Err("This computer's fleet messenger did not start. Check its fleet port and prepare again; any staged remote controls were kept.".into());
        }
        Ok(())
    }

    fn pair(&mut self, plan: &Plan) -> Result<String, String> {
        let key = store::read_key(self.paths).ok_or("Could not read the local fleet key.")?;
        let own_id = pair::identity(self.paths)
            .map_err(|e| format!("Could not read the local fleet identity: {e}"))?;
        let local_addr = scan::route_address(&plan.private_addr)?;
        let me = store::load(self.paths).me;
        let endpoint = callback_endpoint(&local_addr, plan.controller_port);
        let peer = format!(
            "{}={}#{}",
            me,
            endpoint,
            crypto::hex(own_id.verifying_key().as_bytes())
        );
        let script = if plan.os == "Windows" {
            format!("$ErrorActionPreference='Stop'; $env:TEMPO_HOME={}; [Console]::In.ReadToEnd() | & {} fleet install-key --name {} --peer {}; exit $LASTEXITCODE", ps_quote(&plan.home), ps_quote(&plan.launcher), ps_quote(&plan.name), ps_quote(&peer))
        } else {
            format!(
                "TEMPO_HOME={} {} fleet install-key --name {} --peer {}\n",
                sh_quote(&plan.home),
                sh_quote(&plan.launcher),
                sh_quote(&plan.name),
                sh_quote(&peer),
            )
        };
        let said = self.input(plan, &script, format!("{}\n", crypto::hex(&key)).as_bytes())?;
        if !said.ok {
            return Err(format!("Pairing did not finish: {}", first_line(&said.err)));
        }
        said.out
            .lines()
            .find_map(|l| l.strip_prefix("identity="))
            .filter(|id| id.len() == 64 && crypto::unhex(id).is_some())
            .map(str::to_string)
            .ok_or_else(|| {
                "Pairing did not return the target identity; nothing was registered locally."
                    .to_string()
            })
    }

    fn start_messenger(&mut self, plan: &Plan) -> Result<(), String> {
        let script = persistence_script(plan)?;
        let said = self.remote(plan, &script, 30)?;
        if said.ok && said.out.contains("messenger=ready") {
            Ok(())
        } else {
            Err(format!(
                "The key was installed, but background persistence did not verify: {}",
                first_line(&said.err)
            ))
        }
    }

    fn register(&mut self, plan: &Plan, identity: &str) -> Result<(), String> {
        if identity.len() != 64 || crypto::unhex(identity).is_none() {
            return Err("The target identity was invalid; it was not added.".into());
        }
        signed_probe(self.paths, plan, identity).map_err(|e| format!("The messenger started, but its signed reply failed: {e}. Nothing was registered locally."))?;
        let registered_at = store::now_s();
        let before = store::load_checked(self.paths)?;
        let prior = before
            .find(&plan.name)
            .or_else(|| before.find(&plan.private_addr))
            .map(|i| before.machines[i].clone());
        store::update_checked(self.paths, |f| {
            let current = f
                .find(&plan.name)
                .or_else(|| f.find(&plan.private_addr))
                .map(|i| &f.machines[i]);
            if current != prior.as_ref() {
                return Err("the existing fleet entry changed after it was checked".into());
            }
            register_member(f, plan, identity)
        })
        .map_err(|e| {
            format!("The target paired and is running, but its fleet entry could not be saved: {e}")
        })?;
        let deadline = std::time::Instant::now() + Duration::from_secs(40);
        while std::time::Instant::now() < deadline {
            let fleet = store::load(self.paths);
            if fleet
                .find(&plan.name)
                .map(|i| &fleet.machines[i])
                .is_some_and(|m| {
                    m.id == identity
                        && m.last_seen > registered_at
                        && m.health != crate::setup::json::J::Null
                })
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let restored = store::update_checked(self.paths, |f| {
            Ok(rollback_member(f, plan, identity, prior.as_ref()))
        });
        match restored {
            Ok(true) => Err("Fresh signed health did not arrive. The local fleet entry was restored; the remote controls and service were retained for review.".into()),
            Ok(false) => Err("Fresh signed health did not arrive. The local entry changed concurrently and was kept for review; the remote controls and service were retained.".into()),
            Err(e) => Err(format!("Fresh signed health did not arrive and the local entry could not be restored: {e}. Remote controls and service were retained.")),
        }
    }
}

// Restore only the three fields enrollment owns, and only while its exact postimage remains.
fn rollback_member(f: &mut Fleet, plan: &Plan, identity: &str, prior: Option<&Machine>) -> bool {
    let Some(i) = f.find(&plan.name).or_else(|| f.find(&plan.private_addr)) else {
        return false;
    };
    let member = &f.machines[i];
    if member.id != identity || member.addr != plan.private_addr || member.port != plan.fleet_port {
        return false;
    }
    if let Some(old) = prior {
        if member.name != old.name {
            return false;
        }
        f.machines[i].id = old.id.clone();
        f.machines[i].addr = old.addr.clone();
        f.machines[i].port = old.port;
    } else {
        let mut expected = Machine::new(&plan.name, &plan.private_addr, "agent");
        expected.id = identity.into();
        expected.port = plan.fleet_port;
        if *member != expected {
            return false;
        }
        f.machines.remove(i);
    }
    true
}

fn register_member(f: &mut Fleet, plan: &Plan, identity: &str) -> Result<(), String> {
    if let Some(i) = f.find(&plan.name).or_else(|| f.find(&plan.private_addr)) {
        if !f.machines[i].id.is_empty() && f.machines[i].id != identity {
            return Err(format!(
                "identity collision for {} at {}; the existing pin was left unchanged",
                plan.name, f.machines[i].addr
            ));
        }
        f.machines[i].id = identity.into();
        f.machines[i].addr = plan.private_addr.clone();
        f.machines[i].port = plan.fleet_port;
    } else {
        let mut m = Machine::new(&plan.name, &plan.private_addr, "agent");
        m.id = identity.into();
        m.port = plan.fleet_port;
        f.machines.push(m);
    }
    Ok(())
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn callback_endpoint(addr: &str, port: u16) -> String {
    if port == store::FLEET_PORT {
        addr.to_string()
    } else if addr.contains(':') {
        format!("[{addr}]:{port}")
    } else {
        format!("{addr}:{port}")
    }
}

// A new member has no registry pin yet. Use the existing signed wire format with the exact
// approved peer pin, rather than temporarily trusting it in the live registry to test it.
fn signed_probe(paths: &Paths, plan: &Plan, identity: &str) -> Result<(), String> {
    use super::msg::{Frame, Guard};
    use std::io::{Read, Write};
    let raw: [u8; 32] = crypto::unhex(identity)
        .and_then(|v| v.try_into().ok())
        .ok_or("The peer identity is invalid.")?;
    let verifying = ed25519_dalek::VerifyingKey::from_bytes(&raw)
        .map_err(|_| "The peer identity is invalid.")?;
    let key = store::read_key(paths).ok_or("The local fleet key is unavailable.")?;
    let own = pair::identity(paths)?;
    let frame = Frame {
        kind: 'p',
        id: crypto::hex(&crypto::random(24)),
        from: store::load(paths).me,
        to: plan.name.clone(),
        ts: store::now_ms(),
        nonce: crypto::hex(&crypto::random(24)),
        ..Frame::default()
    };
    let addr = std::net::SocketAddr::new(
        plan.private_addr
            .parse()
            .map_err(|_| "Invalid private peer address.")?,
        plan.fleet_port,
    );
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(&super::msg::encode(&key, &own, &frame))
        .map_err(|e| e.to_string())?;
    struct Deadline {
        stream: std::net::TcpStream,
        end: std::time::Instant,
    }
    impl Read for Deadline {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let left = self
                .end
                .saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            self.stream.set_read_timeout(Some(left))?;
            self.stream.read(bytes)
        }
    }
    let mut reader = Deadline {
        stream,
        end: std::time::Instant::now() + Duration::from_secs(5),
    };
    let reply = super::msg::read_frame(&mut reader, &key, &|name| {
        (name == plan.name).then_some(verifying)
    })
    .map_err(|e| format!("{e:?}"))?;
    verify_probe_reply(&frame, &reply, &mut Guard::default(), store::now_ms())
}

fn verify_probe_reply(
    request: &super::msg::Frame,
    reply: &super::msg::Frame,
    guard: &mut super::msg::Guard,
    now: u64,
) -> Result<(), String> {
    if reply.kind != 'o'
        || reply.from != request.to
        || reply.to != request.from
        || reply.id != request.id
        || !reply.body.is_empty()
    {
        return Err("The signed reply did not match this peer and request.".into());
    }
    guard
        .check(reply.ts, &reply.nonce, now)
        .map_err(str::to_string)
}

fn check_existing_fleet(paths: &Paths, plan: &Plan) -> Result<(), String> {
    let local = store::load_checked(paths)?;
    check_member_name(&local, plan)?;
    if !plan.key_hash.is_empty() {
        let local = store::read_key(paths).map(|k| crate::setup::sha256::sha256_hex(&k));
        if local.as_deref() != Some(plan.key_hash.as_str()) {
            return Err("This computer belongs to a different fleet. Its key and pins were kept; migration needs a separate plan.".into());
        }
    }
    if !plan.fleet_json.is_empty() {
        let json =
            parse_json(&plan.fleet_json).map_err(|_| "The target's fleet metadata is invalid.")?;
        let fleet = Fleet::from_j(&json);
        let own = store::load(paths);
        if let Some(m) = fleet
            .find(&own.me)
            .map(|i| &fleet.machines[i])
            .filter(|m| !m.id.is_empty())
        {
            let seed: [u8; 32] = std::fs::read(paths.file("fleet-identity"))
                .ok()
                .and_then(|v| v.try_into().ok())
                .ok_or("The remote pin has no matching local identity. Nothing was changed.")?;
            let id = ed25519_dalek::SigningKey::from_bytes(&seed);
            if m.id != crypto::hex(id.verifying_key().as_bytes()) {
                return Err(
                    "The target pins a different controller identity. Nothing was changed.".into(),
                );
            }
        }
    }
    Ok(())
}

fn check_local_key(paths: &Paths) -> Result<(), String> {
    let path = paths.file("fleet.key");
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("Could not inspect this computer's fleet key: {e}")),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() != 32 {
        return Err("This computer's existing fleet key is invalid or not a regular file; it was kept unchanged.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(
                "This computer's fleet key is a reparse point; it was kept unchanged.".into(),
            );
        }
    }
    if store::read_key(paths).is_none() {
        return Err(
            "This computer's existing fleet key cannot be read; it was kept unchanged.".into(),
        );
    }
    Ok(())
}

// Create the final name exclusively: a key appearing after preflight is never replaced.
fn initialize_local_key(paths: &Paths) -> Result<bool, String> {
    use std::io::Write;
    std::fs::create_dir_all(&paths.home)
        .map_err(|e| format!("Could not open the fleet home: {e}"))?;
    let path = paths.file("fleet.key");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            check_local_key(paths)?;
            return Ok(false);
        }
        Err(e) => return Err(format!("Could not create the local fleet key: {e}")),
    };
    #[cfg(windows)]
    {
        let system = std::env::var_os("SystemRoot")
            .ok_or("Windows SystemRoot is unavailable; the empty key file was kept for review.")?;
        let system = std::path::PathBuf::from(system).join("System32");
        let who = system.join("whoami.exe");
        let who = who.to_str().ok_or("Cannot read the Windows system path.")?;
        let identity = crate::setup::probe::run_with_timeout(
            who,
            &["/user", "/fo", "csv", "/nh"],
            Duration::from_secs(5),
        )
        .ok_or("Could not identify the Windows owner; the empty key file was kept for review.")?;
        let sid = identity
            .split(',')
            .nth(1)
            .unwrap_or("")
            .trim()
            .trim_matches('"');
        if !sid.starts_with("S-1-")
            || !sid
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'-' || b == b'S')
        {
            return Err(
                "The Windows owner SID was invalid; the empty key file was kept for review.".into(),
            );
        }
        let acl = system.join("icacls.exe");
        let acl = acl.to_str().ok_or("Cannot read the Windows system path.")?;
        let filename = path
            .to_str()
            .ok_or("The fleet key path is not valid Unicode.")?;
        let grant = format!("*{sid}:F");
        if crate::setup::probe::run_with_timeout(
            acl,
            &[filename, "/inheritance:r", "/grant:r", &grant],
            Duration::from_secs(5),
        )
        .is_none()
        {
            return Err(
                "Could not protect the Windows fleet key; the empty key file was kept for review."
                    .into(),
            );
        }
    }
    let mut key = crypto::random(32);
    let result = file.write_all(&key).and_then(|()| file.sync_all());
    key.fill(0);
    result.map_err(|e| {
        format!("Could not finish the local fleet key; the partial file was kept for review: {e}")
    })?;
    Ok(true)
}

fn check_member_name(fleet: &Fleet, plan: &Plan) -> Result<(), String> {
    if let Some(i) = fleet.find(&plan.private_addr) {
        if fleet.machines[i].name != plan.name {
            return Err("This address is already registered under a different fleet name. Review the existing member before pairing; nothing was changed.".into());
        }
    }
    Ok(())
}

fn stage_script(plan: &Plan, stage: &str) -> String {
    if plan.os == "Windows" {
        format!("$ErrorActionPreference='Stop'; $stage={}; if(Test-Path -LiteralPath $stage){{throw 'Staging exists'}}; $p=Split-Path $stage; while($p){{if((Test-Path -LiteralPath $p) -and ((Get-Item -Force -LiteralPath $p).Attributes -band [IO.FileAttributes]::ReparsePoint)){{throw 'Reparse parent'}}; $p=Split-Path $p}}; New-Item -ItemType Directory -Path $stage | Out-Null; $acl=New-Object Security.AccessControl.DirectorySecurity; $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User; $acl.SetOwner($sid); $acl.SetAccessRuleProtection($true,$false); $rule=New-Object Security.AccessControl.FileSystemAccessRule($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow'); $acl.AddAccessRule($rule); Set-Acl -LiteralPath $stage -AclObject $acl; New-Item -ItemType Directory -Path ($stage+'/bin'),($stage+'/legal') | Out-Null", ps_quote(stage))
    } else {
        format!("set -eu\numask 077\np={}\nwhile [ \"$p\" != / ] && [ -n \"$p\" ]; do test ! -L \"$p\" || exit 45; p=$(dirname \"$p\"); done\ntest ! -e {}\nmkdir -p {}\nmkdir {}\nmkdir {}/bin {}/legal\n", sh_quote(stage), sh_quote(stage), sh_quote(&format!("{}/.perennos-tempo-controls", plan.user_home)), sh_quote(stage), sh_quote(stage), sh_quote(stage))
    }
}

fn transfer_script(plan: &Plan, stage: &str, file: &controls::ControlFile) -> String {
    let path = format!("{stage}/{}", file.path);
    if plan.os == "Windows" {
        format!("$ErrorActionPreference='Stop'; $p={}; $s=[IO.File]::Open($p,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None); try{{[Console]::OpenStandardInput().CopyTo($s)}}finally{{$s.Dispose()}}; $h=[Security.Cryptography.SHA256]::Create(); $s=[IO.File]::OpenRead($p); try{{$digest=[BitConverter]::ToString($h.ComputeHash($s)).Replace('-','').ToLowerInvariant()}}finally{{$s.Dispose();$h.Dispose()}}; if((Get-Item -LiteralPath $p).Length -ne {} -or $digest -ne {}){{throw 'Transferred file did not match'}}", ps_quote(&path), file.bytes.len(), ps_quote(&file.sha256))
    } else {
        let hash = if plan.os == "Darwin" {
            "shasum -a 256"
        } else {
            "sha256sum"
        };
        format!("set -eu\numask 077\nset -C\ncat > {}\ntest \"$(wc -c < {})\" -eq {}\ntest \"$({hash} < {} | cut -d' ' -f1)\" = {}\n", sh_quote(&path), sh_quote(&path), file.bytes.len(), sh_quote(&path), sh_quote(&file.sha256))
    }
}

fn promote_script(plan: &Plan, stage: &str) -> String {
    let root = controls_root(plan);
    let expected = format!(
        "tempo-launch {}.{}.{}",
        plan.installed_version.0[0], plan.installed_version.0[1], plan.installed_version.0[2]
    );
    if plan.os == "Windows" {
        format!("$ErrorActionPreference='Stop'; $root={}; if(Test-Path -LiteralPath $root){{throw 'Installed directory exists'}}; Move-Item -LiteralPath {} -Destination $root; $env:TEMPO_HOME={}; if((& {} version).Trim() -ne {}){{throw 'Native version mismatch'}}; if(((& {} home --json)|ConvertFrom-Json).home -ne {}){{throw 'Native home mismatch'}}", ps_quote(&root), ps_quote(stage), ps_quote(&plan.home), ps_quote(&plan.launcher), ps_quote(&expected), ps_quote(&plan.launcher), ps_quote(&plan.home))
    } else {
        format!("set -eu\ntest ! -e {root}\nchmod 700 {stage}/bin/tempo-launch {stage}/bin/tempo-local-agent\nmv {stage} {root}\nexport TEMPO_HOME={home}\ntest \"$({bin} version)\" = {expected}\n{bin} home --json\n", root=sh_quote(&root), stage=sh_quote(stage), home=sh_quote(&plan.home), bin=sh_quote(&plan.launcher), expected=sh_quote(&expected))
    }
}

fn persistence_script(plan: &Plan) -> Result<String, String> {
    let suffix = &crate::setup::sha256::sha256_hex(plan.home.as_bytes())[..12];
    let name = format!("perennos-tempo-controls-{suffix}");
    match plan.os.as_str() {
        "Linux" => {
            let quote = |s: &str| {
                format!(
                    "\"{}\"",
                    s.replace('\\', "\\\\")
                        .replace('"', "\\\"")
                        .replace('%', "%%")
                )
            };
            let unit = format!("[Unit]\nDescription=Perennos Tempo fleet controls\nAfter=network-online.target\n[Service]\nType=simple\nEnvironment={}\nEnvironment=TEMPO_FLEET_PORT={}\nExecStart={} fleet serve --quiet\nRestart=on-failure\nRestartSec=5\n[Install]\nWantedBy=default.target\n", quote(&format!("TEMPO_HOME={}", plan.home)), plan.fleet_port, quote(&plan.launcher));
            Ok(format!("set -eu\numask 077\ncommand -v systemctl >/dev/null\nif [ \"$(loginctl show-user \"$(id -un)\" -p Linger --value)\" != yes ]; then sudo -n loginctl enable-linger \"$(id -un)\"; fi\ntest \"$(loginctl show-user \"$(id -un)\" -p Linger --value)\" = yes\nd=$HOME/.config/systemd/user\nmkdir -p \"$d\"\ntest ! -L \"$d\"\nf=\"$d/{name}.service\"\ntest ! -L \"$f\"\ntmp=\"$d/{name}.stage-{}\"\nset -C\ncat > \"$tmp\" <<'TEMPO_UNIT'\n{unit}TEMPO_UNIT\nif [ -e \"$f\" ]; then cmp -s \"$f\" \"$tmp\" || {{ echo 'Existing service differs; kept for review.' >&2; exit 46; }}; else mv \"$tmp\" \"$f\"; fi\nsystemctl --user daemon-reload\nsystemctl --user enable --now {name}.service\nsystemctl --user is-active --quiet {name}.service\necho messenger=ready\n", crypto::hex(&crypto::random(8))))
        }
        "Darwin" => {
            let xml = crate::html_escape;
            let label = format!("com.perennos.{name}");
            let plist = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{}</string><string>fleet</string><string>serve</string><string>--quiet</string></array><key>EnvironmentVariables</key><dict><key>TEMPO_HOME</key><string>{}</string><key>TEMPO_FLEET_PORT</key><string>{}</string></dict><key>RunAtLoad</key><true/><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict><key>ThrottleInterval</key><integer>5</integer></dict></plist>\n", xml(&plan.launcher), xml(&plan.home), plan.fleet_port);
            Ok(format!("set -eu\numask 077\nd=$HOME/Library/LaunchAgents\nmkdir -p \"$d\"\ntest ! -L \"$d\"\nf=\"$d/{label}.plist\"\ntest ! -L \"$f\"\ntmp=\"$d/{label}.stage-{}\"\nset -C\ncat > \"$tmp\" <<'TEMPO_PLIST'\n{plist}TEMPO_PLIST\nplutil -lint \"$tmp\"\nif [ -e \"$f\" ]; then cmp -s \"$f\" \"$tmp\" || {{ echo 'Existing LaunchAgent differs; kept for review.' >&2; exit 46; }}; else mv \"$tmp\" \"$f\"; fi\nif ! launchctl print \"gui/$(id -u)/{label}\" >/dev/null 2>&1; then launchctl bootstrap \"gui/$(id -u)\" \"$f\"; fi\nlaunchctl print \"gui/$(id -u)/{label}\" >/dev/null\necho messenger=ready\n", crypto::hex(&crypto::random(8))))
        }
        "Windows" => {
            // A wrapper sets the native home; the task stays in this user's interactive token.
            let wrapper = format!("{}/fleet-controls-{suffix}.ps1", plan.home);
            let text = format!("$env:TEMPO_HOME={}; $env:TEMPO_FLEET_PORT='{}'; & {} fleet serve --quiet; exit $LASTEXITCODE\r\n", ps_quote(&plan.home), plan.fleet_port, ps_quote(&plan.launcher));
            Ok(format!(
                r#"$ErrorActionPreference='Stop'
$name={}; $file={}; $text={}; $dir={}
$user=[Security.Principal.WindowsIdentity]::GetCurrent().Name
$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$exe=$env:SystemRoot+'\System32\WindowsPowerShell\v1.0\powershell.exe'
$args='-NoProfile -NonInteractive -File '+[char]34+$file+[char]34
$prior=Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
$parent=$dir
while($parent) {{ if((Test-Path -LiteralPath $parent) -and ((Get-Item -Force -LiteralPath $parent).Attributes -band [IO.FileAttributes]::ReparsePoint)){{throw 'A parent is a reparse point'}}; $parent=Split-Path $parent }}
$item=Get-Item -Force -LiteralPath $file -ErrorAction SilentlyContinue
if($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {{throw 'Wrapper is a reparse point'}}
if(Test-Path -LiteralPath $file) {{ if([IO.File]::ReadAllText($file) -cne $text){{throw 'Existing wrapper differs'}} }}
elseif($prior) {{throw 'Existing task has no matching wrapper'}}
if($prior) {{
  $owner=(New-Object Security.Principal.NTAccount($prior.Principal.UserId)).Translate([Security.Principal.SecurityIdentifier]).Value
  if($owner -ne $sid -or $prior.Principal.RunLevel -ne 'Limited' -or $prior.Principal.LogonType -ne 'Interactive' -or @($prior.Actions).Count -ne 1 -or $prior.Actions[0].Execute -ne $exe -or $prior.Actions[0].Arguments -cne $args -or $prior.Actions[0].WorkingDirectory -or @($prior.Triggers).Count -ne 1 -or $prior.Triggers[0].CimClass.CimClassName -ne 'MSFT_TaskLogonTrigger' -or !$prior.Triggers[0].Enabled -or $prior.Triggers[0].EndBoundary -or ($prior.Triggers[0].Delay -and $prior.Triggers[0].Delay -ne 'PT0S') -or ($prior.Triggers[0].Repetition.Interval -and $prior.Triggers[0].Repetition.Interval -ne 'PT0S') -or $prior.Settings.MultipleInstances -ne 'IgnoreNew' -or $prior.Settings.ExecutionTimeLimit -ne 'PT0S' -or $prior.Settings.DisallowStartIfOnBatteries -or $prior.Settings.StopIfGoingOnBatteries) {{throw 'Existing task differs; it was kept'}}
  $triggerOwner=(New-Object Security.Principal.NTAccount($prior.Triggers[0].UserId)).Translate([Security.Principal.SecurityIdentifier]).Value
  if($triggerOwner -ne $sid) {{throw 'Existing task trigger differs; it was kept'}}
}} else {{
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  if(!(Test-Path -LiteralPath $file)) {{$stream=[IO.File]::Open($file,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None); $writer=New-Object IO.StreamWriter($stream); try{{$writer.Write($text)}}finally{{$writer.Dispose()}}}}
  $a=New-ScheduledTaskAction -Execute $exe -Argument $args
  $t=New-ScheduledTaskTrigger -AtLogOn -User $user
  $p=New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
  $s=New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
  Register-ScheduledTask -TaskName $name -Action $a -Trigger $t -Principal $p -Settings $s | Out-Null
}}
if((Get-ScheduledTask -TaskName $name).State -ne 'Running') {{Start-ScheduledTask -TaskName $name}}
Start-Sleep -Seconds 2
if((Get-ScheduledTask -TaskName $name).State -ne 'Running'){{throw 'Background task did not remain running'}}
'messenger=ready'"#,
                ps_quote(&name),
                ps_quote(&wrapper),
                ps_quote(&text),
                ps_quote(&plan.home)
            ))
        }
        _ => Err("This platform has no supported persistent messenger route.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::json::J;
    use std::cell::RefCell;

    struct Fake {
        calls: RefCell<Vec<&'static str>>,
        fail_at: Option<&'static str>,
    }

    impl Fake {
        fn call(&self, name: &'static str) -> Result<(), String> {
            self.calls.borrow_mut().push(name);
            if self.fail_at == Some(name) {
                Err(format!("{name} failed"))
            } else {
                Ok(())
            }
        }
    }

    impl Actions for Fake {
        fn recheck(&mut self, _: &Plan) -> Result<(), String> {
            self.call("recheck")
        }
        fn install(&mut self, plan: &Plan, progress: &mut dyn FnMut(&str)) -> Result<(), String> {
            if plan.artifact.is_some() {
                progress("Installing controls");
                self.call("install")?;
            }
            Ok(())
        }
        fn controller(&mut self, _: &Plan) -> Result<(), String> {
            self.call("controller")
        }
        fn pair(&mut self, _: &Plan) -> Result<String, String> {
            self.call("pair")?;
            Ok("a".repeat(64))
        }
        fn start_messenger(&mut self, _: &Plan) -> Result<(), String> {
            self.call("service")
        }
        fn register(&mut self, _: &Plan, _: &str) -> Result<(), String> {
            self.call("register")
        }
    }

    fn plan() -> Plan {
        let mut p = Plan {
            target: "nas".into(),
            name: "nas".into(),
            private_addr: "10.0.0.9".into(),
            port: 22,
            fleet_port: 8770,
            controller_port: 8770,
            installed_version: Version([2, 1, 0]),
            home: "/home/nas/.local/state/perennos-tempo".into(),
            launcher: "/home/nas/.local/bin/tempo-launch".into(),
            os: "Linux".into(),
            arch: "x86_64".into(),
            user: "nas".into(),
            user_home: "/home/nas".into(),
            key_hash: String::new(),
            fleet_json: String::new(),
            artifact: None,
            id: String::new(),
        };
        p.id = plan_id(&p);
        p
    }

    fn fake(fail_at: Option<&'static str>) -> Fake {
        Fake {
            calls: RefCell::new(vec![]),
            fail_at,
        }
    }

    #[test]
    fn parses_the_real_launcher_version_and_active_tempo_home() {
        let text = format!(
            "noise\n{BEGIN}\nos=Linux\narch=x86_64\nuser=worker\nuser_home=/home/worker\nname=worker\nlauncher=/usr/local/bin/tempo-launch\nversion=tempo-launch 2.1.0\nhome_json={{\"home\":\"/home/worker/.local/state/perennos-tempo\",\"step\":\"env:TEMPO_HOME\"}}\nfleet_port=8770\n{END}\n"
        );
        let p = parse_probe(&text, "worker", "10.0.0.9", 22).unwrap();
        assert_eq!(p.installed_version, Version([2, 1, 0]));
        assert_eq!(p.home, "/home/worker/.local/state/perennos-tempo");
        assert_eq!(p.launcher, "/usr/local/bin/tempo-launch");
    }

    #[test]
    fn native_name_lookup_uses_the_production_fleet_serializer_key() {
        let fleet = Fleet {
            me: "worker-hub".into(),
            ..Fleet::default()
        };
        let serialized = fleet.to_j().pretty();
        assert!(serialized.contains(&format!("\"{FLEET_SELF_KEY}\": \"worker-hub\"")));
    }

    #[test]
    fn incomplete_or_invalid_probe_metadata_is_rejected() {
        let missing_end = format!("{BEGIN}\nos=Linux\narch=x86_64\n");
        assert!(parse_probe(&missing_end, "nas", "10.0.0.9", 22).is_err());
        let bad_home = format!("{BEGIN}\nos=Linux\narch=x86_64\nname=nas\nlauncher=/bin/tempo-launch\nversion=tempo-launch 2.1.0\nhome_json=not-json\nfleet_port=0\n{END}");
        assert!(parse_probe(&bad_home, "nas", "10.0.0.9", 22).is_err());
        let no_launcher = format!("{BEGIN}\nos=Linux\narch=x86_64\nhome_json={{\"home\":\"/home/nas/.local/state/perennos-tempo\"}}\n{END}");
        assert!(parse_probe(&no_launcher, "nas", "10.0.0.9", 22).is_err());
    }

    #[test]
    fn decline_has_no_remote_or_local_writes() {
        let p = plan();
        let mut f = fake(None);
        assert!(apply_with(&p, None, &mut f).is_err());
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn every_mutable_plan_fact_is_bound_to_the_old_approval() {
        let base = plan();
        let approval = base.approval();
        for field in 0..8 {
            let mut p = base.clone();
            match field {
                0 => p.target.push_str("-other"),
                1 => p.name.push_str("-other"),
                2 => p.private_addr = "10.0.0.10".into(),
                3 => p.port = 2200,
                4 => p.fleet_port = 8771, // service endpoint remains bound to approval
                5 => p.installed_version = Version([2, 1, 1]),
                6 => p.home.push_str("-other"),
                _ => p.launcher.push_str("-other"),
            }
            let mut f = fake(None);
            assert!(
                apply_with(&p, Some(&approval), &mut f).is_err(),
                "field {field}"
            );
            assert!(f.calls.borrow().is_empty(), "field {field} reached I/O");
        }
    }

    #[test]
    fn install_key_and_service_must_succeed_before_registration() {
        let p = plan();
        let mut pair = fake(Some("pair"));
        assert!(apply_with(&p, Some(&p.approval()), &mut pair).is_err());
        assert_eq!(*pair.calls.borrow(), ["recheck", "controller", "pair"]);
        let mut service = fake(Some("service"));
        assert!(apply_with(&p, Some(&p.approval()), &mut service).is_err());
        assert_eq!(
            *service.calls.borrow(),
            ["recheck", "controller", "pair", "service"]
        );
    }

    #[test]
    fn enrollment_registers_only_after_pairing_and_service_start() {
        let p = plan();
        let mut f = fake(None);
        apply_with(&p, Some(&p.approval()), &mut f).unwrap();
        assert_eq!(
            *f.calls.borrow(),
            ["recheck", "controller", "pair", "service", "register"]
        );
    }

    #[test]
    fn persistence_uses_user_service_and_verifies_linger_without_models() {
        let script = persistence_script(&plan()).unwrap();
        assert!(script.contains("systemctl --user enable --now"));
        assert!(script.contains("Linger --value"));
        assert!(
            !script.contains("nohup")
                && !script.contains("ollama")
                && !script.contains("setup --apply")
        );
    }

    #[test]
    fn registration_updates_only_approved_identity_and_address() {
        let mut p = plan();
        p.fleet_port = 9001;
        p.id = plan_id(&p);
        let mut fleet = Fleet::default();
        let mut existing = Machine::new("nas", "10.0.0.8", "both");
        existing.enabled = false;
        existing.last_seen = 47;
        existing.probe = J::s("probe");
        existing.health = J::s("health");
        existing.ssh = J::s("ssh");
        existing.tools = J::s("tools");
        existing.id = "a".repeat(64);
        fleet.machines.push(existing);
        let other = Machine::new("other", "10.0.0.7", "engine");
        fleet.machines.push(other.clone());
        register_member(&mut fleet, &p, &"a".repeat(64)).unwrap();
        let updated = &fleet.machines[0];
        assert_eq!(updated.addr, "10.0.0.9");
        assert_eq!(updated.port, 9001);
        assert_eq!(updated.id, "a".repeat(64));
        assert_eq!(updated.role, "both");
        assert!(!updated.enabled);
        assert_eq!(updated.last_seen, 47);
        assert_eq!(updated.probe, J::s("probe"));
        assert_eq!(updated.health, J::s("health"));
        assert_eq!(updated.ssh, J::s("ssh"));
        assert_eq!(updated.tools, J::s("tools"));
        assert_eq!(fleet.machines[1], other);

        let mut new_fleet = Fleet::default();
        register_member(&mut new_fleet, &p, &"c".repeat(64)).unwrap();
        assert_eq!(new_fleet.machines[0].port, 9001);
    }

    #[test]
    fn rollback_restores_only_owned_member_fields_and_refuses_a_changed_postimage() {
        let p = plan();
        let id = "a".repeat(64);
        let mut original = Machine::new("nas", "10.0.0.8", "engine");
        original.port = 1234;
        original.id = id.clone();
        let mut fleet = Fleet::default();
        fleet.machines.push(original.clone());
        fleet
            .machines
            .push(Machine::new("other", "10.0.0.7", "agent"));
        register_member(&mut fleet, &p, &id).unwrap();
        fleet.machines[0].health = J::s("new heartbeat");
        assert!(rollback_member(&mut fleet, &p, &id, Some(&original)));
        assert_eq!(
            (fleet.machines[0].addr.as_str(), fleet.machines[0].port),
            ("10.0.0.8", 1234)
        );
        assert_eq!(fleet.machines[0].health, J::s("new heartbeat"));
        assert_eq!(fleet.machines[1].name, "other");
        assert!(!rollback_member(&mut fleet, &p, &id, Some(&original)));
        register_member(&mut fleet, &p, &id).unwrap();
        fleet.machines[0].port = 4321;
        let changed = fleet.clone();
        assert!(!rollback_member(&mut fleet, &p, &id, Some(&original)));
        assert_eq!(fleet, changed);
    }

    #[test]
    fn rollback_removes_only_an_unchanged_new_member() {
        let p = plan();
        let id = "a".repeat(64);
        let mut fleet = Fleet::default();
        fleet
            .machines
            .push(Machine::new("other", "10.0.0.7", "agent"));
        register_member(&mut fleet, &p, &id).unwrap();
        assert!(rollback_member(&mut fleet, &p, &id, None));
        assert_eq!(fleet.machines.len(), 1);
        register_member(&mut fleet, &p, &id).unwrap();
        fleet.machines[1].enabled = false;
        let changed = fleet.clone();
        assert!(!rollback_member(&mut fleet, &p, &id, None));
        assert_eq!(fleet, changed);
    }

    #[test]
    fn a_different_saved_identity_is_refused_without_changing_either_node() {
        let p = plan();
        let mut fleet = Fleet::default();
        let mut existing = Machine::new("nas", "10.0.0.8", "both");
        existing.id = "b".repeat(64);
        fleet.machines.push(existing);
        fleet
            .machines
            .push(Machine::new("other", "10.0.0.7", "engine"));
        let before = fleet.clone();
        let error = register_member(&mut fleet, &p, &"a".repeat(64)).unwrap_err();
        assert!(error.contains("identity collision"));
        assert_eq!(fleet, before);
    }

    #[test]
    fn existing_address_with_another_name_is_refused_before_pairing() {
        let p = plan();
        let mut f = Fleet::default();
        f.machines
            .push(Machine::new("different", &p.private_addr, "agent"));
        let before = f.clone();
        assert!(check_member_name(&f, &p).is_err());
        assert_eq!(f, before);
        f.machines[0].name = p.name.clone();
        assert!(check_member_name(&f, &p).is_ok());
    }

    #[test]
    fn missing_controls_install_and_partial_failures_never_register_early() {
        let mut p = plan();
        p.artifact = Some(controls::Artifact {
            version: p.installed_version,
            target: "x86_64-unknown-linux-gnu".into(),
            name: "controls.tar.gz".into(),
            url: String::new(),
            size: 100,
            sha256: "a".repeat(64),
        });
        p.id = plan_id(&p);
        let mut success = fake(None);
        let mut progress = Vec::new();
        apply_steps(&p, Some(&p.approval()), &mut success, &mut |s| {
            progress.push(s.to_string())
        })
        .unwrap();
        assert_eq!(
            *success.calls.borrow(),
            [
                "recheck",
                "install",
                "controller",
                "pair",
                "service",
                "register"
            ]
        );
        assert_eq!(progress.last().unwrap(), "Connected");
        for (fail, expected) in [
            ("recheck", vec!["recheck"]),
            ("install", vec!["recheck", "install"]),
            ("controller", vec!["recheck", "install", "controller"]),
            (
                "service",
                vec!["recheck", "install", "controller", "pair", "service"],
            ),
        ] {
            let mut actions = fake(Some(fail));
            assert!(apply_with(&p, Some(&p.approval()), &mut actions).is_err());
            assert_eq!(*actions.calls.borrow(), expected);
        }
        let approval = p.approval();
        p.artifact.as_mut().unwrap().sha256 = "b".repeat(64);
        let mut actions = fake(None);
        assert!(apply_with(&p, Some(&approval), &mut actions).is_err());
        assert!(actions.calls.borrow().is_empty());
    }

    #[test]
    fn user_platform_home_and_fleet_fingerprint_are_approval_facts() {
        let p = plan();
        let approval = p.approval();
        for field in 0..7 {
            let mut changed = p.clone();
            match field {
                0 => changed.os = "Darwin".into(),
                1 => changed.arch = "arm64".into(),
                2 => changed.user.push('x'),
                3 => changed.user_home.push('x'),
                4 => changed.key_hash = "a".repeat(64),
                5 => changed.fleet_json = "{}".into(),
                _ => changed.controller_port = 9123,
            }
            let mut actions = fake(None);
            assert!(apply_with(&changed, Some(&approval), &mut actions).is_err());
            assert!(actions.calls.borrow().is_empty());
        }
    }

    #[test]
    fn default_callback_remains_compatible_with_existing_install_key() {
        assert_eq!(callback_endpoint("10.0.0.1", 8770), "10.0.0.1");
        assert_eq!(callback_endpoint("10.0.0.1", 9123), "10.0.0.1:9123");
        assert_eq!(callback_endpoint("fd00::1", 9123), "[fd00::1]:9123");
    }

    #[test]
    fn other_fleet_key_is_refused_before_install_or_pairing() {
        let paths = Paths {
            home: std::env::temp_dir().join(format!(
                "tempo-other-fleet-{}",
                crypto::hex(&crypto::random(8))
            )),
            agent_data: std::env::temp_dir(),
        };
        let mut p = plan();
        p.key_hash = "a".repeat(64);
        assert!(check_existing_fleet(&paths, &p)
            .unwrap_err()
            .contains("different fleet"));
        assert!(!paths.home.exists());
    }

    #[test]
    fn invalid_local_key_is_never_treated_as_permission_to_replace_it() {
        let paths = Paths {
            home: std::env::temp_dir().join(format!(
                "tempo-local-key-{}",
                crypto::hex(&crypto::random(8))
            )),
            agent_data: std::env::temp_dir(),
        };
        assert!(check_local_key(&paths).is_ok());
        std::fs::create_dir_all(&paths.home).unwrap();
        std::fs::write(paths.file("fleet.key"), b"damaged").unwrap();
        assert!(check_local_key(&paths).is_err());
        assert!(initialize_local_key(&paths).is_err());
        assert_eq!(std::fs::read(paths.file("fleet.key")).unwrap(), b"damaged");
        std::fs::write(paths.file("fleet.key"), [7u8; 32]).unwrap();
        assert!(check_local_key(&paths).is_ok());
        assert!(!initialize_local_key(&paths).unwrap());
        assert_eq!(std::fs::read(paths.file("fleet.key")).unwrap(), [7u8; 32]);
    }

    #[test]
    fn absent_local_key_is_created_once_and_never_replaced() {
        let paths = Paths {
            home: std::env::temp_dir().join(format!(
                "tempo-create-key-{}",
                crypto::hex(&crypto::random(8))
            )),
            agent_data: std::env::temp_dir(),
        };
        assert!(initialize_local_key(&paths).unwrap());
        let first = std::fs::read(paths.file("fleet.key")).unwrap();
        assert_eq!(first.len(), 32);
        assert!(!initialize_local_key(&paths).unwrap());
        assert_eq!(std::fs::read(paths.file("fleet.key")).unwrap(), first);
    }

    #[test]
    fn missing_launcher_probe_uses_the_actual_native_home_on_all_supported_targets() {
        for (os, arch, user_home, home) in [
            ("Linux", "x86_64", "/home/worker", "/home/worker/.tempo"),
            ("Darwin", "arm64", "/Users/worker", "/Users/worker/.tempo"),
            (
                "Windows",
                "AMD64",
                "C:/Users/worker",
                "C:/Users/worker/AppData/Local/Perennos Tempo",
            ),
        ] {
            let text = format!("{BEGIN}\nos={os}\narch={arch}\nname=worker\nuser=worker\nuser_home={user_home}\ndefault_home={home}\nlauncher=\nfleet_port=8770\n{END}\n");
            let plan = parse_probe(&text, "worker", "10.0.0.9", 22).unwrap();
            assert_eq!(plan.home, home);
            assert!(plan.launcher.is_empty());
            assert!(plan.artifact.is_none());
        }
    }

    #[test]
    fn decoded_native_home_with_quotes_and_backslashes_stays_a_single_shell_argument() {
        let text = format!("{BEGIN}\nos=Linux\narch=x86_64\nname=worker\nuser=worker\nuser_home=/home/worker\nlauncher=/bin/tempo-launch\nversion=tempo-launch 2.1.0\nhome_json={{\"home\":\"/home/worker/a'\\\\b\"}}\nfleet_port=8770\n{END}\n");
        let plan = parse_probe(&text, "worker", "10.0.0.9", 22).unwrap();
        assert_eq!(plan.home, "/home/worker/a'\\b");
        assert_eq!(
            sh_quote(&format!("{}/fleet.key", plan.home)),
            "'/home/worker/a'\\''\\b/fleet.key'"
        );
    }

    #[test]
    fn initial_native_challenge_binds_peer_request_recipient_and_replay_guard() {
        let request = super::super::msg::Frame {
            kind: 'p',
            id: "request".into(),
            from: "main".into(),
            to: "worker".into(),
            ..Default::default()
        };
        let reply = super::super::msg::Frame {
            kind: 'o',
            id: request.id.clone(),
            from: request.to.clone(),
            to: request.from.clone(),
            ts: 100,
            nonce: "fresh".into(),
            ..Default::default()
        };
        let mut guard = super::super::msg::Guard::default();
        verify_probe_reply(&request, &reply, &mut guard, 100).unwrap();
        assert!(verify_probe_reply(&request, &reply, &mut guard, 100).is_err());
        for field in 0..5 {
            let mut changed = reply.clone();
            match field {
                0 => changed.id.push('x'),
                1 => changed.from.push('x'),
                2 => changed.to.push('x'),
                3 => changed.kind = 'a',
                _ => changed.ts = 999_999,
            };
            assert!(verify_probe_reply(&request, &changed, &mut Default::default(), 100).is_err());
        }
    }
}
