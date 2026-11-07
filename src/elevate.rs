//! A small elevated helper that reads the change journal for the desktop app.
//!
//! The app asks Windows to start a copy of itself with administrator rights and
//! talks to it over a named pipe that only that one copy can join. The helper
//! answers journal questions and nothing else: copying and deleting stay in the
//! unelevated app, and the helper writes no files. It exits when the pipe closes.

#[cfg(windows)]
pub use windows_impl::*;

/// The argument that makes the app run as the helper.
pub const HELPER_ARG: &str = "--journal-helper";

#[cfg(not(windows))]
pub fn start() -> std::io::Result<crate::journal::RemoteHandle> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(not(windows))]
pub fn helper_main(_pipe: &str) -> i32 {
    1
}

#[cfg(windows)]
mod windows_impl {
    use crate::journal::{self, Cursor, Keys, ReadFailure, RemoteHandle, RemoteJournal};
    use serde::{Deserialize, Serialize};
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{self, BufRead, BufReader, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, HANDLE, INVALID_HANDLE_VALUE,
        STILL_ACTIVE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, SetNamedPipeHandleState, PIPE_NOWAIT,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };

    const PIPE_PREFIX: &str = r"\\.\pipe\hayakagami-journal-";
    /// Time for the helper to connect after the permission prompt has closed.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
    const SW_HIDE: i32 = 0;

    #[derive(Serialize, Deserialize)]
    struct Request {
        source: PathBuf,
        from: Cursor,
        until: i64,
    }

    fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain(Some(0)).collect()
    }

    fn pipe_name() -> String {
        format!(
            "{PIPE_PREFIX}{}-{}",
            std::process::id(),
            crate::cache::new_run_id()
        )
    }

    struct Client {
        reader: BufReader<File>,
        writer: File,
    }

    struct Remote(Mutex<Option<Client>>);

    impl Remote {
        fn ask(client: &mut Client, request: &Request) -> io::Result<Result<Keys, ReadFailure>> {
            let mut line = serde_json::to_vec(request)?;
            line.push(b'\n');
            client.writer.write_all(&line)?;
            let mut answer = String::new();
            if client.reader.read_line(&mut answer)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            Ok(serde_json::from_str(&answer)?)
        }
    }

    impl RemoteJournal for Remote {
        fn read(&self, source: &Path, from: Cursor, until: i64) -> Result<Keys, ReadFailure> {
            let request = Request {
                source: source.to_path_buf(),
                from,
                until,
            };
            let mut guard = self.0.lock().unwrap();
            let answer = match guard.as_mut() {
                Some(client) => Remote::ask(client, &request),
                None => Err(io::ErrorKind::NotConnected.into()),
            };
            answer.unwrap_or_else(|e| {
                // The helper is gone. Later scans fall straight back to listing.
                *guard = None;
                Err(ReadFailure {
                    reason: crate::trf!(
                        "the change journal helper stopped: {e}",
                        "変更ジャーナルの補助プロセスが停止しました: {e}"
                    ),
                    denied: false,
                })
            })
        }
    }

    /// Shows the Windows permission prompt and connects to the elevated helper.
    /// An error includes the user declining the prompt.
    pub fn start() -> io::Result<RemoteHandle> {
        let name = pipe_name();
        let wide_name = wide(OsStr::new(&name));
        let pipe: HANDLE = unsafe {
            CreateNamedPipeW(
                wide_name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                65_536,
                65_536,
                0,
                std::ptr::null(),
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // The File owns the handle from here, closing it on every path.
        let file = unsafe { File::from_raw_handle(pipe) };

        let exe = std::env::current_exe()?;
        let exe = wide(exe.as_os_str());
        let verb = wide(OsStr::new("runas"));
        let args = wide(OsStr::new(&format!("{} {name}", super::HELPER_ARG)));
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = exe.as_ptr();
        info.lpParameters = args.as_ptr();
        info.nShow = SW_HIDE;
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = info.hProcess;
        let connected = wait_for_helper(pipe, process);
        unsafe {
            CloseHandle(process);
        }
        connected?;

        let mode = PIPE_WAIT;
        let ok =
            unsafe { SetNamedPipeHandleState(pipe, &mode, std::ptr::null(), std::ptr::null()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let client = Client {
            reader: BufReader::new(file.try_clone()?),
            writer: file,
        };
        Ok(RemoteHandle(Arc::new(Remote(Mutex::new(Some(client))))))
    }

    fn wait_for_helper(pipe: HANDLE, process: HANDLE) -> io::Result<()> {
        let started = Instant::now();
        loop {
            if unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) } != 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error().map(|c| c as u32) {
                Some(ERROR_PIPE_CONNECTED) => return Ok(()),
                Some(ERROR_PIPE_LISTENING) => {}
                _ => return Err(error),
            }
            let mut code = 0u32;
            let exited = unsafe { GetExitCodeProcess(process, &mut code) } != 0
                && code != STILL_ACTIVE as u32;
            if exited || started.elapsed() > CONNECT_TIMEOUT {
                return Err(io::ErrorKind::TimedOut.into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Runs as the elevated copy. Answers until the app closes the pipe.
    pub fn helper_main(pipe: &str) -> i32 {
        if !pipe.starts_with(PIPE_PREFIX) {
            return 2;
        }
        let Ok(mut writer) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe)
        else {
            return 1;
        };
        let Ok(reader) = writer.try_clone() else {
            return 1;
        };
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else {
                break;
            };
            let Ok(request) = serde_json::from_str::<Request>(&line) else {
                break;
            };
            let answer = journal::read_changes(&request.source, request.from, request.until);
            let Ok(mut bytes) = serde_json::to_vec(&answer) else {
                break;
            };
            bytes.push(b'\n');
            if writer.write_all(&bytes).is_err() {
                break;
            }
        }
        0
    }
}
