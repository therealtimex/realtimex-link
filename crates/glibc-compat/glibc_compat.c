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
 * glibc 2.35 versioned hypot and hypotf again (faster, still correctly rounded). rtx-link's window
 * (egui) calls them. Defined here instead: the largest magnitude times sqrt(1 + r^2), r <= 1, so
 * nothing overflows; within an ulp or two, enough for layout. (Forwarding to the earlier versioned
 * symbol bound back to these very definitions and looped forever.)
 */
double hypot(double x, double y) {
  x = __builtin_fabs(x);
  y = __builtin_fabs(y);
  if (__builtin_isinf(x) || __builtin_isinf(y)) return __builtin_inf();
  if (__builtin_isnan(x) || __builtin_isnan(y)) return __builtin_nan("");
  if (x < y) {
    double t = x;
    x = y;
    y = t;
  }
  if (x == 0.0) return 0.0;
  double r = y / x;
  return x * __builtin_sqrt(1.0 + r * r);
}

float hypotf(float x, float y) {
  /* In double, x*x + y*y neither overflows nor loses a float's precision. */
  double dx = x, dy = y;
  if (__builtin_isinf(dx) || __builtin_isinf(dy)) return __builtin_inff();
  return (float)__builtin_sqrt(dx * dx + dy * dy);
}
