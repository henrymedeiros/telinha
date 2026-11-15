//! Compiles the UI and tells the binary where to find the FFmpeg and libva
//! bundled with it.

fn main() {
    // The UI (ui/app.slint and what it imports).
    slint_build::compile("ui/app.slint").expect("compile the UI");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" {
        // Packages: libraries in lib/ next to the binary (AppImage, portable)
        // or in /usr/lib/telinha (.deb).
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/lib:$ORIGIN/../lib/telinha");
        // Development: the copies downloaded into .deps.
        if std::env::var("PROFILE").as_deref() == Ok("debug") || std::env::var("TELINHA_DEV_RPATH").is_ok() {
            let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
            println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}/.deps/libva/lib:{dir}/.deps/ffmpeg-linux/lib");
        }
    }
    if target_os == "windows" {
        // Icon and version info for the .exe. The version is Cargo.toml's: the
        // resource wants it as text and as four numbers.
        let version = std::env::var("CARGO_PKG_VERSION").unwrap();
        let mut numbers = version.split(['.', '-', '+']).map(|p| p.parse::<u16>().unwrap_or(0));
        let mut next = || numbers.next().unwrap_or(0);
        let numbers = format!("TELINHA_VERSION_NUMBERS={},{},{},0", next(), next(), next());
        println!("cargo:rerun-if-changed=assets/windows/telinha.rc");
        embed_resource::compile("assets/windows/telinha.rc", [format!("TELINHA_VERSION={version}"), numbers]).manifest_optional().unwrap();
    }
    println!("cargo:rerun-if-env-changed=TELINHA_DEV_RPATH");
    // Embedded default server (option_env!): changing the variable triggers a rebuild.
    println!("cargo:rerun-if-env-changed=TELINHA_SERVER");
    // Public key that signs the updates (update.rs); without it the app does not update itself.
    println!("cargo:rerun-if-env-changed=TELINHA_UPDATE_KEY");
}
