//! Mounting through macFUSE's FSKit backend with `MFMount.framework`.
//! Selected by the `backend=fskit` mount option; without it, macOS mounts use
//! the kernel extension through libfuse (`fuse3.rs`).

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use crate::channel_fskit::FskitChannel;
use crate::mnt::MountOption;
use crate::mnt::mount_options::option_to_string;

/// Whether `option` selects the FSKit backend (`-o backend=fskit`).
fn is_backend_fskit(option: &MountOption) -> bool {
    matches!(option, MountOption::CUSTOM(s) if s == "backend=fskit")
}

/// Whether `options` ask for the FSKit backend.
pub(crate) fn requested(options: &[MountOption]) -> bool {
    options.iter().any(is_backend_fskit)
}

#[derive(Debug)]
pub(crate) struct MountImpl {
    channel: Arc<FskitChannel>,
    mountpoint: CString,
}

impl MountImpl {
    pub(crate) fn new(
        mountpoint: &Path,
        options: &[MountOption],
    ) -> io::Result<(Arc<FskitChannel>, MountImpl)> {
        let mountpoint = CString::new(mountpoint.as_os_str().as_bytes())?;
        // MFMount mounts only through FSKit; the selector itself is not an
        // option it takes.
        let options: Vec<String> = options
            .iter()
            .filter(|o| !is_backend_fskit(o))
            .map(option_to_string)
            .collect();
        let options = CString::new(options.join(","))?;
        let channel = FskitChannel::mount(mountpoint.clone(), options)?;
        Ok((
            channel.clone(),
            MountImpl {
                channel,
                mountpoint,
            },
        ))
    }

    pub(crate) fn umount_impl(&mut self) -> io::Result<()> {
        let result = crate::mnt::libc_umount(&self.mountpoint).map_err(io::Error::from);
        let gone = match &result {
            Ok(()) => true,
            Err(err) => crate::mnt::nothing_mounted(err),
        };
        if gone {
            // Ends the session loop's receive, as the device fd does on
            // unmount.
            self.channel.close();
        }
        result
    }
}
