//! The console of a release build on Windows (issue #176).
//!
//! A release build of the GUI is linked as a Windows-subsystem program
//! (`windows_subsystem = "windows"` in `main.rs`), so started from
//! Explorer, the Start menu or "Open with" it gets no console window next
//! to its own. The price: started from a terminal, it is not attached to
//! that terminal either, and `--help`, `--version`, clap's error messages
//! and anything written to stderr would vanish.
//!
//! [`attach_parent_console`] fixes that by attaching to the console of the
//! parent process, if it has one. Rust's standard streams ask Windows for
//! the standard handle on every write, so they use the console from then
//! on. As with every GUI program, `cmd.exe` does not wait for it, so the
//! output may come after the next prompt.
//!
//! The Win32 calls are FFI and so `unsafe`; besides the video renderers
//! this module is the one place in `lac-gui` allowed to use it (AGENTS.md).

#![allow(unsafe_code)]

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType};
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE,
};

/// Attaches the program to its parent's console, unless stderr already
/// goes somewhere usable.
///
/// Output redirected into a file or a pipe (`losslessaudiocut --version >
/// v.txt`, or a script capturing it) arrives as inherited handles that
/// work as they are; attaching then could only redirect it away from
/// where the caller wants it. Without a parent console (started from
/// Explorer), `AttachConsole` fails, and nothing changes. A debug build is
/// a console program and already has its console; there, too, the call
/// fails harmlessly.
pub fn attach_parent_console() {
    // SAFETY: GetStdHandle takes no pointers and returns a handle or
    // null/INVALID_HANDLE_VALUE, which we check before using it.
    let stderr = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if !stderr.is_null() && stderr != INVALID_HANDLE_VALUE {
        // SAFETY: `stderr` is a handle the process owns (checked above);
        // GetFileType only queries it.
        let kind = unsafe { GetFileType(stderr) };
        if kind == FILE_TYPE_DISK || kind == FILE_TYPE_PIPE {
            return;
        }
    }
    // SAFETY: AttachConsole takes a process id (here the documented
    // "parent" constant) and no pointers. Its failure (no parent console,
    // or already attached) is not an error for us, so the result is
    // ignored.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
