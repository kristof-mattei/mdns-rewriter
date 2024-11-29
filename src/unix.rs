use std::os::fd::AsRawFd;

use libc::c_int;
use socket2::Socket;

/// Helper macro to execute a system call that returns an `io::Result`.
macro_rules! syscall {
    ($fn: ident ( $($arg: expr_2021),* $(,)* ) ) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { libc::$fn($($arg, )*) };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

/// Caller must ensure `T` is the correct type for `opt` and `val`.
#[expect(clippy::needless_pass_by_value)]
pub(crate) unsafe fn setsockopt<T>(
    fd: &Socket,
    opt: libc::c_int,
    val: c_int,
    payload: T,
) -> std::io::Result<()> {
    let payload = std::ptr::addr_of!(payload).cast();

    #[expect(clippy::cast_possible_truncation)]
    syscall!(setsockopt(
        fd.as_raw_fd(),
        opt,
        val,
        payload,
        std::mem::size_of::<T>() as libc::socklen_t,
    ))
    .map(|_| ())
}
