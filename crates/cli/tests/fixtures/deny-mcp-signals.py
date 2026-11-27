"""Launch the first PTY with both MCP signal routes refused, retaining kill(pid, 0)."""
import ctypes
import errno
import os
import signal
import sys


class Comparison(ctypes.Structure):
    _fields_ = [("arg", ctypes.c_uint), ("op", ctypes.c_int),
                ("value", ctypes.c_uint64), ("mask", ctypes.c_uint64)]


lib = ctypes.CDLL("libseccomp.so.2")
lib.seccomp_init.argtypes = [ctypes.c_uint32]
lib.seccomp_init.restype = ctypes.c_void_p
lib.seccomp_syscall_resolve_name.argtypes = [ctypes.c_char_p]
lib.seccomp_rule_add_array.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int,
                                      ctypes.c_uint, ctypes.POINTER(Comparison)]
lib.seccomp_load.argtypes = [ctypes.c_void_p]
ctx = lib.seccomp_init(0x7fff0000)  # SCMP_ACT_ALLOW
if not ctx:
    raise RuntimeError("cannot create seccomp context")
for name, comparison in [(b"pidfd_open", None),
                         (b"kill", Comparison(1, 4, signal.SIGTERM, 0)),
                         (b"kill", Comparison(1, 4, signal.SIGKILL, 0))]:
    number = lib.seccomp_syscall_resolve_name(name)
    count = int(comparison is not None)
    rc = lib.seccomp_rule_add_array(ctx, 0x50000 | errno.EPERM, number, count,
                                    ctypes.byref(comparison) if count else None)
    if rc:
        raise OSError(-rc, os.strerror(-rc))
rc = lib.seccomp_load(ctx)
if rc:
    raise OSError(-rc, os.strerror(-rc))
os.execv(sys.argv[1], sys.argv[1:])
