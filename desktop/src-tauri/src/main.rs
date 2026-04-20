// Prevent the Windows release build from spawning a console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    arc_desktop_lib::run()
}
