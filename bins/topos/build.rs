// Embed the target triple the binary is built for, so `topos upgrade` fetches the release asset
// matching this exact platform — no runtime uname guessing.
fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=TOPOS_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
