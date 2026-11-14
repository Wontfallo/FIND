//! The FIND service: how FIND indexes NTFS without running as Administrator.
//!
//! Same design as Everything (found with REA: `CreateServiceW`,
//! `StartServiceCtrlDispatcherW`, and a pipe named `\\.\PIPE\Everything
//! Service`). Opening `\\.\C:` needs admin, so a small Windows service running
//! as LocalSystem does only that part — reading the MFT and the USN journal —
//! and hands the results to the normal, unelevated FIND over a named pipe.
//! Installing it needs one UAC prompt; after that FIND never asks again.
//!
//! Security choices:
//! - The service only answers read-only metadata queries (names, sizes,
//!   timestamps, change records) for local fixed drives. It never reads file
//!   contents and never writes anything.
//! - Like Everything's service, it lets any signed-in user list every file
//!   name on the drive, including in other users' folders. That is inherent
//!   to the feature; the pipe refuses remote and anonymous clients.
//! - The service binary is copied into `%ProgramFiles%\FIND`, which only
//!   administrators can write. Running a SYSTEM service from a user-writable
//!   folder (e.g. `Documents\...\find.exe`) would let any program replace it
//!   and gain SYSTEM.
#![cfg(target_os = "windows")]

use crate::volume::{self, FileRecord, JournalInfo, UsnChange, VResult, VolumeError};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub const SERVICE_NAME: &str = "FINDIndexService";
const DISPLAY_NAME: &str = "FIND Indexing Service";
pub const PIPE_NAME: &str = r"\\.\pipe\FIND Index Service";
/// Bumped when the request/response layout changes. A client talking to an
/// older service gets a clear "reinstall" status instead of garbage.
pub const PROTOCOL_VERSION: u32 = 1;
/// Largest single message accepted from a client (requests are tiny).
const MAX_REQUEST: u32 = 4096;
/// Largest single response a client will accept (a big volume's MFT).
const MAX_RESPONSE: u64 = 4 << 30;

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Ping,
    EnumVolume { letter: char },
    QueryJournal { letter: char },
    ReadJournal {
        letter: char,
        journal_id: u64,
        start_usn: i64,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    Pong { version: u32 },
    Records { records: Vec<FileRecord>, full_metadata: bool },
    Journal(JournalInfo),
    Changes { changes: Vec<UsnChange>, next_usn: i64 },
    Error(VolumeError),
}

// ----------------------------------------------------------------- framing

fn write_frame(w: &mut impl Write, body: &[u8]) -> std::io::Result<()> {
    w.write_all(&(body.len() as u64).to_le_bytes())?;
    w.write_all(body)?;
    w.flush()
}

fn read_frame(r: &mut impl Read, max: u64) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 8];
    r.read_exact(&mut len)?;
    let len = u64::from_le_bytes(len);
    if len > max {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok(body)
}

// ----------------------------------------------------------------- handling

/// Enumerate a volume directly. Prefers the raw MFT (names + sizes + dates);
/// falls back to FSCTL_ENUM_USN_DATA (names only).
pub fn enum_volume_direct(
    letter: char,
    progress: &AtomicUsize,
    cancel: &AtomicBool,
) -> VResult<(Vec<FileRecord>, bool)> {
    match volume::read_mft(letter, progress, cancel) {
        Ok(r) => Ok((r, true)),
        Err(VolumeError::Unavailable(m)) => Err(VolumeError::Unavailable(m)),
        Err(_) => volume::enum_usn_data(letter, progress, cancel).map(|r| (r, false)),
    }
}

fn handle(req: Request) -> Response {
    let progress = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let letter_ok = |l: char| l.is_ascii_alphabetic() && volume::is_fixed_drive(l);
    match req {
        Request::Ping => Response::Pong {
            version: PROTOCOL_VERSION,
        },
        Request::EnumVolume { letter } if letter_ok(letter) => {
            match enum_volume_direct(letter, &progress, &cancel) {
                Ok((records, full_metadata)) => Response::Records {
                    records,
                    full_metadata,
                },
                Err(e) => Response::Error(e),
            }
        }
        Request::QueryJournal { letter } if letter_ok(letter) => {
            match volume::query_journal(letter) {
                Ok(j) => Response::Journal(j),
                Err(e) => Response::Error(e),
            }
        }
        Request::ReadJournal {
            letter,
            journal_id,
            start_usn,
        } if letter_ok(letter) => match volume::read_journal(letter, journal_id, start_usn, 8 << 20)
        {
            Ok((changes, next_usn)) => Response::Changes { changes, next_usn },
            Err(e) => Response::Error(e),
        },
        _ => Response::Error(VolumeError::Unavailable(
            "only local fixed drives are served".into(),
        )),
    }
}

