fn main() {
    // Embeds assets/rustcord.ico as icon resource 1 (exe, window, balloons) and
    // assets/rustcord-tray.ico as 2 (notification-area icon in notify.rs).
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/rustcord.rc");
        println!("cargo:rerun-if-changed=assets/rustcord.ico");
        println!("cargo:rerun-if-changed=assets/rustcord-tray.ico");
        embed_resource::compile("assets/rustcord.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("compiling assets/rustcord.rc");
        static_vcruntime();
    }
}

/// With `+crt-static` (.cargo/config.toml) rustc links the static CRT, but prebuilt C++ objects
/// (WebView2's loader) still name the DLL runtime; this swaps all of it for the static libs, the
/// way Tauri's `static_vcruntime` does. Result: no `vcruntime140*.dll` import.
#[cfg(windows)]
fn static_vcruntime() {
    let features = std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    if !features.split(',').any(|f| f == "crt-static") || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    for lib in ["libvcruntimed", "vcruntime", "vcruntimed", "libcmtd", "msvcrt", "msvcrtd", "libucrt", "libucrtd"] {
        println!("cargo:rustc-link-arg-bins=/NODEFAULTLIB:{lib}.lib");
    }
    for lib in ["libcmt", "libvcruntime", "ucrt"] {
        println!("cargo:rustc-link-arg-bins=/DEFAULTLIB:{lib}.lib");
    }
}
