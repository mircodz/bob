fn main() {
    println!("cargo:rerun-if-env-changed=BOB_BUILD_NUMBER");
    println!("cargo:rerun-if-env-changed=BOB_BUILD_COMMIT");
    let declared = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let mut version = match std::env::var("BOB_BUILD_NUMBER") {
        Ok(number) => {
            let (series, _) = declared
                .rsplit_once('.')
                .expect("the workspace version has a patch number");
            format!("{series}.{number}")
        }
        Err(_) => declared,
    };
    if let Ok(commit) = std::env::var("BOB_BUILD_COMMIT") {
        version = format!("{version} ({})", commit.get(..7).unwrap_or(&commit));
    }
    println!("cargo:rustc-env=BOB_VERSION={version}");
}
