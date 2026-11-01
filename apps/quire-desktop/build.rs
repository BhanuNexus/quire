// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Embed the icon and version info in Windows executables.

fn main() {
    println!("cargo:rerun-if-changed=../../packaging/icons/quire.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../packaging/icons/quire.ico");
    res.set("ProductName", "Quire");
    res.set("FileDescription", "Quire");
    res.set("LegalCopyright", "© 2026 Quire contributors · AGPL-3.0-or-later");
    if let Err(e) = res.compile() {
        // Packaging still works without resources; the exe just has no icon.
        println!("cargo:warning=Windows resources not embedded: {e}");
    }
}
