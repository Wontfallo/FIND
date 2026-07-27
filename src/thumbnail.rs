//! Windows shell thumbnails: asks Explorer's own thumbnail provider for a
//! bitmap of any file it can render — video frames, PSDs, 3D models, CAD,
//! Office docs, whatever the installed codecs and shell extensions support.
//!
//! This is how Explorer itself shows video frames, so coverage matches what
//! the user already sees in their file manager, without bundling a decoder.
#![cfg(target_os = "windows")]

use std::path::Path;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::SIZE;
use windows_sys::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, GetDIBits, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, GetDC, ReleaseDC, HBITMAP};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::UI::Shell::{SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK};

/// windows-sys exposes functions but not COM interfaces, so declare the bit
/// of IShellItemImageFactory we use. Layout: the three IUnknown slots
/// followed by GetImage.
#[repr(C)]
struct ImageFactoryVtbl {
    query_interface: unsafe extern "system" fn(
        *mut ImageFactory,
        *const GUID,
        *mut *mut std::ffi::c_void,
    ) -> i32,
    add_ref: unsafe extern "system" fn(*mut ImageFactory) -> u32,
    release: unsafe extern "system" fn(*mut ImageFactory) -> u32,
    get_image: unsafe extern "system" fn(*mut ImageFactory, SIZE, i32, *mut HBITMAP) -> i32,
}

#[repr(C)]
struct ImageFactory {
    vtbl: *const ImageFactoryVtbl,
}

/// A decoded thumbnail: RGBA8 pixels ready for the preview pane.
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

thread_local! {
    static COM_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn ensure_com() {
    COM_READY.with(|ready| {
        if !ready.get() {
            unsafe {
                CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
            }
            ready.set(true);
        }
    });
}

/// Ask the shell for a thumbnail of `path`, at most `max_edge` pixels on its
/// longest side. Returns None when no provider can render the file.
pub fn shell_thumbnail(path: &Path, max_edge: u32) -> Option<Thumbnail> {
    ensure_com();

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // IShellItemImageFactory {BCC18B79-BA16-442F-80C4-8A59C30C463B}
    const IID_IMAGE_FACTORY: GUID = GUID {
        data1: 0xBCC18B79,
        data2: 0xBA16,
        data3: 0x442F,
        data4: [0x80, 0xC4, 0x8A, 0x59, 0xC3, 0x0C, 0x46, 0x3B],
    };

    let mut factory: *mut std::ffi::c_void = std::ptr::null_mut();
    let hr = unsafe {
        SHCreateItemFromParsingName(
            wide.as_ptr(),
            std::ptr::null_mut(),
            &IID_IMAGE_FACTORY,
            &mut factory,
        )
    };
    if hr < 0 || factory.is_null() {
        return None;
    }
    let factory = factory as *mut ImageFactory;
    let result = (|| {
        let mut hbitmap: HBITMAP = std::ptr::null_mut();
        let size = SIZE {
            cx: max_edge as i32,
            cy: max_edge as i32,
        };
        let hr = unsafe {
            ((*(*factory).vtbl).get_image)(factory, size, SIIGBF_BIGGERSIZEOK, &mut hbitmap)
        };
        if hr < 0 || hbitmap.is_null() {
            return None;
        }
        let thumb = bitmap_to_rgba(hbitmap);
        unsafe { DeleteObject(hbitmap as _) };
        thumb
    })();
    unsafe { ((*(*factory).vtbl).release)(factory) };
    result
}

/// Copy an HBITMAP's pixels into a straight RGBA8 buffer.
fn bitmap_to_rgba(hbitmap: HBITMAP) -> Option<Thumbnail> {
    let mut bmp: BITMAP = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetObjectW(
            hbitmap as _,
            std::mem::size_of::<BITMAP>() as i32,
            &mut bmp as *mut _ as *mut std::ffi::c_void,
        )
    };
    if ok == 0 || bmp.bmWidth <= 0 || bmp.bmHeight == 0 {
        return None;
    }
    let width = bmp.bmWidth as u32;
    let height = bmp.bmHeight.unsigned_abs();

    let mut header: BITMAPINFO = unsafe { std::mem::zeroed() };
    header.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: bmp.bmWidth,
        // Negative height requests a top-down buffer.
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };

    let mut buffer = vec![0u8; (width * height * 4) as usize];
    let hdc = unsafe { GetDC(std::ptr::null_mut()) };
    let copied = unsafe {
        GetDIBits(
            hdc,
            hbitmap,
            0,
            height,
            buffer.as_mut_ptr() as *mut std::ffi::c_void,
            &mut header,
            DIB_RGB_COLORS,
        )
    };
    unsafe { ReleaseDC(std::ptr::null_mut(), hdc) };
    if copied == 0 {
        return None;
    }

    // Shell bitmaps are BGRA; some providers leave alpha at zero, which would
    // render fully transparent — treat an all-zero alpha channel as opaque.
    let opaque = buffer.chunks_exact(4).all(|px| px[3] == 0);
    for px in buffer.chunks_exact_mut(4) {
        px.swap(0, 2);
        if opaque {
            px[3] = 255;
        }
    }
    Some(Thumbnail {
        width,
        height,
        rgba: buffer,
    })
}

use std::os::windows::ffi::OsStrExt;
