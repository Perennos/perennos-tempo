fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout();
    if let Err(e) = tempo_vault::cli::run(&args, &mut out) {
        eprintln!("tempo-vault: {e}");
        std::process::exit(2);
    }
}
