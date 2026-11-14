//! Renders the logo SVG to PNGs and a multi-size Windows .ico.
//!
//!     cargo run --example export_icons [-- path/to/logo.svg [out_dir]]
//!
//! Sizes <= 32 px use `logo-small.svg` when it exists next to the main SVG (a simplified mark
//! that stays legible at tiny sizes). `logo-mono.svg` next to it (the flat mark) becomes
//! `rustcord-tray.ico`, the notification-area icon (resource id 2, notify.rs), and
//! `icons/rustcord-tray-36.png`, the macOS menu bar template image (black + alpha).

use resvg::{tiny_skia, usvg};
use std::path::{Path, PathBuf};

/// 1024: the macOS iconset's icon_512x512@2x (scripts/build-macos.sh).
const PNG_SIZES: [u32; 8] = [16, 32, 48, 64, 128, 256, 512, 1024];
/// macOS menu bar icon: 18 pt at 2x.
const MENU_BAR_SIZE: u32 = 36;
const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];
/// The small-icon size at 100-300 % scaling.
const TRAY_SIZES: [u32; 6] = [16, 20, 24, 32, 40, 48];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let svg = PathBuf::from(args.get(1).map(String::as_str).unwrap_or("assets/logo.svg"));
    let out = PathBuf::from(args.get(2).map(String::as_str).unwrap_or("assets"));
    let small = svg.with_file_name("logo-small.svg");

    let big_tree = load(&svg)?;
    let small_tree = if small.exists() { Some(load(&small)?) } else { None };
    let tree_for = |size: u32| if size <= 32 { small_tree.as_ref().unwrap_or(&big_tree) } else { &big_tree };

    std::fs::create_dir_all(out.join("icons"))?;
    for size in PNG_SIZES {
        let rgba = render(tree_for(size), size)?;
        let path = out.join("icons").join(format!("rustcord-{size}.png"));
        image::save_buffer(&path, &rgba, size, size, image::ExtendedColorType::Rgba8)?;
        println!("wrote {}", path.display());
    }

    write_ico(&out.join("rustcord.ico"), &ICO_SIZES, tree_for)?;
    let mono = svg.with_file_name("logo-mono.svg");
    if mono.exists() {
        let mono_tree = load(&mono)?;
        write_ico(&out.join("rustcord-tray.ico"), &TRAY_SIZES, |_| &mono_tree)?;
        // A template image: macOS only reads the alpha channel and tints it for the menu bar.
        let mut rgba = render(&mono_tree, MENU_BAR_SIZE)?;
        for px in rgba.as_chunks_mut::<4>().0 {
            px[..3].fill(0);
        }
        let path = out.join("icons").join(format!("rustcord-tray-{MENU_BAR_SIZE}.png"));
        image::save_buffer(&path, &rgba, MENU_BAR_SIZE, MENU_BAR_SIZE, image::ExtendedColorType::Rgba8)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

fn write_ico<'a>(path: &Path, sizes: &[u32], tree_for: impl Fn(u32) -> &'a usvg::Tree) -> Result<(), Box<dyn std::error::Error>> {
    let mut frames = Vec::new();
    for &size in sizes {
        let rgba = render(tree_for(size), size)?;
        frames.push(image::codecs::ico::IcoFrame::as_png(&rgba, size, size, image::ExtendedColorType::Rgba8)?);
    }
    image::codecs::ico::IcoEncoder::new(std::fs::File::create(path)?).encode_images(&frames)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn load(path: &Path) -> Result<usvg::Tree, Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    Ok(usvg::Tree::from_data(&data, &usvg::Options::default())?)
}

/// Straight (non-premultiplied) RGBA, `size` x `size`.
fn render(tree: &usvg::Tree, size: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or("bad size")?;
    let s = tree.size();
    let scale = size as f32 / s.width().max(s.height());
    resvg::render(tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Ok(pixmap.take_demultiplied())
}
