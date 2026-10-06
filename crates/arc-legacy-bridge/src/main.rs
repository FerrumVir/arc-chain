//! `arc-node` as delivered to released v0.7 updaters by the v0.7.12 bridge
//! release. All behavior lives in the library so it is unit tested.

fn main() {
    let code = arc_legacy_bridge::main_entry(std::env::args_os().collect());
    std::process::exit(code);
}
