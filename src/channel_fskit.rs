//! Kernel channel for macFUSE's FSKit backend, through `MFMount.framework`
//! (macFUSE 5.3+).
//!
//! An FSKit volume has no FUSE device fd. `MFMount.framework` mounts it and
//! exchanges the same serialized FUSE messages over a channel instead:
//! requests arrive as one or two borrowed buffers (the structured part, then
//! an optional payload such as WRITE data) and replies are sent as iovecs.
//! fuser's parser and reply encoding are used unchanged.
//!
//! macFUSE's zero-copy extensions (`FUSE_DARWIN_REPLY_BUF`,
//! `FUSE_DARWIN_PAYLOAD_BUF`) are INIT flags fuser does not request, so every
//! reply carries its payload inline.

use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::c_char;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::io::IoSlice;
use std::mem::transmute;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;

use log::error;
use nix::errno::Errno;
use parking_lot::Mutex;

type MFTypeRef = *mut c_void;

/// `MFMountResult`.
const MF_MOUNT_SUCCESS: i32 = 0;
const MF_MOUNT_UNSUPPORTED_OS: i32 = 1;
const MF_MOUNT_HELPER_TOOLS_FAILED: i32 = 2;
const MF_MOUNT_EXTENSION_NOT_FOUND: i32 = 3;
const MF_MOUNT_EXTENSION_NEEDS_APPROVAL: i32 = 4;
const MF_MOUNT_CREATING_MOUNT_POINT_FAILED: i32 = 5;

/// Where macFUSE installs `MFMount.framework`: the macFUSE bundle's location
/// is fixed by its installer.
const MFMOUNT_PATH: &CStr =
    c"/Library/Filesystems/macfuse.fs/Contents/Frameworks/MFMount.framework/MFMount";

/// `MFMount.framework`'s functions, loaded on the first FSKit mount rather
/// than linked, so a fuser binary runs (with the kext) where macFUSE < 5.3 or
/// no macFUSE is installed.
struct Api {
    release: unsafe extern "C" fn(MFTypeRef),
    channel_create: unsafe extern "C" fn() -> MFTypeRef,
    mount: unsafe extern "C" fn(MFTypeRef, *const c_char, *const c_char, bool) -> i32,
    copy_next_message: unsafe extern "C" fn(MFTypeRef) -> MFTypeRef,
    get_body_buffers: unsafe extern "C" fn(MFTypeRef, *mut *const libc::iovec) -> isize,
    send_message: unsafe extern "C" fn(MFTypeRef, *const libc::iovec, usize) -> isize,
    channel_close: unsafe extern "C" fn(MFTypeRef) -> bool,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MFMount.framework")
    }
}

impl Api {
    /// The loaded framework, or why it could not be loaded.
    fn get() -> io::Result<&'static Api> {
        static API: OnceLock<Result<Api, String>> = OnceLock::new();
        API.get_or_init(|| Api::load(MFMOUNT_PATH))
            .as_ref()
            .map_err(|e| io::Error::new(io::ErrorKind::Unsupported, e.clone()))
    }

    // Each transmute's target is inferred from the `Api` field it
    // initializes, whose declared type is the annotation.
    #[allow(clippy::missing_transmute_annotations)]
    fn load(path: &CStr) -> Result<Api, String> {
        // SAFETY: loading a system framework; the handle is never closed, so
        // the symbols stay valid for the process's lifetime.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!(
                "FSKit mounts need macFUSE 5.3 or later ({}: {})",
                path.to_string_lossy(),
                dlerror()
            ));
        }
        // SAFETY: each field's type is the C signature MFMount.h declares for
        // that symbol; `transmute` checks at compile time that the pointer
        // and that concrete type have the same size.
        unsafe {
            Ok(Api {
                release: transmute::<*mut c_void, _>(lookup(handle, c"MFRelease")?),
                channel_create: transmute::<*mut c_void, _>(lookup(handle, c"MFChannelCreate")?),
                mount: transmute::<*mut c_void, _>(lookup(handle, c"MFMount")?),
                copy_next_message: transmute::<*mut c_void, _>(lookup(
                    handle,
                    c"MFChannelCopyNextMessage",
                )?),
                get_body_buffers: transmute::<*mut c_void, _>(lookup(
                    handle,
                    c"MFMessageGetBodyBuffers",
                )?),
                send_message: transmute::<*mut c_void, _>(lookup(handle, c"MFChannelSendMessage")?),
                channel_close: transmute::<*mut c_void, _>(lookup(handle, c"MFChannelClose")?),
            })
        }
    }
}

