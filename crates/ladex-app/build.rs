use std::path::Path;

fn main() {
    let built_page = Path::new("../../ui/dist/index.html");
    assert!(
        built_page.exists(),
        "the app embeds the built web UI: run `npm ci && npm run build` in ui/ first (this build looked for {})",
        built_page.display()
    );
    println!("cargo:rerun-if-changed=../../ui/dist");
    tauri_build::build();
}
