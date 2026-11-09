//! A read-only plan, then an explicitly approved link to a computer with Tempo already installed.

use super::crypto;
use super::first_line;
use super::pair;
use super::scan;
use super::ssh;
use super::store::{self, Fleet, Machine, Paths};
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
launcher=$(command -v tempo-launch 2>/dev/null)
if [ -z "$launcher" ] && [ -x "$HOME/.local/bin/tempo-launch" ]; then launcher="$HOME/.local/bin/tempo-launch"; fi
printf 'launcher=%s\n' "$launcher"
if [ -n "$launcher" ]; then
  printf 'version=%s\n' "$("$launcher" version 2>/dev/null | head -n 1)"
  printf 'home_json=%s\n' "$("$launcher" home --json 2>/dev/null)"
fi
printf 'fleet_port=%s\n' "${TEMPO_FLEET_PORT:-8770}"
printf '__TEMPO_BOOTSTRAP_END__\n'
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub target: String,
    pub name: String,
    pub private_addr: String,
    pub port: u16,
    pub fleet_port: u16,
    pub installed_version: Version,
    pub home: String,
    pub launcher: String,
    id: String,
}

impl Plan {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn approval(&self) -> Approval {
        Approval(self.id.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval(String);

pub trait Actions {
    fn recheck(&mut self, plan: &Plan) -> Result<(), String>;
    fn pair(&mut self, plan: &Plan) -> Result<String, String>;
    fn start_messenger(&mut self, plan: &Plan) -> Result<(), String>;
    fn register(&mut self, plan: &Plan, identity: &str) -> Result<(), String>;
}

pub fn prepare(target: &str) -> Result<Plan, String> {
    probe(target)
}

pub fn apply(p: &Paths, plan: &Plan, approval: Option<&Approval>) -> Result<(), String> {
    let mut native = Native { paths: p };
    apply_with(plan, approval, &mut native)
}

pub fn apply_with(
    plan: &Plan,
    approval: Option<&Approval>,
    actions: &mut dyn Actions,
) -> Result<(), String> {
    if plan_id(plan) != plan.id || !approval.is_some_and(|a| a.0 == plan.id) {
        return Err("Nothing was changed; approval did not match this plan.".into());
    }
    actions.recheck(plan)?;
    let identity = actions.pair(plan)?;
    actions.start_messenger(plan)?;
    actions.register(plan, &identity)
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
    let said =
        ssh::run(&args, Some(PROBE), Duration::from_secs(15)).map_err(|e| ssh_problem(&e))?;
    if !said.ok {
        return Err(ssh_problem(&said.err));
    }
    let mut plan = parse_probe(&said.out, target, &dest, port)?;
    if let Some(name) = fleet_self_name(&plan)? {
        plan.name = name;
    }
    plan.id = plan_id(&plan);
    Ok(plan)
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
    if os != "Linux" {
        return Err(format!("The target reported {os} {arch}. This existing-launcher enrollment currently supports Linux only; no file or key was sent."));
    }
    if arch != "x86_64" && arch != "amd64" {
        return Err(format!(
            "The target reported Linux {arch}; this enrollment path supports x86_64 only."
        ));
    }
    let launcher = get("launcher");
    let version = parse_version(get("version"))
        .ok_or("Bootstrap pending: Tempo is not installed or its version could not be read. No key or setup script was sent.")?;
    if launcher.is_empty() {
        return Err(
            "Bootstrap pending: Tempo is not installed. No key or setup script was sent.".into(),
        );
    }
    let home = parse_json(get("home_json"))
        .ok()
        .and_then(|j| j.str("home").filter(|s| !s.is_empty()).map(str::to_string))
        .ok_or(
            "tempo-launch home --json did not return the active Tempo home; no changes were made.",
        )?;
    if home.chars().any(char::is_control) {
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
        installed_version: version,
        home,
        launcher: launcher.into(),
        id: String::new(),
    })
}

fn fleet_self_name(plan: &Plan) -> Result<Option<String>, String> {
    let file = sh_quote(&format!("{}/fleet.json", plan.home.trim_end_matches('/')));
    let script = format!(
        "if [ -r {file} ]; then sed -n 's/.*\"{FLEET_SELF_KEY}\"[[:space:]]*:[[:space:]]*\"\\([A-Za-z0-9._-]*\\)\".*/me=\\1/p' {file}; fi\n"
    );
    let args = ssh::strict_args_for(
        &plan.target,
        &plan.private_addr,
        plan.port,
        &["sh".into(), "-s".into()],
    )?;
    let said =
        ssh::run(&args, Some(&script), Duration::from_secs(10)).map_err(|e| ssh_problem(&e))?;
    if !said.ok {
        return Err(
            "Could not read the target's existing fleet name; no changes were made.".into(),
        );
    }
    Ok(said
        .out
        .lines()
        .find_map(|line| line.strip_prefix("me="))
        .map(store::clean_name)
        .filter(|n| !n.is_empty()))
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
        let args = ssh::strict_args_for(
            &plan.target,
            &plan.private_addr,
            plan.port,
            &["sh".into(), "-s".into()],
        )?;
        ssh::run(&args, Some(script), Duration::from_secs(seconds)).map_err(|e| ssh_problem(&e))
    }
}

impl Actions for Native<'_> {
    fn recheck(&mut self, plan: &Plan) -> Result<(), String> {
        let now = probe(&plan.target)?;
        if now.name != plan.name
            || now.private_addr != plan.private_addr
            || now.port != plan.port
            || now.fleet_port != plan.fleet_port
            || now.installed_version != plan.installed_version
            || now.home != plan.home
            || now.launcher != plan.launcher
        {
            return Err(
                "The target or installed launcher changed after this plan. Prepare again.".into(),
            );
        }
        Ok(())
    }

