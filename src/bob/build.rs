fn main() {
    println!("cargo:rerun-if-env-changed=BOB_BUILD_COMMIT");
    let mut version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    if let Some(commit) = std::env::var("BOB_BUILD_COMMIT")
        .ok()
        .filter(|commit| commit.len() >= 7)
    {
        version = format!("{version} ({})", &commit[..7]);
    }
    println!("cargo:rustc-env=BOB_VERSION={version}");
}
