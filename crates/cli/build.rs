fn main() {
    // Windows reserves 1 MiB for the main thread, which the Clap command tree
    // for this CLI exceeds in debug builds. Match the 8 MiB used by help.
    #[cfg(all(windows, target_env = "msvc"))]
    println!("cargo:rustc-link-arg-bins=/STACK:8388608");
}
