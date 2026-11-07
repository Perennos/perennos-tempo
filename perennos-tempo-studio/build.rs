//! On Windows targets, give tempo-studio.exe its icon and version details (the name Windows shows in Task
//! Manager, Settings > Apps and the file's properties: Perennos Tempo Studio, by Perennos). The icon is
//! `assets/tempo-studio.ico`, or the file `TEMPO_STUDIO_ICON` names. Without a resource compiler (rc.exe or
//! windres) this prints a warning and the program is built without them: it works the same, never a build failure.
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = "tempo-studio";
const DESCRIPTION: &str = "Perennos Tempo Studio";
const ICON_ENV: &str = "TEMPO_STUDIO_ICON";
const ICON_FILE: &str = "assets/tempo-studio.ico";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={ICON_ENV}");
    println!("cargo:rerun-if-env-changed=WINDRES");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let mut nums: Vec<u32> = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    nums.resize(4, 0);
    let commas = nums
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let default_icon =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join(ICON_FILE);
    println!("cargo:rerun-if-changed={}", default_icon.display());
    let icon = env::var(ICON_ENV)
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| Some(default_icon).filter(|p| p.is_file()))
        .map(|p| {
            println!("cargo:rerun-if-changed={}", p.display());
            format!(
                "1 ICON \"{}\"\n",
                p.display().to_string().replace('\\', "\\\\")
            )
        })
        .unwrap_or_default();
    let rc = format!(
        r#"{icon}1 VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
FILEFLAGSMASK 0x3f
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "Perennos"
      VALUE "FileDescription", "{DESCRIPTION}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{BIN}"
      VALUE "LegalCopyright", "Copyright 2026 Perennos"
      VALUE "OriginalFilename", "{BIN}.exe"
      VALUE "ProductName", "Perennos Tempo"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let rc_path = out.join(format!("{BIN}.rc"));
    if std::fs::write(&rc_path, rc).is_err() {
        println!("cargo:warning=cannot write the resource script: {BIN}.exe is built without its icon and version details");
        return;
    }
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let (res, ok) = if msvc {
        let res = out.join(format!("{BIN}.res"));
        let ok = Command::new("rc.exe")
            .arg("/nologo")
            .arg("/fo")
            .arg(&res)
            .arg(&rc_path)
            .status()
            .is_ok_and(|s| s.success());
        (res, ok)
    } else {
        let res = out.join(format!("{BIN}-res.o"));
        let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let tools = [
            env::var("WINDRES").unwrap_or_default(),
            format!("{arch}-w64-mingw32-windres"),
            "windres".to_string(),
        ];
        let ok = tools.iter().filter(|t| !t.is_empty()).any(|t| {
            Command::new(t)
                .arg("--input")
                .arg(&rc_path)
                .arg("--output")
                .arg(&res)
                .arg("--output-format=coff")
                .status()
                .is_ok_and(|s| s.success())
        });
        (res, ok)
    };
    if ok && Path::new(&res).is_file() {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        println!("cargo:warning=no resource compiler (rc.exe or windres): {BIN}.exe is built without its icon and version details");
    }
}
