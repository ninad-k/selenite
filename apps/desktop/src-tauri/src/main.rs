//! Binary entry point for the desktop shell.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(err) = selenite_desktop_lib::run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
