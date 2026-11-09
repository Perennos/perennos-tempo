//! On Windows targets, give tempo-launch.exe its version details (the name Windows shows in Task
//! Manager and in the file's properties) and its icon: `assets/tempo-launch.ico` (the Perennos insignia), or
//! the .ico file TEMPO_LAUNCH_ICON names.
//! Without a resource compiler this is skipped with a note: the program works the same.
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TEMPO_LAUNCH_ICON");
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
    let default_icon = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("assets/tempo-launch.ico");
    let icon = env::var("TEMPO_LAUNCH_ICON")
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
      VALUE "FileDescription", "Perennos Tempo"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "tempo-launch"
      VALUE "LegalCopyright", "Copyright 2026 Perennos"
      VALUE "OriginalFilename", "tempo-launch.exe"
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
    let rc_path = out.join("tempo-launch.rc");
    std::fs::write(&rc_path, rc).expect("write the resource script");
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let (res, ok) = if msvc {
        let res = out.join("tempo-launch.res");
        let ok = Command::new("rc.exe")
            .arg("/nologo")
            .arg("/fo")
            .arg(&res)
            .arg(&rc_path)
            .status()
            .is_ok_and(|s| s.success());
        (res, ok)
    } else {
        let res = out.join("tempo-launch-res.o");
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
    if ok {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        println!("cargo:warning=no resource compiler (rc.exe or windres): tempo-launch.exe is built without its icon and version details");
    }
}
