//! Named-pipe building blocks of the daemon transport: the per-user pipe
//! name, its current-user-only security descriptor, peer SID checks on both
//! ends, and a deadline-bounded client stream. The OS-neutral protocol
//! client and the async listener live in `broscribe_platform_native::ipc`.
use crate::native::OwnedHandle;
use std::{
    ffi::c_void,
    io::{self, Read, Write},
    mem::size_of,
    os::windows::io::RawHandle,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Security::{Authorization::*, *},
        Storage::FileSystem::*,
        System::{Pipes::*, Threading::*},
    },
    core::{PCWSTR, PWSTR},
};
fn io_error(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error((e.code().0 as u32 & 0xffff) as i32)
}
fn process_sid(process: HANDLE) -> io::Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).map_err(io_error)?;
        let token = OwnedHandle(token);
        let mut len = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut len);
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut data = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(data.as_mut_ptr().cast()),
            len,
            &mut len,
        )
        .map_err(io_error)?;
        let user = &*data.as_ptr().cast::<TOKEN_USER>();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).map_err(io_error)?;
        let result = text
            .to_string()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        result
    }
}
pub fn current_user_sid() -> io::Result<String> {
    unsafe { process_sid(GetCurrentProcess()) }
}
/// The daemon pipe for this user and `data_dir` (one endpoint per data
/// folder, like the Unix socket).
pub fn pipe_name(data_dir: &std::path::Path) -> io::Result<String> {
    Ok(crate::pipe_naming::pipe_name_for(
        &current_user_sid()?,
        data_dir,
    ))
}

/// Observe the first-instance claim without opening or creating a pipe.
pub fn agent_instance_held(data_dir: &std::path::Path) -> io::Result<bool> {
    let name = crate::wide(pipe_name(data_dir)?);
    // SAFETY: the pipe name is NUL-terminated and lives through the call.
    if unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), NMPWAIT_NOWAIT) }.as_bool() {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error().map(|code| code as u32) {
        Some(code) if code == ERROR_SEM_TIMEOUT.0 => Ok(true), // All instances are busy.
        Some(code) if code == ERROR_FILE_NOT_FOUND.0 || code == ERROR_PATH_NOT_FOUND.0 => Ok(false),
        _ => Err(error),
    }
}

/// The desktop app's single-instance pipe for this user and `data_dir`
/// (SHL-14).
pub fn app_pipe_name(data_dir: &std::path::Path) -> io::Result<String> {
    Ok(crate::pipe_naming::app_pipe_name_for(
        &current_user_sid()?,
        data_dir,
    ))
}
fn check_peer(handle: HANDLE, server: bool) -> io::Result<()> {
    unsafe {
        let mut pid = 0;
        if server {
            GetNamedPipeServerProcessId(handle, &mut pid)
        } else {
            GetNamedPipeClientProcessId(handle, &mut pid)
        }
        .map_err(io_error)?;
        let process = OwnedHandle(
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map_err(io_error)?,
        );
        if process_sid(process.0)? != current_user_sid()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named pipe peer is not the current user",
            ));
        }
        Ok(())
    }
}
/// `PermissionDenied` unless the client connected to the server pipe
/// instance `handle` runs as the current user.
pub fn verify_pipe_client(handle: RawHandle) -> io::Result<()> {
    check_peer(HANDLE(handle), false)
}

/// Current-user-only security attributes for pipe instances and private
/// helper folders. Keep the descriptor alive through the create call.
pub struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: Box<SECURITY_ATTRIBUTES>,
}
// SAFETY: the descriptor is process-heap memory owned by this value and
// never thread-affine; Win32 reads it only during the create call.
unsafe impl Send for PipeSecurity {}
impl PipeSecurity {
    pub fn current_user() -> io::Result<Self> {
        Self::current_user_with_inheritance("")
    }

    pub(crate) fn private_directory() -> io::Result<Self> {
        Self::current_user_with_inheritance("OICI")
    }

    fn current_user_with_inheritance(inheritance: &str) -> io::Result<Self> {
        let sid = current_user_sid()?;
        let sddl = crate::wide(format!("D:P(A;{inheritance};GA;;;{sid})"));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(io_error)?;
        let attributes = Box::new(SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        });
        Ok(Self {
            descriptor,
            attributes,
        })
    }
    pub(crate) fn attributes(&self) -> &SECURITY_ATTRIBUTES {
        &self.attributes
    }

    /// The `SECURITY_ATTRIBUTES` pointer for pipe creation.
    pub fn as_ptr(&mut self) -> *mut c_void {
        (&mut *self.attributes as *mut SECURITY_ATTRIBUTES).cast()
    }
}
impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.descriptor.0)));
        }
    }
}

/// Whether a pipe-creation or accept failure is resource exhaustion that
/// only a pause can relieve.
pub fn is_resource_exhaustion(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == ERROR_TOO_MANY_OPEN_FILES.0 as i32
            || code == ERROR_NO_SYSTEM_RESOURCES.0 as i32
            || code == ERROR_NOT_ENOUGH_MEMORY.0 as i32
    )
}

/// Longest pause while polling a nonblocking pipe.
const MAX_POLL: Duration = Duration::from_millis(20);

