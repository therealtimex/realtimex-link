fn main() {
    // glibc symbols newer than 2.34 (Rust std's weak pidfd_*), defined so a binary built on a
    // newer system still runs on glibc 2.34, RealTimeX's floor (see ../../glibc/glibc_compat.c).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu")
    {
        println!("cargo:rerun-if-changed=../../glibc/glibc_compat.c");
        cc::Build::new()
            .file("../../glibc/glibc_compat.c")
            .std("c11")
            .link_lib_modifier("+whole-archive")
            .compile("glibc_compat");
    }
}
