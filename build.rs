fn main() {
    // Embeds assets/rustcord.ico as icon resource 1 (exe icon + tray icon in notify.rs).
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/rustcord.rc");
        println!("cargo:rerun-if-changed=assets/rustcord.ico");
        embed_resource::compile("assets/rustcord.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("compiling assets/rustcord.rc");
    }
}
