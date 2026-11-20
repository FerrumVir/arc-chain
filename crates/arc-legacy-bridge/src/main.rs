fn main() {
    let code = arc_legacy_bridge::main_entry(std::env::args_os().collect());
    std::process::exit(code);
}
