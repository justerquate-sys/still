//! Write standard Win32 resources in Rust, so cross builds need no resource
//! compiler. The operating system receives the same crop icon as the Mac app.
use std::{env, fs, path::PathBuf};

fn word(out: &mut Vec<u8>, value: u16) {
    out.extend(value.to_le_bytes());
}
fn dword(out: &mut Vec<u8>, value: u32) {
    out.extend(value.to_le_bytes());
}
fn align(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}
fn resource(out: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
    align(out);
    dword(out, data.len() as u32);
    dword(out, 32);
    for value in [0xffff, kind, 0xffff, id] {
        word(out, value);
    }
    dword(out, 0);
    word(out, if kind == 0 { 0 } else { 0x1030 });
    word(out, 0); // Language neutral.
    dword(out, 0);
    dword(out, 0);
    out.extend(data);
    align(out);
}
fn main() {
    println!("cargo:rerun-if-changed=Resources/Still.icns");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut out = Vec::new();
    resource(&mut out, 0, 0, &[]); // Required null resource header.

    let icon = include_bytes!("Resources/Still.icns");
    let mut images = Vec::new();
    let mut offset = 8;
    while offset + 8 <= icon.len() {
        let length = u32::from_be_bytes(icon[offset + 4..offset + 8].try_into().unwrap()) as usize;
        assert!(
            length >= 8 && offset + length <= icon.len(),
            "Invalid ICNS resource"
        );
        let data = &icon[offset + 8..offset + length];
        if data.len() >= 24 && data.starts_with(b"\x89PNG\r\n\x1a\n") {
            let width = u32::from_be_bytes(data[16..20].try_into().unwrap());
            let height = u32::from_be_bytes(data[20..24].try_into().unwrap());
            if (16..=256).contains(&width)
                && width == height
                && !images.iter().any(|(w, _)| *w == width)
            {
                images.push((width, data));
            }
        }
        offset += length;
    }
    images.sort_by_key(|(width, _)| *width);
    assert!(!images.is_empty(), "Still needs a PNG icon resource");
    let mut group = Vec::new();
    for value in [0, 1, images.len() as u16] {
        word(&mut group, value);
    }
    for (index, (size, data)) in images.into_iter().enumerate() {
        let id = index as u16 + 1;
        group.extend([size as u8, size as u8, 0, 0]);
        word(&mut group, 1);
        word(&mut group, 32);
        dword(&mut group, data.len() as u32);
        word(&mut group, id);
        resource(&mut out, 3, id, data);
    }
    resource(&mut out, 14, 1, &group);

    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let components: Vec<u32> = version.split('.').map(|v| v.parse().unwrap()).collect();
    let mut info = vec![0u8; 6];
    for value in "VS_VERSION_INFO".encode_utf16().chain(Some(0)) {
        word(&mut info, value);
    }
    align(&mut info);
    let high = components[0] << 16 | components[1];
    let low = components[2] << 16;
    for value in [
        0xfeef04bd, 0x10000, high, low, high, low, 0x3f, 0, 0x40004, 1, 0, 0, 0,
    ] {
        dword(&mut info, value);
    }
    let length = info.len() as u16;
    info[..2].copy_from_slice(&length.to_le_bytes());
    info[2..4].copy_from_slice(&52u16.to_le_bytes());
    resource(&mut out, 16, 1, &info);

    let manifest = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="{version}.0" processorArchitecture="*" name="Still" type="win32"/>
  <description>Still screenshot utility</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges>
    <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
  </requestedPrivileges></security></trustInfo>
  <application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings>
    <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
    <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
  </windowsSettings></application>
</assembly>"#
    );
    resource(&mut out, 24, 1, manifest.as_bytes());
    let file = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("Still.res");
    fs::write(&file, out).expect("write Windows resources");
    println!("cargo:rustc-link-arg-bin=Still={}", file.display());
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bin=Still=/MANIFEST:NO");
    }
}
