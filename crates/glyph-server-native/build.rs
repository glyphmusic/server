fn main() {
    // Target triple of this build, used to pick the matching release asset.
    println!("cargo:rustc-env=GLYPH_TARGET={}", std::env::var("TARGET").unwrap());
}