/// The only constructor authenticates the server's process-token SID before
/// returning; callers cannot accidentally send credentials to an impostor.
/// Reads and writes honour a caller deadline; without one, reads block.
pub struct WinPipeStream {
    handle: OwnedHandle,
    nonblocking: bool,
}
impl WinPipeStream {
    /// Connects to the daemon pipe of `data_dir` within `deadline`. A
    /// missing pipe is `NotFound`.
    pub fn connect(data_dir: &std::path::Path, deadline: Instant) -> io::Result<Self> {
        Self::connect_named(&pipe_name(data_dir)?, deadline)
    }
    /// Connects to the pipe `name` within `deadline`, authenticating its
    /// server's user. A missing pipe is `NotFound`.
    pub fn connect_named(name: &str, deadline: Instant) -> io::Result<Self> {
        let name = crate::wide(name);
        let open = || unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };
        let handle = loop {
            match open() {
                Ok(handle) => break OwnedHandle(handle),
                Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => {
                    let left = remaining(deadline)?;
                    let millis = left.as_millis().clamp(1, u128::from(u32::MAX - 1)) as u32;
                    // A busy pipe that frees no instance in time is a timeout;
                    // a vanished one is reported by the next open.
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), millis) };
                }
                Err(e) => return Err(io_error(e)),
            }
        };
        check_peer(handle.0, true)?;
        let mut stream = Self {
            handle,
            nonblocking: false,
        };
        stream.set_nonblocking(true)?;
        Ok(stream)
    }
    pub fn server_process_id(&self) -> io::Result<u32> {
        let mut pid = 0;
        unsafe { GetNamedPipeServerProcessId(self.handle.0, &mut pid) }.map_err(io_error)?;
        Ok(pid)
    }
    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        if self.nonblocking == nonblocking {
            return Ok(());
        }
        let mode = PIPE_READMODE_BYTE | if nonblocking { PIPE_NOWAIT } else { PIPE_WAIT };
        unsafe { SetNamedPipeHandleState(self.handle.0, Some(&mode), None, None) }
            .map_err(io_error)?;
        self.nonblocking = nonblocking;
        Ok(())
    }
    /// Reads at least one byte (0 at end of stream) before `deadline`;
    /// `None` blocks until data arrives.
    pub fn read_within(
        &mut self,
        bytes: &mut [u8],
        deadline: Option<Instant>,
    ) -> io::Result<usize> {
        let Some(deadline) = deadline else {
            self.set_nonblocking(false)?;
            return read_handle(self.handle.0, bytes, false);
        };
        self.set_nonblocking(true)?;
        let mut pause = Duration::from_millis(1);
        loop {
            match read_handle(self.handle.0, bytes, true) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(pause.min(remaining(deadline)?));
                    pause = (pause * 2).min(MAX_POLL);
                }
                result => return result,
            }
        }
    }
    /// Writes all of `bytes` before `deadline`.
    pub fn write_all_within(&mut self, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            let written = self.write_some_within(bytes, deadline)?;
            bytes = &bytes[written..];
        }
        Ok(())
    }
    /// Writes at least one byte of the nonempty `bytes` before `deadline`
    /// and returns how many; `TimedOut` only when none was written, so a
    /// caller resuming after it never loses or repeats bytes.
    pub fn write_some_within(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize> {
        self.set_nonblocking(true)?;
        let mut pause = Duration::from_millis(1);
        loop {
            match write_handle(self.handle.0, bytes) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(pause.min(remaining(deadline)?));
                    pause = (pause * 2).min(MAX_POLL);
                }
                result => return result,
            }
        }
    }
    /// One response line (lifecycle commands), bounded in time and size.
    pub fn read_line(&mut self, timeout: Duration, max_bytes: usize) -> io::Result<String> {
        let deadline = Instant::now() + timeout;
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if self.read_within(&mut byte, Some(deadline))? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            if byte[0] == b'\n' {
                return String::from_utf8(line)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
            }
            if line.len() >= max_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "named pipe line too large",
                ));
            }
            line.push(byte[0]);
        }
    }
}
impl Read for WinPipeStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.read_within(bytes, None)
    }
}
impl Write for WinPipeStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.set_nonblocking(false)?;
        write_handle(self.handle.0, bytes)
    }
    // Writes are synchronous; FlushFileBuffers would wait for the peer to read
    // and can deadlock a request/response protocol, so it is not a flush here.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "named pipe deadline passed",
        ))
    } else {
        Ok(left)
    }
}

fn read_handle(handle: HANDLE, bytes: &mut [u8], nonblocking: bool) -> io::Result<usize> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let mut read = 0;
    match unsafe { ReadFile(handle, Some(bytes), Some(&mut read), None) } {
        Ok(()) if read == 0 && nonblocking => Err(io::ErrorKind::WouldBlock.into()),
        Ok(()) => Ok(read as usize),
        Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(0),
        Err(e) if e.code() == ERROR_NO_DATA.to_hresult() && nonblocking => {
            Err(io::ErrorKind::WouldBlock.into())
        }
        Err(e) => Err(io_error(e)),
    }
}

/// One write; a nonblocking pipe without room reports `WouldBlock`.
fn write_handle(handle: HANDLE, bytes: &[u8]) -> io::Result<usize> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let mut written = 0;
    match unsafe { WriteFile(handle, Some(bytes), Some(&mut written), None) } {
        Ok(()) if written == 0 => Err(io::ErrorKind::WouldBlock.into()),
        Ok(()) => Ok(written as usize),
        Err(e) if e.code() == ERROR_NO_DATA.to_hresult() => {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe is closing"))
        }
        Err(e) => Err(io_error(e)),
    }
}

/// Switches an anonymous child pipe (stdin of a worker) to nonblocking
/// writes, so a stalled reader never blocks the sender thread.
pub fn set_pipe_nonblocking(handle: RawHandle) -> io::Result<()> {
    let mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
    unsafe { SetNamedPipeHandleState(HANDLE(handle), Some(&mode), None, None) }.map_err(io_error)
}

/// One nonblocking write to a pipe set up by [`set_pipe_nonblocking`].
pub fn write_nonblocking(handle: RawHandle, bytes: &[u8]) -> io::Result<usize> {
    write_handle(HANDLE(handle), bytes)
}