/// The address of `name` in `handle`.
fn lookup(handle: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: valid handle and symbol name.
    let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if p.is_null() {
        return Err(format!(
            "MFMount.framework lacks {} (macFUSE too old?)",
            name.to_string_lossy()
        ));
    }
    Ok(p)
}

/// The last `dlopen`/`dlsym` error.
fn dlerror() -> String {
    // SAFETY: dlerror returns null or a valid C string.
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        "unknown error".to_string()
    } else {
        // SAFETY: non-null, NUL-terminated.
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}

/// An `MFMount` channel associated with a mounted FSKit volume.
#[derive(Debug)]
pub(crate) struct FskitChannel {
    api: &'static Api,
    channel: NonNull<c_void>,
    closed: AtomicBool,
    /// Set by the mount thread if `MFMount` fails.
    mount_error: Mutex<Option<io::Error>>,
    /// `/dev/null`, so `Session`'s `AsFd` has something to return. An FSKit
    /// channel has no fd to poll.
    placeholder: File,
}

// SAFETY: fuser receives on one thread, sends replies from any, and mounts on
// another. MFMount.h does not document channel thread safety; this relies on
// its implementation (macfuse/mount 68f082d, Mount/Channel/XPC/XPCTransport.swift):
// received messages are queued under an NSCondition that `send` never takes,
// `send` copies the buffers into an XPC message on the thread-safe XPC
// connection, and closing wakes a blocked receiver with ENODEV. Re-check when
// macFUSE updates MFMount.
unsafe impl Send for FskitChannel {}
unsafe impl Sync for FskitChannel {}

impl FskitChannel {
    /// Starts mounting an FSKit volume at `mountpoint` with comma-separated
    /// FUSE mount `options`, on a thread of its own: `MFMount` returns only
    /// once the volume is mounted, which takes INIT (and the first requests)
    /// to be answered on this channel. Receiving may start right away; it
    /// blocks until the mount associates the channel. If the mount fails,
    /// the channel is closed (ending `receive` with `ENODEV`) and the reason
    /// is kept for [`Self::mount_error`].
    pub(crate) fn mount(mountpoint: CString, options: CString) -> io::Result<Arc<Self>> {
        let api = Api::get()?;
        // SAFETY: plain constructor.
        let channel = NonNull::new(unsafe { (api.channel_create)() })
            .ok_or_else(|| io::Error::other("MFChannelCreate failed"))?;
        let this = Arc::new(Self {
            api,
            channel,
            closed: AtomicBool::new(false),
            mount_error: Mutex::new(None),
            placeholder: File::open("/dev/null")?,
        });
        let mounting = this.clone();
        thread::Builder::new()
            .name("fuser-fskit-mount".to_string())
            .spawn(move || {
                if let Err(err) = mounting.run_mount(&mountpoint, &options) {
                    error!("{err}");
                    *mounting.mount_error.lock() = Some(err);
                    mounting.close();
                }
            })?;
        Ok(this)
    }

