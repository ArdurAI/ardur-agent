/* Test-only macOS syscall faults. Only the armed temporary deny file is affected.
 * The Rust FileDenyList and std::fs::File types are never replaced. */
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <unistd.h>

#define INTERPOSE(replacement, original) \
    __attribute__((used)) static const struct { \
        const void *replacement; const void *original; \
    } interpose_##original __attribute__((section("__DATA,__interpose"))) = { \
        (const void *)&replacement, (const void *)&original \
    }

static void path_for(char *path, size_t size, const char *name) {
    const char *dir = getenv("FILE_DENY_TEST_DIR");
    int length = snprintf(path, size, "%s/%s", dir ? dir : "", name);
    if (length < 0 || (size_t)length >= size) _exit(90);
}

static int exists(const char *name) {
    char path[4096];
    path_for(path, sizeof(path), name);
    return access(path, F_OK) == 0;
}

static void mark(const char *name) {
    char path[4096];
    path_for(path, sizeof(path), name);
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd < 0 || close(fd) != 0) _exit(91);
}

static int target(int fd) {
    char path[4096];
    struct stat actual, expected;
    if (!getenv("FILE_DENY_TEST_DIR") || !exists("fault.armed")) return 0;
    path_for(path, sizeof(path), "deny.hex");
    return fstat(fd, &actual) == 0 && stat(path, &expected) == 0 &&
        actual.st_dev == expected.st_dev && actual.st_ino == expected.st_ino;
}

static int mode_is(const char *mode) {
    const char *configured = getenv("FILE_DENY_FAULT_MODE");
    return configured && strcmp(configured, mode) == 0;
}

static void audit_exclusive_lock(void) {
    char path[4096];
    path_for(path, sizeof(path), "deny.hex");
    int probe = open(path, O_RDWR);
    if (probe < 0) _exit(92);
    /* A shared probe conflicts only with an exclusive lock on another handle. */
    if (flock(probe, LOCK_SH | LOCK_NB) == 0) {
        mark("write.unlocked");
        if (flock(probe, LOCK_UN) != 0) _exit(93);
    } else if (errno != EWOULDBLOCK) {
        _exit(94);
    }
    if (close(probe) != 0) _exit(95);
}

static ssize_t fault_write(int fd, const void *buffer, size_t size) {
    if (target(fd)) {
        if (mode_is("write-eio")) {
            mark("fault.hit");
            errno = EIO;
            return -1;
        }
        if (mode_is("partial-eio")) {
            if (!exists("fault.hit")) {
                mark("fault.hit");
                return write(fd, buffer, size > 1 ? size / 2 : size);
            }
            errno = EIO;
            return -1;
        }
        if (mode_is("short-write")) {
            mark("fault.hit");
            audit_exclusive_lock();
            /* Exercise write_all's retry loop, without modifying the bytes. */
            return write(fd, buffer, size > 5 ? 5 : size);
        }
    }
    return write(fd, buffer, size);
}
INTERPOSE(fault_write, write);

static ssize_t fault_read(int fd, void *buffer, size_t size) {
    if (target(fd) && mode_is("read-eio")) {
        mark("fault.hit");
        errno = EIO;
        return -1;
    }
    return read(fd, buffer, size);
}
INTERPOSE(fault_read, read);

static int fault_flock(int fd, int operation) {
    if (target(fd) && mode_is("lock-eio") && operation != LOCK_UN) {
        mark("fault.hit");
        errno = EIO;
        return -1;
    }
    return flock(fd, operation);
}
INTERPOSE(fault_flock, flock);

static int fault_fcntl(int fd, int command, ...) {
    /* On this platform std::fs::File::sync_all uses F_FULLFSYNC. */
    if (command == F_FULLFSYNC && target(fd)) {
        if (mode_is("sync-eio")) {
            mark("fault.hit");
            errno = EIO;
            return -1;
        }
        if (mode_is("sync-pause")) {
            mark("sync.entered");
            for (int attempts = 0; attempts < 4000; attempts++) {
                if (exists("sync.release")) return fcntl(fd, command);
                usleep(5000);
            }
            errno = ETIMEDOUT;
            return -1;
        }
    }
    /* Preserve the calling convention of the commands used by the Rust process. */
    switch (command) {
        case F_GETFD: case F_GETFL: case F_GETOWN: case F_FULLFSYNC:
            return fcntl(fd, command);
        default: break;
    }
    va_list args;
    va_start(args, command);
    int result;
    switch (command) {
        case F_SETFD: case F_SETFL: case F_SETOWN: case F_DUPFD:
        case F_DUPFD_CLOEXEC: {
            int value = va_arg(args, int);
            result = fcntl(fd, command, value);
            break;
        }
        default: {
            void *value = va_arg(args, void *);
            result = fcntl(fd, command, value);
            break;
        }
    }
    va_end(args);
    return result;
}
INTERPOSE(fault_fcntl, fcntl);