    fn pair(&mut self, plan: &Plan) -> Result<String, String> {
        store::init_key(self.paths)
            .map_err(|e| format!("Could not create the local fleet key: {e}"))?;
        let key = store::read_key(self.paths).ok_or("Could not read the local fleet key.")?;
        let own_id = pair::identity(self.paths)
            .map_err(|e| format!("Could not read the local fleet identity: {e}"))?;
        let local_addr = scan::route_address(&plan.private_addr)?;
        let me = store::load(self.paths).me;
        let script = format!(
            "TEMPO_HOME={} {} fleet install-key --name {} --peer {} <<'TEMPOKEY'\n{}\nTEMPOKEY\n",
            sh_quote(&plan.home),
            sh_quote(&plan.launcher),
            sh_quote(&plan.name),
            sh_quote(&format!(
                "{}={}#{}",
                me,
                local_addr,
                crypto::hex(own_id.verifying_key().as_bytes())
            )),
            crypto::hex(&key),
        );
        let said = self.remote(plan, &script, 30)?;
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
        let script = messenger_script(plan);
        let said = self.remote(plan, &script, 10)?;
        if said.ok && said.out.contains("messenger=ready") {
            Ok(())
        } else {
            Err(format!(
                "The key was installed, but the fleet messenger did not answer /hello: {}",
                first_line(&said.err)
            ))
        }
    }

    fn register(&mut self, plan: &Plan, identity: &str) -> Result<(), String> {
        if identity.len() != 64 || crypto::unhex(identity).is_none() {
            return Err("The target identity was invalid; it was not added.".into());
        }
        let result =
            store::update(self.paths, |f| register_member(f, plan, identity)).map_err(|e| {
                format!(
                    "The target paired and is running, but its fleet entry could not be saved: {e}"
                )
            })?;
        result.map_err(|e| {
            format!("The target paired and is running, but the saved identity needs review: {e}")
        })
    }
}

fn messenger_script(plan: &Plan) -> String {
    let host = if plan.private_addr.contains(':') {
        format!("[{}]", plan.private_addr)
    } else {
        plan.private_addr.clone()
    };
    let url = sh_quote(&format!("http://{host}:{}/hello", plan.fleet_port));
    let bin = sh_quote(&plan.launcher);
    format!(
            "set -eu\nurl={url}\nif ! command -v curl >/dev/null 2>&1; then echo 'curl is needed to check the fleet messenger.' >&2; exit 41; fi\ncheck() {{ curl -fsS --max-time 2 \"$url\" | grep -q '\"app\":\"tempo-fleet\"'; }}\nif check; then echo messenger=ready; exit 0; fi\nTEMPO_HOME={} nohup {bin} fleet serve --quiet >/dev/null 2>&1 </dev/null &\nfor n in 1 2 3 4 5; do sleep 1; if check; then echo messenger=ready; exit 0; fi; done\necho 'The fleet messenger did not answer /hello.' >&2\nexit 42\n",
            sh_quote(&plan.home),
        )
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
            installed_version: Version([2, 1, 0]),
            home: "/home/nas/.local/state/perennos-tempo".into(),
            launcher: "/home/nas/.local/bin/tempo-launch".into(),
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
            "noise\n{BEGIN}\nos=Linux\narch=x86_64\nname=worker\nlauncher=/usr/local/bin/tempo-launch\nversion=tempo-launch 2.1.0\nhome_json={{\"home\":\"/home/worker/.local/state/perennos-tempo\",\"step\":\"env:TEMPO_HOME\"}}\nfleet_port=8770\n{END}\n"
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
        assert!(parse_probe(&no_launcher, "nas", "10.0.0.9", 22)
            .unwrap_err()
            .contains("Bootstrap pending"));
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
        assert_eq!(*pair.calls.borrow(), ["recheck", "pair"]);
        let mut service = fake(Some("service"));
        assert!(apply_with(&p, Some(&p.approval()), &mut service).is_err());
        assert_eq!(*service.calls.borrow(), ["recheck", "pair", "service"]);
    }

    #[test]
    fn enrollment_registers_only_after_pairing_and_service_start() {
        let p = plan();
        let mut f = fake(None);
        apply_with(&p, Some(&p.approval()), &mut f).unwrap();
        assert_eq!(
            *f.calls.borrow(),
            ["recheck", "pair", "service", "register"]
        );
    }

    #[test]
    fn an_existing_hello_is_checked_before_starting_another_messenger() {
        let script = messenger_script(&plan());
        let check = script
            .find("if check; then echo messenger=ready; exit 0; fi")
            .unwrap();
        let start = script.find("nohup ").unwrap();
        assert!(check < start);
        assert!(script.contains("curl -fsS") && script.contains("/hello"));
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
}