    fn run_mount(&self, mountpoint: &CStr, options: &CStr) -> io::Result<()> {
        // SAFETY: valid channel and C strings. quiet: report failures only
        // through the result, never with a dialog.
        let res = unsafe {
            (self.api.mount)(
                self.channel.as_ptr(),
                mountpoint.as_ptr(),
                options.as_ptr(),
                true,
            )
        };
        let reason = match res {
            MF_MOUNT_SUCCESS => return Ok(()),
            MF_MOUNT_UNSUPPORTED_OS => "macOS version not supported by macFUSE's FSKit backend",
            MF_MOUNT_HELPER_TOOLS_FAILED => "macFUSE helper tools could not be installed",
            // Also what macFUSE reports for a registered but disabled
            // extension, e.g. after a macFUSE upgrade.
            MF_MOUNT_EXTENSION_NOT_FOUND => {
                "macFUSE's FSKit extension is not registered or not enabled: open \
                 /Library/Filesystems/macfuse.fs/Contents/Resources/macfuse.app once, \
                 then turn on macFUSE in System Settings > General > \
                 Login Items & Extensions > File System Extensions"
            }
            MF_MOUNT_EXTENSION_NEEDS_APPROVAL => {
                "macFUSE's FSKit extension needs approval (System Settings > General > \
                 Login Items & Extensions > File System Extensions)"
            }
            MF_MOUNT_CREATING_MOUNT_POINT_FAILED => "mount point could not be created",
            _ => return Err(io::Error::last_os_error()),
        };
        Err(io::Error::other(format!("FSKit mount failed: {reason}")))
    }

    /// Why the mount failed, if it did.
    pub(crate) fn mount_error(&self) -> Option<io::Error> {
        self.mount_error.lock().take()
    }

    /// Receives one request into `buffer`. `ENODEV` once the channel is
    /// closed, as the device fd reports an ended session.
    pub(crate) fn receive(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        // SAFETY: valid channel; blocks until a message arrives or the
        // channel is closed or interrupted.
        let message = unsafe { (self.api.copy_next_message)(self.channel.as_ptr()) };
        if message.is_null() {
            return Err(Errno::last());
        }
        let _release = Released(self.api, message);
        let mut buffers: *const libc::iovec = std::ptr::null();
        // SAFETY: valid message; buffers is written on success.
        let count = unsafe { (self.api.get_body_buffers)(message, &mut buffers) };
        if count < 0 {
            return Err(Errno::last());
        }
        // SAFETY: count entries, borrowed from message (alive until _release).
        let buffers = unsafe { std::slice::from_raw_parts(buffers, count as usize) };
        let mut len = 0;
        for b in buffers {
            let dst = buffer
                .get_mut(len..len + b.iov_len)
                .ok_or(Errno::EOVERFLOW)?;
            // SAFETY: the entry describes iov_len readable bytes.
            dst.copy_from_slice(unsafe {
                std::slice::from_raw_parts(b.iov_base.cast(), b.iov_len)
            });
            len += b.iov_len;
        }
        Ok(len)
    }

    /// Sends one serialized reply or notification.
    pub(crate) fn send(&self, bufs: &[IoSlice<'_>]) -> io::Result<()> {
        // IoSlice is ABI-compatible with iovec on Unix.
        // SAFETY: valid channel; bufs are live for the call.
        let res = unsafe {
            (self.api.send_message)(self.channel.as_ptr(), bufs.as_ptr().cast(), bufs.len())
        };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Closes the channel, ending a blocked or later `receive` with `ENODEV`.
    /// Idempotent.
    pub(crate) fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            // SAFETY: valid, not yet closed.
            unsafe { (self.api.channel_close)(self.channel.as_ptr()) };
        }
    }
}

impl AsFd for FskitChannel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.placeholder.as_fd()
    }
}

impl Drop for FskitChannel {
    fn drop(&mut self) {
        self.close();
        // SAFETY: last reference to the channel.
        unsafe { (self.api.release)(self.channel.as_ptr()) };
    }
}

/// Releases a received message when dropped.
struct Released(&'static Api, MFTypeRef);

impl Drop for Released {
    fn drop(&mut self) {
        // SAFETY: a message this code copied and owns.
        unsafe { (self.0.release)(self.1) };
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn missing_framework_is_a_clear_error() {
        let err = Api::load(c"/nonexistent/MFMount.framework/MFMount").unwrap_err();
        assert!(
            err.starts_with("FSKit mounts need macFUSE 5.3 or later"),
            "{err}"
        );
    }

    #[test]
    fn installed_framework_loads() {
        if !Path::new(MFMOUNT_PATH.to_str().unwrap()).exists() {
            return;
        }
        Api::load(MFMOUNT_PATH).unwrap();
    }
}
