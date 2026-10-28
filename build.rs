// No build-time work needed.
//
// WinFsp requires delay-load linking (winfsp-x64.dll), but the `winfsp-sys`
// crate's own build script already emits those MSVC linker directives
// (`-l delayimp`, `/DELAYLOAD:winfsp-x64.dll`) when the winfsp deps are built.
// We deliberately do NOT duplicate them here — doing so caused no error, but
// it's redundant and would risk double directives.
fn main() {}
