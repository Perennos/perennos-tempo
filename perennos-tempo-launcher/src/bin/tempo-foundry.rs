fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(tempo_launch::foundry::run(&args));
}
