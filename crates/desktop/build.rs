//! Embeds the app icon in the Windows executable, so Explorer, pinned
//! taskbar entries and the window itself (`Icon::from_resource`) show it.
//!
//! The icon is written as a compiled resource file (`.res`) by hand, which
//! MSVC's linker takes directly: an `RT_GROUP_ICON` directory with id 1 that
//! points at one PNG-compressed `RT_ICON` per size in `assets/icon/`.

use std::path::PathBuf;
use std::{env, fs};

/// The pre-rendered sizes in `assets/icon/<size>.png`.
const SIZES: [u16; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
/// `RT_ICON`.
const RT_ICON: u16 = 3;
/// `RT_GROUP_ICON`.
const RT_GROUP_ICON: u16 = 14;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        // ponytail: MSVC only; the GNU linker needs the .res turned into a COFF object (windres) first.
        return;
    }
    // The empty entry every .res file starts with.
    let mut res = Vec::new();
    entry(&mut res, 0, 0, &[]);
    let mut group = vec![0, 0, 1, 0];
    group.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    for (id, size) in (1..).zip(SIZES) {
        let png = fs::read(format!("assets/icon/{size}.png")).expect("the icon PNGs are checked in");
        entry(&mut res, RT_ICON, id, &png);
        // GRPICONDIRENTRY: 256 is written as 0.
        let side = if size >= 256 { 0 } else { size as u8 };
        group.extend_from_slice(&[side, side, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(&32u16.to_le_bytes());
        group.extend_from_slice(&(png.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut res, RT_GROUP_ICON, 1, &group);
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join("icon.res");
    fs::write(&out, res).expect("OUT_DIR is writable");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// Appends one resource: its 32-byte header (numeric type and name, neutral
/// language, moveable and discardable), then `data` padded to 4 bytes.
fn entry(res: &mut Vec<u8>, kind: u16, name: u16, data: &[u8]) {
    let flags: u16 = if kind == 0 { 0 } else { 0x1030 };
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes());
    for part in [0xFFFF, kind, 0xFFFF, name] {
        res.extend_from_slice(&part.to_le_bytes());
    }
    res.extend_from_slice(&0u32.to_le_bytes());
    res.extend_from_slice(&flags.to_le_bytes());
    res.extend_from_slice(&0u16.to_le_bytes());
    res.extend_from_slice(&[0; 8]);
    res.extend_from_slice(data);
    res.resize(res.len().next_multiple_of(4), 0);
}
