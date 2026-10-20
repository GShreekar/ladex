//! Builds the web UI in `ui/` and stages it in OUT_DIR for `main.rs` to embed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

const PLACEHOLDER_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>LADEX</title>\
<p style=\"font-family:sans-serif;padding:2rem\">This build of LADEX has no web UI: it was built without Node.js. \
Install Node 20 or newer and rebuild.</p>";

fn main() {
    let ui_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../ui");
    let staged = PathBuf::from(env::var("OUT_DIR").unwrap()).join("ui");
    for watched in ["src", "public", "index.html", "join.html", "package.json", "package-lock.json", "vite.config.ts"] {
        println!("cargo:rerun-if-changed={}", ui_dir.join(watched).display());
    }
    println!("cargo:rerun-if-env-changed=LADEX_SKIP_UI_BUILD");

    let is_release = env::var("PROFILE").is_ok_and(|profile| profile == "release");
    let is_skipped = env::var_os("LADEX_SKIP_UI_BUILD").is_some();

    let built = if is_skipped { false } else { build_ui(&ui_dir, is_release) };
    let _ = fs::remove_dir_all(&staged);
    let dist = ui_dir.join("dist");
    if built || (is_skipped && dist.join("index.html").exists()) {
        copy_dir(&dist, &staged);
    } else {
        assert!(
            !is_release || is_skipped,
            "a release build needs the web UI: install Node.js, or set LADEX_SKIP_UI_BUILD to build without it"
        );
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("index.html"), PLACEHOLDER_PAGE).unwrap();
        fs::write(staged.join("join.html"), PLACEHOLDER_PAGE).unwrap();
    }
}

fn npm(ui_dir: &Path, args: &[&str]) -> Option<bool> {
    let program = if cfg!(windows) { "npm.cmd" } else { "npm" };
    match Command::new(program).args(args).current_dir(ui_dir).status() {
        Ok(status) => Some(status.success()),
        Err(_) => None,
    }
}

// False when Node.js is missing; a failing build is an error, so a broken UI never ships quietly.
fn build_ui(ui_dir: &Path, is_release: bool) -> bool {
    if !ui_dir.join("package.json").exists() {
        return false;
    }
    if !ui_dir.join("node_modules").exists() {
        match npm(ui_dir, &["ci"]) {
            None => {
                println!("cargo:warning=Node.js was not found; building LADEX without its web UI");
                return false;
            }
            Some(false) => panic!("`npm ci` failed in ui/"),
            Some(true) => {}
        }
    }
    match npm(ui_dir, &["run", "build"]) {
        Some(true) => true,
        Some(false) => panic!("`npm run build` failed in ui/"),
        None => {
            assert!(!is_release, "npm disappeared while building the web UI");
            false
        }
    }
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}