fn serve_client(mut pipe: std::fs::File) {
    // One request per connection keeps the server trivially correct.
    let Ok(body) = read_frame(&mut pipe, MAX_REQUEST as u64) else {
        return;
    };
    let resp = match bincode::deserialize::<Request>(&body) {
        Ok(req) => handle(req),
        Err(_) => Response::Error(VolumeError::Io("bad request".into())),
    };
    if let Ok(bytes) = bincode::serialize(&resp) {
        let _ = write_frame(&mut pipe, &bytes);
    }
    // Let the client drain before the handle closes.
    unsafe {
        use std::os::windows::io::AsRawHandle;
        windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(pipe.as_raw_handle() as _);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Accept clients until `stop` is set. Each connection gets its own thread.
fn pipe_server(stop: &AtomicBool) {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;

    // Read/write for authenticated users, full control for SYSTEM and
    // Administrators. Anonymous and remote clients are refused.
    let sddl = wide("D:(A;;GRGW;;;AU)(A;;GA;;;SY)(A;;GA;;;BA)");
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut sd, std::ptr::null_mut())
    };
    if ok == 0 {
        return;
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let name = wide(PIPE_NAME);
    while !stop.load(Ordering::Relaxed) {
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                1 << 20,
                1 << 12,
                0,
                &sa,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        let pipe = unsafe { std::fs::File::from_raw_handle(h as _) };
        if !connected || stop.load(Ordering::Relaxed) {
            continue; // `pipe` drops and closes the handle
        }
        let _ = std::thread::Builder::new()
            .name("find-service-client".into())
            .spawn(move || serve_client(pipe));
    }
    unsafe { LocalFree(sd as _) };
}

// ------------------------------------------------------------------ client

/// Send one request to the service. `None` when it isn't installed/running.
pub fn request(req: &Request) -> Option<Response> {
    let mut pipe = None;
    for _ in 0..20 {
        match std::fs::OpenOptions::new().read(true).write(true).open(PIPE_NAME) {
            Ok(p) => {
                pipe = Some(p);
                break;
            }
            // ERROR_PIPE_BUSY: all instances in use; the server makes a new
            // one immediately, so retry briefly.
            Err(e) if e.raw_os_error() == Some(231) => {
                std::thread::sleep(std::time::Duration::from_millis(25))
            }
            Err(_) => return None,
        }
    }
    let mut pipe = pipe?;
    let body = bincode::serialize(req).ok()?;
    write_frame(&mut pipe, &body).ok()?;
    let resp = read_frame(&mut pipe, MAX_RESPONSE).ok()?;
    bincode::deserialize(&resp).ok()
}

/// Service state as the Settings panel shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    /// Responds, but speaks a different protocol (FIND was updated).
    Outdated,
    NotRunning,
}

pub fn status() -> Status {
    match request(&Request::Ping) {
        Some(Response::Pong { version }) if version == PROTOCOL_VERSION => Status::Running,
        Some(_) => Status::Outdated,
        None => Status::NotRunning,
    }
}

// ------------------------------------------------------------- volume source

/// How this process reaches raw volume data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Elevated: open the volume ourselves.
    Direct,
    /// Through the FIND service.
    Service,
    None,
}

pub fn access_for(letter: char) -> Access {
    if volume::can_open_directly(letter) {
        Access::Direct
    } else if status() == Status::Running {
        Access::Service
    } else {
        Access::None
    }
}

pub fn enum_volume(
    letter: char,
    progress: &AtomicUsize,
    cancel: &AtomicBool,
) -> VResult<(Vec<FileRecord>, bool)> {
    match access_for(letter) {
        Access::Direct => enum_volume_direct(letter, progress, cancel),
        Access::Service => match request(&Request::EnumVolume { letter }) {
            Some(Response::Records {
                records,
                full_metadata,
            }) => {
                progress.store(records.len(), Ordering::Relaxed);
                Ok((records, full_metadata))
            }
            Some(Response::Error(e)) => Err(e),
            _ => Err(VolumeError::Unavailable("service did not answer".into())),
        },
        Access::None => Err(VolumeError::Unavailable("no volume access".into())),
    }
}

