//! The glibc 2.34 floor for RealTimeX's native executables on Linux: newer glibc symbols they
//! would otherwise reference, defined in glibc_compat.c (see build.rs). Nothing to call: an
//! executable links it with `use rtx_glibc_compat as _;`.
