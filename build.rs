// WinFsp delay-loads winfsp-x64.dll. These linker directives are the MSVC-only
// mechanism the `winfsp` crate's winfsp_link_delayload() emits — we emit them
// ourselves so we don't need winfsp as a build-dependency (which would try to
// compile on the default GNU build and break it).
//
// Gated on BOTH: the `winfsp` cargo feature being enabled (CARGO_FEATURE_WINFSP)
// AND an MSVC target (CARGO_CFG_TARGET_ENV == "msvc"). On the GNU toolchain, or
// without the feature, this is a no-op, so the core crate builds everywhere.
fn main() {
    let winfsp_feature = std::env::var_os("CARGO_FEATURE_WINFSP").is_some();
    let is_msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if winfsp_feature && is_msvc {
        let dll = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
            Ok("x86_64") => "winfsp-x64.dll",
            Ok("aarch64") => "winfsp-a64.dll",
            Ok("x86") => "winfsp-x86.dll",
            _ => "winfsp-x64.dll",
        };
        println!("cargo:rustc-link-lib=dylib=delayimp");
        println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
    }
}
