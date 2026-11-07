// A Windows GUI program: started from a shortcut it never opens a console window.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(tempo_launch::run(&args));
}
