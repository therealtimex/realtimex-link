fn main() {
    // glibc symbols newer than 2.34 (aws-lc's __isoc23_strtol/sscanf, Rust std's weak pidfd_*),
    // defined so a binary built on a newer system still runs on glibc 2.34 (glibc_compat.c).
    // Whole archive: nothing in Rust references them, and aws-lc's objects come later on the
    // link line.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu")
    {
        println!("cargo:rerun-if-changed=glibc_compat.c");
        cc::Build::new()
            .file("glibc_compat.c")
            .std("c11")
            .link_lib_modifier("+whole-archive")
            .compile("glibc_compat");
    }
}
