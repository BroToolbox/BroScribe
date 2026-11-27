//! Links BroScribe's app icon into the Windows (MSVC) executable: the
//! committed resource file `assets/brand/windows/BroScribe.res`, generated
//! with the other brand files by `examples/render_brand.rs`. GPUI loads its
//! icon 1 for the app's windows; Explorer, the taskbar, Start and the
//! notification shortcut show it too. Other targets link nothing.

fn main() {
    let res = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("brand")
        .join("windows")
        .join("BroScribe.res");
    println!("cargo:rerun-if-changed={}", res.display());
    let target = |key: &str| std::env::var(key).unwrap_or_default();
    if target("CARGO_CFG_TARGET_OS") == "windows" && target("CARGO_CFG_TARGET_ENV") == "msvc" {
        // MSVC's linker takes a compiled resource file as an input.
        println!("cargo:rustc-link-arg-bins={}", res.display());
        // The GPUI recording panel's first draw needs more than the default
        // MSVC main-thread stack in unoptimized Windows builds.
        println!("cargo:rustc-link-arg-bin=broscribe-app=/STACK:8388608");
    }
}
