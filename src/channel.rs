use std::io;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

use nix::errno::Errno;

use crate::SessionACL;
#[cfg(fuser_fskit)]
use crate::channel_fskit::FskitChannel;
use crate::dev_fuse::DevFuse;
use crate::passthrough::BackingId;

/// A raw communication channel to the FUSE kernel driver
#[derive(Debug, Clone)]
pub(crate) enum Channel {
    /// The FUSE device fd.
    Dev(Arc<DevFuse>),
    /// An `MFMount` channel: macFUSE's FSKit backend has no device fd.
    #[cfg(fuser_fskit)]
    Fskit(Arc<FskitChannel>),
}

impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Channel::Dev(dev) => dev.as_fd(),
            #[cfg(fuser_fskit)]
            Channel::Fskit(channel) => channel.as_fd(),
        }
    }
}

impl Channel {
    /// Create a new communication channel to the kernel driver by mounting the
    /// given path. The kernel driver will delegate filesystem operations of
    /// the given path to the channel.
    pub(crate) fn new(device: Arc<DevFuse>) -> Self {
        Self::Dev(device)
    }

    /// Receives data up to the capacity of the given buffer (can block).
    fn receive(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        match self {
            Channel::Dev(dev) => nix::unistd::read(dev, buffer),
            #[cfg(fuser_fskit)]
            Channel::Fskit(channel) => channel.receive(buffer),
        }
    }

    /// The caller check to apply given the configured `acl`. macFUSE's FSKit
    /// backend sends no caller credentials (uid, gid and pid are 0 on every
    /// request), so checking callers against the session owner would refuse
    /// nearly everything.
    pub(crate) fn caller_acl(&self, acl: SessionACL) -> SessionACL {
        match self {
            Channel::Dev(_) => acl,
            #[cfg(fuser_fskit)]
            Channel::Fskit(_) => {
                if acl != SessionACL::All {
                    log::info!(
                        "FSKit channel has no caller credentials; not checking callers against the owner"
                    );
                }
                SessionACL::All
            }
        }
    }

    /// Why an FSKit mount failed, if it did.
    #[cfg(fuser_fskit)]
    pub(crate) fn fskit_mount_error(&self) -> Option<io::Error> {
        match self {
            Channel::Fskit(channel) => channel.mount_error(),
            Channel::Dev(_) => None,
        }
    }

    /// Receives data up to the capacity of the given buffer (can block),
    /// retrying on errors that are safe to retry (ENOENT, EINTR, EAGAIN).
    ///
    /// - ENOENT: Operation interrupted. According to FUSE, this is safe to retry.
    /// - EINTR: Interrupted system call, retry.
    /// - EAGAIN: Explicitly instructed to try again.
    pub(crate) fn receive_retrying(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        loop {
            match self.receive(buffer) {
                Ok(size) => return Ok(size),
                Err(Errno::ENOENT | Errno::EINTR | Errno::EAGAIN) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub(crate) fn sender(&self) -> ChannelSender {
        match self {
            // Since write/writev syscalls are threadsafe, we can simply create
            // a sender by using the same file and use it in other threads.
            Channel::Dev(dev) => ChannelSender::Dev(dev.clone()),
            #[cfg(fuser_fskit)]
            Channel::Fskit(channel) => ChannelSender::Fskit(channel.clone()),
        }
    }

    /// Clone the FUSE device fd using FUSE_DEV_IOC_CLONE ioctl.
    ///
    /// This creates a new fd that can read FUSE requests independently,
    /// enabling true parallel request processing. The kernel distributes
    /// requests across all cloned fds.
    ///
    /// Requires Linux 4.5+. Returns an error on older kernels or non-Linux.
    #[cfg(target_os = "linux")]
    pub(crate) fn clone_fd(&self) -> io::Result<Channel> {
        use std::os::fd::AsRawFd;

        let new_dev = DevFuse::open()?;

        let mut source_fd = self.as_fd().as_raw_fd() as u32;
        // SAFETY: fuse_dev_ioc_clone is a valid ioctl for /dev/fuse
        unsafe {
            crate::ll::ioctl::fuse_dev_ioc_clone(new_dev.as_raw_fd(), &mut source_fd)?;
        }

        Ok(Channel::new(Arc::new(new_dev)))
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ChannelSender {
    Dev(Arc<DevFuse>),
    #[cfg(fuser_fskit)]
    Fskit(Arc<FskitChannel>),
}

impl ChannelSender {
    pub(crate) fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        match self {
            ChannelSender::Dev(dev) => {
                let rc = nix::sys::uio::writev(dev, bufs)?;
                // writev is atomic, so do not need to check how many bytes are written.
                // libfuse does not do it either
                // https://github.com/libfuse/libfuse/blob/6278995cca991978abd25ebb2c20ebd3fc9e8a13/lib/fuse_lowlevel.c#L267
                debug_assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), rc);
                Ok(())
            }
            #[cfg(fuser_fskit)]
            ChannelSender::Fskit(channel) => channel.send(bufs),
        }
    }

    pub(crate) fn open_backing(&self, fd: BorrowedFd<'_>) -> std::io::Result<BackingId> {
        match self {
            ChannelSender::Dev(dev) => BackingId::create(dev, fd),
            #[cfg(fuser_fskit)]
            ChannelSender::Fskit(_) => Err(io::Error::from_raw_os_error(libc::ENOTSUP)),
        }
    }

    pub(crate) unsafe fn wrap_backing(&self, id: u32) -> BackingId {
        match self {
            ChannelSender::Dev(dev) => unsafe { BackingId::wrap_raw(dev, id) },
            #[cfg(fuser_fskit)]
            ChannelSender::Fskit(_) => unreachable!("FSKit channels have no backing files"),
        }
    }
}
