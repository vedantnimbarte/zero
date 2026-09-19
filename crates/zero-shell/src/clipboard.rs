//! The system clipboard, written to as plain text.
//!
//! One call, one platform backend, no dependency: the OS already has this and
//! every crate that wraps it wraps exactly these three implementations. Reading
//! the clipboard is not here because nothing asks for it yet — paste into the
//! address bar is the next thing that will, and it belongs beside `set`.
//!
//! Failures are silent by design. A clipboard that will not open (another
//! process is holding it, or there is no display server at all) is a copy that
//! did not happen, not a browser that should say anything about it.

/// Put `text` on the system clipboard as plain text.
pub fn set(text: &str) {
    if text.is_empty() {
        return; // nothing selected is not a request to clear what is there
    }
    backend::set(text);
}

#[cfg(windows)]
mod backend {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };

    /// `CF_UNICODETEXT`. Naming it here costs one line and saves pulling in the
    /// whole `Win32_System_Ole` surface for a single `13`.
    const CF_UNICODETEXT: u32 = 13;

    pub fn set(text: &str) {
        // Win32 wants UTF-16, NUL-terminated, in a moveable global block that
        // the clipboard then *owns* — which is why nothing below frees it on
        // the success path.
        let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = std::mem::size_of_val(&utf16[..]);

        // SAFETY: the clipboard is opened for this thread and closed on every
        // path out. The block is sized from `utf16` and written exactly that
        // many units; on success ownership passes to the clipboard, and on
        // failure the block is leaked rather than freed while possibly owned.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return;
            }
            EmptyClipboard();
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if !handle.is_null() {
                let dst = GlobalLock(handle);
                if !dst.is_null() {
                    std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst as *mut u16, utf16.len());
                    GlobalUnlock(handle);
                    SetClipboardData(CF_UNICODETEXT, handle as HANDLE);
                }
            }
            CloseClipboard();
        }
    }
}

#[cfg(not(windows))]
mod backend {
    //! macOS and Linux both hand the clipboard to a helper process, and on X11
    //! that is not a shortcut: the selection belongs to a *live* process, so
    //! something has to stay running to keep serving it. `xclip` and `wl-copy`
    //! already fork and do exactly that.
    use std::io::Write;
    use std::process::{Command, Stdio};

    pub fn set(text: &str) {
        // Wayland first, then X11: a Wayland session usually has `xclip`
        // available through XWayland, and it would put the text on the wrong
        // clipboard.
        let helpers: &[(&str, &[&str])] = match cfg!(target_os = "macos") {
            true => &[("pbcopy", &[])],
            false => &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])],
        };
        for (program, args) in helpers {
            if pipe_into(program, args, text) {
                return;
            }
        }
    }

    /// Run `program` and write `text` to its stdin. `false` if it is not
    /// installed or refused the text, so the caller can try the next one.
    fn pipe_into(program: &str, args: &[&str], text: &str) -> bool {
        let Ok(mut child) = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let Some(mut stdin) = child.stdin.take() else {
            return false;
        };
        let written = stdin.write_all(text.as_bytes()).is_ok();
        drop(stdin); // the helper waits on EOF before it owns the selection
                     // `xclip` and `wl-copy` fork and keep serving the selection, so this
                     // returns as soon as the parent half exits — it is not a wait on the
                     // process that holds the clipboard.
        written && child.wait().map(|s| s.success()).unwrap_or(false)
    }
}
