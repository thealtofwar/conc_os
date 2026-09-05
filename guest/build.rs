fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=-T{}/guest.ld", dir.replace('\\', "/"));
    println!("cargo:rerun-if-changed=guest.ld");
    println!("cargo:rerun-if-changed=build.rs");
}
