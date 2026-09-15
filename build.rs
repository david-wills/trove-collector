fn main() {
    // Embed Info.plist into the binary so macOS treats this bare binary as
    // its own TCC "responsible process" with usage strings (the Screen
    // Recording pane needs a bundle identifier to key the grant to). The
    // section survives re-signing in scripts/build.sh (it is part of the
    // linked image).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
        println!(
            "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
        println!("cargo:rerun-if-changed=Info.plist");
    }
}