pub fn query_journal(letter: char) -> VResult<JournalInfo> {
    match access_for(letter) {
        Access::Direct => volume::query_journal(letter),
        Access::Service => match request(&Request::QueryJournal { letter }) {
            Some(Response::Journal(j)) => Ok(j),
            Some(Response::Error(e)) => Err(e),
            _ => Err(VolumeError::Unavailable("service did not answer".into())),
        },
        Access::None => Err(VolumeError::Unavailable("no volume access".into())),
    }
}

pub fn read_journal(letter: char, journal_id: u64, start_usn: i64) -> VResult<(Vec<UsnChange>, i64)> {
    match access_for(letter) {
        Access::Direct => volume::read_journal(letter, journal_id, start_usn, 8 << 20),
        Access::Service => match request(&Request::ReadJournal {
            letter,
            journal_id,
            start_usn,
        }) {
            Some(Response::Changes { changes, next_usn }) => Ok((changes, next_usn)),
            Some(Response::Error(e)) => Err(e),
            _ => Err(VolumeError::Unavailable("service did not answer".into())),
        },
        Access::None => Err(VolumeError::Unavailable("no volume access".into())),
    }
}

// --------------------------------------------------------- SCM entry point

static STOP: AtomicBool = AtomicBool::new(false);
static STATUS_HANDLE: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

fn set_state(state: u32) {
    use windows_sys::Win32::System::Services::{
        SetServiceStatus, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_RUNNING,
        SERVICE_STATUS, SERVICE_WIN32_OWN_PROCESS,
    };
    let h = STATUS_HANDLE.load(Ordering::SeqCst);
    if h == 0 {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    unsafe { SetServiceStatus(h as _, &status) };
}

unsafe extern "system" fn control_handler(control: u32) {
    use windows_sys::Win32::System::Services::{
        SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_STOP_PENDING,
    };
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        STOP.store(true, Ordering::SeqCst);
        set_state(SERVICE_STOP_PENDING);
        // Unblock ConnectNamedPipe with a throwaway connection.
        let _ = std::fs::OpenOptions::new().read(true).write(true).open(PIPE_NAME);
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    use windows_sys::Win32::System::Services::{
        RegisterServiceCtrlHandlerW, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STOPPED,
    };
    let name = wide(SERVICE_NAME);
    let h = RegisterServiceCtrlHandlerW(name.as_ptr(), Some(control_handler));
    if h.is_null() {
        return;
    }
    STATUS_HANDLE.store(h as isize, Ordering::SeqCst);
    set_state(SERVICE_START_PENDING);
    set_state(SERVICE_RUNNING);
    pipe_server(&STOP);
    set_state(SERVICE_STOPPED);
}

/// `find.exe --service`: hand control to the Service Control Manager.
pub fn run_as_service() {
    use windows_sys::Win32::System::Services::{StartServiceCtrlDispatcherW, SERVICE_TABLE_ENTRYW};
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
}

// ------------------------------------------------------ install / uninstall

fn install_dir() -> PathBuf {
    let pf = std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into());
    PathBuf::from(pf).join("FIND")
}

pub fn installed_exe() -> PathBuf {
    install_dir().join("find-service.exe")
}

struct Sc(windows_sys::Win32::System::Services::SC_HANDLE);
impl Drop for Sc {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::System::Services::CloseServiceHandle(self.0) };
    }
}

fn last_error() -> String {
    std::io::Error::last_os_error().to_string()
}

