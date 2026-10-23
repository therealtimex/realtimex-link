/*
 * Keeps rtx-linkd built on a newer system (the arm64 runner: Ubuntu 24.04, glibc 2.39) running
 * on glibc 2.34, the floor RealTimeX ships for. Every symbol version a binary references is a
 * hard requirement at load time, weak references included, so the newer symbols are defined
 * here instead of taken from glibc. crates/linkd/build.rs compiles it on Linux (gnu).
 *
 * Built as plain C11 (no _GNU_SOURCE).
 */
#include <errno.h>

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
