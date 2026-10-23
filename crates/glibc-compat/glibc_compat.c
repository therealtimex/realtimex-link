/*
 * Keeps RealTimeX's native executables (rtx-capture, rtx-control, rtx-linkd, rtx-link) built on a
 * newer system (the arm64 runner: Ubuntu 24.04, glibc 2.39) running on glibc 2.34. Every symbol
 * version a binary references is a hard requirement at load time, weak references included, so
 * each newer symbol is defined here instead of taken from glibc. Linked into every executable that
 * depends on the rtx-glibc-compat crate (its build.rs), whole, since nothing in Rust references
 * it.
 *
 * glibc 2.38 renamed strtol and sscanf to __isoc23_* for C code built with _GNU_SOURCE (C23 adds
 * "0b" binary input). aws-lc (DTLS for rtx-control's live view, through str0m) is C that defines
 * _GNU_SOURCE. These forward to the classic functions: aws-lc only reads decimal and hex numbers
 * with them (RSA settings, the CPU capability variables), so binary input is never needed. Where
 * there is no aws-lc they are unused.
 *
 * Built as plain C11 (no _GNU_SOURCE) so these calls are not renamed again.
 */
#undef _GNU_SOURCE
#include <errno.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

long __isoc23_strtol(const char *nptr, char **endptr, int base) {
  return strtol(nptr, endptr, base);
}

int __isoc23_sscanf(const char *str, const char *format, ...) {
  va_list args;
  va_start(args, format);
  int n = vsscanf(str, format, args);
  va_end(args);
  return n;
}

/*
 * glibc 2.39. Rust's std links these weakly and calls them only for a child process asked to
 * create a pidfd (an unstable option nothing here uses). ENOSYS is "not available": std then
 * spawns the ordinary way. The file-action and attribute parameters are only passed through, so
 * they are declared as plain pointers (same ABI) and the spawn header is not needed.
 */
int pidfd_spawnp(int *pidfd, const char *file, const void *file_actions, const void *attrp,
                 char *const argv[], char *const envp[]) {
  (void)pidfd;
  (void)file;
  (void)file_actions;
  (void)attrp;
  (void)argv;
  (void)envp;
  return ENOSYS;
}

int pidfd_getpid(int pidfd) {
  (void)pidfd;
  errno = ENOSYS;
  return -1;
}

/*
 * glibc 2.35 versioned hypot and hypotf again (faster, still correctly rounded); the earlier
 * versions stay in every glibc with the same meaning. rtx-link's window (egui) calls them. These
 * definitions forward to the earlier versions, so the binary references only those.
 */
#if defined(__x86_64__)
#define RTX_LIBM_BASE "GLIBC_2.2.5"
#elif defined(__aarch64__)
#define RTX_LIBM_BASE "GLIBC_2.17"
#endif
#ifdef RTX_LIBM_BASE
double rtx_hypot_base(double x, double y);
float rtx_hypotf_base(float x, float y);
__asm__(".symver rtx_hypot_base, hypot@" RTX_LIBM_BASE);
__asm__(".symver rtx_hypotf_base, hypotf@" RTX_LIBM_BASE);

double hypot(double x, double y) {
  return rtx_hypot_base(x, y);
}

float hypotf(float x, float y) {
  return rtx_hypotf_base(x, y);
}
#endif
