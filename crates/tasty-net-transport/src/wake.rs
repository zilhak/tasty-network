//! Closing a TCP socket both ways so that a read blocked on it in another thread returns.
//!
//! On Linux and macOS `shutdown(Both)` returns a blocked `recv` at once. On Windows it does
//! not, so after the shutdown the pending I/O of the socket is cancelled as well: the blocked
//! read returns an error (observed `WSAEINTR`, 10004) and a later read sees EOF. Cancelling
//! may end a write in progress too, so it is used only where the reader is to end, never for
//! a half close (`tasty:crates/tasty-utils/src/socket_wake.rs`).

use std::io;
use std::net::{Shutdown, TcpStream};

/// `shutdown(Both)`, returning its result. On Windows the pending I/O of the socket is then
/// cancelled whatever that result; the cancel result is not mixed into the return value. The
/// cancel does not wait for completion, so the reader may not have returned yet.
pub(crate) fn shutdown_both_and_wake(stream: &TcpStream) -> io::Result<()> {
    let shut = stream.shutdown(Shutdown::Both);
    #[cfg(windows)]
    if let Err(error) = cancel_pending_io(stream) {
        tracing::debug!(%error, "cancelling pending socket I/O after shutdown failed");
    }
    shut
}

/// No request to cancel (`ERROR_NOT_FOUND`) counts as success.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "std has no way to cancel a recv blocked in another thread on Windows; the FFI call borrows the socket and is bounded to this function"
)]
fn cancel_pending_io(stream: &TcpStream) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, HANDLE};
    use windows_sys::Win32::System::IO::CancelIoEx;

    let handle = stream.as_raw_socket() as usize as HANDLE;
    // SAFETY: the borrow keeps the socket open for this synchronous call. A SOCKET is a kernel
    // handle and is passed at pointer width. The null OVERLAPPED pointer asks to cancel every
    // pending request on the handle. The call neither takes nor closes it.
    let ok = unsafe { CancelIoEx(handle, std::ptr::null()) };
    if ok != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
        return Ok(());
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::shutdown_both_and_wake;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// OS codes of a read that the cancel returned on Windows: 10004 (`WSAEINTR`, observed)
    /// and 995 (`ERROR_OPERATION_ABORTED`). std maps 995 and the timeout 10060 to the same
    /// `TimedOut`, so the code and the time decide.
    const CANCELLED: &[i32] = if cfg!(windows) { &[10_004, 995] } else { &[] };
    const READ_LIMIT: Duration = Duration::from_secs(5);
    /// Well before the 5 s timeout. A cancel code later than this is not a wake.
    const WOKEN_WITHIN: Duration = Duration::from_secs(1);

    #[test]
    fn shutting_both_sides_ends_a_read_already_blocked_in_another_thread() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let peer = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (socket, _) = listener.accept().expect("accept");
        let reader_copy = socket.try_clone().expect("clone");
        reader_copy
            .set_read_timeout(Some(READ_LIMIT))
            .expect("read timeout");
        let (ready_tx, ready_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut copy = reader_copy;
            let mut byte = [0u8; 1];
            ready_tx.send(()).expect("ready");
            let result = copy.read(&mut byte);
            (Instant::now(), result)
        });
        ready_rx.recv_timeout(READ_LIMIT).expect("reader ready");
        // The ready signal does not prove the thread is inside recv; give it time.
        std::thread::sleep(Duration::from_millis(50));

        let called = Instant::now();
        shutdown_both_and_wake(&socket).expect("shutdown");
        let (ended, result) = reader.join().expect("reader thread");
        let took = ended.saturating_duration_since(called);
        match &result {
            Ok(0) => {}
            Err(error)
                if error
                    .raw_os_error()
                    .is_some_and(|code| CANCELLED.contains(&code))
                    && took < WOKEN_WITHIN => {}
            other => panic!("the blocked read was not woken: {other:?} after {took:?}"),
        }
        // The peer closes after the result, so its FIN is not taken for the wake.
        drop(peer);
    }
}