fn stop_service(svc: &Sc) {
    use windows_sys::Win32::System::Services::{
        ControlService, QueryServiceStatus, SERVICE_CONTROL_STOP, SERVICE_STATUS, SERVICE_STOPPED,
    };
    let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st) };
    for _ in 0..50 {
        if unsafe { QueryServiceStatus(svc.0, &mut st) } == 0 || st.dwCurrentState == SERVICE_STOPPED {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// `find.exe --install-service` (run elevated). Copies the binary into
/// Program Files, registers an auto-start LocalSystem service, starts it.
/// Re-running updates an existing install in place.
pub fn install() -> Result<(), String> {
    use windows_sys::Win32::System::Services::{
        CreateServiceW, OpenSCManagerW, OpenServiceW, StartServiceW, SC_MANAGER_ALL_ACCESS,
        SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_ERROR_NORMAL, SERVICE_WIN32_OWN_PROCESS,
    };
    let scm = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS) };
    if scm.is_null() {
        return Err(format!("open service manager: {}", last_error()));
    }
    let scm = Sc(scm);
    let name = wide(SERVICE_NAME);

    // Stop an existing install first so its binary can be replaced.
    let existing = unsafe { OpenServiceW(scm.0, name.as_ptr(), SERVICE_ALL_ACCESS) };
    let existing = (!existing.is_null()).then(|| Sc(existing));
    if let Some(svc) = &existing {
        stop_service(svc);
    }

    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = installed_exe();
    std::fs::create_dir_all(install_dir()).map_err(|e| format!("create {}: {e}", install_dir().display()))?;
    if me != target {
        std::fs::copy(&me, &target).map_err(|e| format!("copy to {}: {e}", target.display()))?;
    }

    let svc = match existing {
        Some(s) => s,
        None => {
            let cmd = wide(&format!("\"{}\" --service", target.display()));
            let display = wide(DISPLAY_NAME);
            let h = unsafe {
                CreateServiceW(
                    scm.0,
                    name.as_ptr(),
                    display.as_ptr(),
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    cmd.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(), // LocalSystem
                    std::ptr::null(),
                )
            };
            if h.is_null() {
                return Err(format!("create service: {}", last_error()));
            }
            Sc(h)
        }
    };
    if unsafe { StartServiceW(svc.0, 0, std::ptr::null()) } == 0 {
        // 1056 = already running
        if std::io::Error::last_os_error().raw_os_error() != Some(1056) {
            return Err(format!("start service: {}", last_error()));
        }
    }
    Ok(())
}

/// `find.exe --uninstall-service` (run elevated).
pub fn uninstall() -> Result<(), String> {
    use windows_sys::Win32::System::Services::{
        DeleteService, OpenSCManagerW, OpenServiceW, SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS,
    };
    let scm = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS) };
    if scm.is_null() {
        return Err(format!("open service manager: {}", last_error()));
    }
    let scm = Sc(scm);
    let name = wide(SERVICE_NAME);
    let svc = unsafe { OpenServiceW(scm.0, name.as_ptr(), SERVICE_ALL_ACCESS) };
    if svc.is_null() {
        return Ok(()); // not installed
    }
    let svc = Sc(svc);
    stop_service(&svc);
    if unsafe { DeleteService(svc.0) } == 0 {
        return Err(format!("delete service: {}", last_error()));
    }
    drop(svc);
    let _ = std::fs::remove_file(installed_exe());
    let _ = std::fs::remove_dir(install_dir());
    Ok(())
}

/// True when this process has administrator rights.
pub fn is_elevated() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// Run this exe elevated with `arg` (one UAC prompt) and wait for it.
/// Used by the Settings buttons.
pub fn run_elevated(arg: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_w = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let params = wide(arg);
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = exe_w.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 0; // SW_HIDE
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        // 1223 = the user clicked No on the UAC prompt
        return Err(match std::io::Error::last_os_error().raw_os_error() {
            Some(1223) => "cancelled at the administrator prompt".into(),
            _ => last_error(),
        });
    }
    if info.hProcess.is_null() {
        return Err("no process handle".into());
    }
    let mut code: u32 = 1;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    if code == 0 {
        Ok(())
    } else {
        Err(format!("installer exited with code {code}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        let req = Request::ReadJournal {
            letter: 'C',
            journal_id: 42,
            start_usn: 7,
        };
        write_frame(&mut buf, &bincode::serialize(&req).unwrap()).unwrap();
        let body = read_frame(&mut buf.as_slice(), MAX_REQUEST as u64).unwrap();
        match bincode::deserialize::<Request>(&body).unwrap() {
            Request::ReadJournal { letter, journal_id, start_usn } => {
                assert_eq!((letter, journal_id, start_usn), ('C', 42, 7));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_REQUEST as u64 + 1).to_le_bytes());
        assert!(read_frame(&mut buf.as_slice(), MAX_REQUEST as u64).is_err());
    }

    #[test]
    fn service_rejects_non_fixed_drives() {
        // '1' isn't a drive letter at all; the handler must refuse it rather
        // than build a volume path from it.
        match handle(Request::EnumVolume { letter: '1' }) {
            Response::Error(VolumeError::Unavailable(_)) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}
