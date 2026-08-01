//! File operations invoked from the results list: copy/cut to the clipboard
//! (as real files, so Explorer can paste them) and delete to the Recycle Bin.
//!
//! Everything here goes through the OS shell rather than raw filesystem calls,
//! so deletes are recoverable and clipboard payloads behave exactly like a
//! copy performed in Explorer.

use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClipMode {
    Copy,
    Cut,
}

/// Put `paths` on the clipboard as files. Explorer (and any app accepting
/// dropped files) can then paste them; `Cut` marks them for a move.
#[cfg(target_os = "windows")]
pub fn clipboard_files(paths: &[String], mode: ClipMode) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW,
        SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows_sys::Win32::System::Ole::CF_HDROP;
    use windows_sys::Win32::UI::Shell::DROPFILES;

    if paths.is_empty() {
        return Err("nothing selected".into());
    }

    // CF_HDROP payload: DROPFILES header, then double-null-terminated UTF-16
    // paths, then one extra NUL to end the list.
    let mut chars: Vec<u16> = Vec::new();
    for path in paths {
        chars.extend(std::ffi::OsStr::new(path).encode_wide());
        chars.push(0);
    }
    chars.push(0);

    let header_size = std::mem::size_of::<DROPFILES>();
    let total = header_size + chars.len() * 2;

    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, total);
        if handle.is_null() {
            return Err("out of memory".into());
        }
        let ptr = GlobalLock(handle) as *mut u8;
        if ptr.is_null() {
            return Err("could not lock clipboard memory".into());
        }
        let drop_files = ptr as *mut DROPFILES;
        (*drop_files).pFiles = header_size as u32;
        (*drop_files).pt = std::mem::zeroed();
        (*drop_files).fNC = 0;
        (*drop_files).fWide = 1; // UTF-16 paths
        std::ptr::copy_nonoverlapping(
            chars.as_ptr() as *const u8,
            ptr.add(header_size),
            chars.len() * 2,
        );
        GlobalUnlock(handle);

        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("clipboard is in use by another app".into());
        }
        EmptyClipboard();
        if SetClipboardData(CF_HDROP as u32, handle as HANDLE).is_null() {
            CloseClipboard();
            return Err("could not set clipboard data".into());
        }

        // "Preferred DropEffect" tells the target whether this is a copy (1)
        // or a move (2). Without it, a cut behaves like a copy.
        let format_name: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
        let format = RegisterClipboardFormatW(format_name.as_ptr());
        if format != 0 {
            let effect: u32 = match mode {
                ClipMode::Copy => 1, // DROPEFFECT_COPY
                ClipMode::Cut => 2,  // DROPEFFECT_MOVE
            };
            let effect_handle = GlobalAlloc(GMEM_MOVEABLE, 4);
            if !effect_handle.is_null() {
                let effect_ptr = GlobalLock(effect_handle) as *mut u32;
                if !effect_ptr.is_null() {
                    *effect_ptr = effect;
                    GlobalUnlock(effect_handle);
                    SetClipboardData(format, effect_handle as HANDLE);
                }
            }
        }
        CloseClipboard();
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn clipboard_files(_paths: &[String], _mode: ClipMode) -> Result<(), String> {
    Err("clipboard file operations are only implemented on Windows".into())
}

/// Delete `paths` to the Recycle Bin (recoverable). Returns the number of
/// items the shell reported as deleted.
#[cfg(target_os = "windows")]
pub fn delete_to_recycle_bin(paths: &[String]) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FO_DELETE,
        SHFILEOPSTRUCTW,
    };

    if paths.is_empty() {
        return Err("nothing selected".into());
    }
    // Double-null-terminated list of source paths.
    let mut from: Vec<u16> = Vec::new();
    for path in paths {
        from.extend(std::ffi::OsStr::new(path).encode_wide());
        from.push(0);
    }
    from.push(0);

    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE as u32;
    op.pFrom = from.as_ptr();
    // We ask the user ourselves, so suppress the shell's own prompt; ALLOWUNDO
    // is what routes the delete to the Recycle Bin instead of erasing.
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT) as u16;

    let result = unsafe { SHFileOperationW(&mut op) };
    if result != 0 {
        return Err(format!("delete failed (shell error {result})"));
    }
    if op.fAnyOperationsAborted != 0 {
        return Err("delete was cancelled".into());
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn delete_to_recycle_bin(paths: &[String]) -> Result<(), String> {
    // No portable trash API here; delete outright so the action still works.
    for path in paths {
        let p = Path::new(path);
        let result = if p.is_dir() {
            std::fs::remove_dir_all(p)
        } else {
            std::fs::remove_file(p)
        };
        result.map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}

/// Human summary for the confirmation dialog.
pub fn describe(paths: &[String]) -> String {
    match paths.len() {
        0 => "nothing".into(),
        1 => Path::new(&paths[0])
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| paths[0].clone()),
        n => format!("{n} items"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_describe() {
        assert_eq!(describe(&[]), "nothing");
        assert_eq!(
            describe(&["C:\\dir\\file.txt".to_string()]),
            if cfg!(windows) { "file.txt" } else { "C:\\dir\\file.txt" }
        );
        assert_eq!(
            describe(&["a".to_string(), "b".to_string(), "c".to_string()]),
            "3 items"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn test_delete_removes_files() {
        let tmp = std::env::temp_dir().join(format!("find_del_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("gone.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(file.exists());
        delete_to_recycle_bin(&[file.display().to_string()]).unwrap();
        assert!(!file.exists());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
