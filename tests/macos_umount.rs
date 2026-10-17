//! `BackgroundSession::umount` against a real macFUSE mount, kext and FSKit.
//! Needs an approved macFUSE install, so it runs only with
//! `FUSER_MACFUSE_TESTS=1`.

#![cfg(target_os = "macos")]

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;
use std::time::UNIX_EPOCH;

use fuser::BackgroundSession;
use fuser::Config;
use fuser::Errno;
use fuser::FileAttr;
use fuser::FileHandle;
use fuser::FileType;
use fuser::Filesystem;
use fuser::INodeNo;
use fuser::MountOption;
use fuser::ReplyAttr;
use fuser::Request;

const ROOT_ATTR: FileAttr = FileAttr {
    ino: INodeNo::ROOT,
    size: 0,
    blocks: 0,
    atime: UNIX_EPOCH,
    mtime: UNIX_EPOCH,
    ctime: UNIX_EPOCH,
    crtime: UNIX_EPOCH,
    kind: FileType::Directory,
    perm: 0o755,
    nlink: 2,
    uid: 0,
    gid: 0,
    rdev: 0,
    flags: 0,
    blksize: 512,
};

/// An empty root directory.
struct EmptyFs;

impl Filesystem for EmptyFs {
    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if ino == INodeNo::ROOT {
            reply.attr(&Duration::ZERO, &ROOT_ATTR);
        } else {
            reply.error(Errno::ENOENT);
        }
    }
}

fn enabled() -> bool {
    std::env::var_os("FUSER_MACFUSE_TESTS").is_some()
}

/// Mounts `EmptyFs` at `dir`, returning once the mount is visible there.
fn mount(dir: &Path, fskit: bool) -> BackgroundSession {
    let unmounted_dev = std::fs::metadata(dir).unwrap().dev();
    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("umount-test".to_string())];
    if fskit {
        config
            .mount_options
            .push(MountOption::CUSTOM("backend=fskit".to_string()));
    }
    let session = fuser::spawn_mount(EmptyFs, dir, &config).unwrap();
    // macFUSE finishes the mount in a helper process after `spawn_mount`
    // returns.
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(dir).map(|m| m.dev()).ok() == Some(unmounted_dev) {
        assert!(Instant::now() < deadline, "{} never mounted", dir.display());
        std::thread::sleep(Duration::from_millis(1));
    }
    session
}

/// `session.umount()`, retried on `EBUSY`: macOS services briefly probe a
/// newly mounted volume's root, which makes `unmount(2)` fail meanwhile.
fn umount_when_idle(session: &mut BackgroundSession) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match session.umount() {
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            result => return result,
        }
    }
}

/// Force-unmounts `dir` if a failing test leaves it mounted, while this
/// process can still serve the requests that entails: a mount outliving its
/// server can hang `umount -f`.
struct ForceUnmountOnPanic<'a> {
    dir: &'a Path,
    mounted_dev: u64,
}

impl Drop for ForceUnmountOnPanic<'_> {
    fn drop(&mut self) {
        let still_mounted =
            std::fs::metadata(self.dir).map(|m| m.dev()).ok() == Some(self.mounted_dev);
        if std::thread::panicking() && still_mounted {
            let _ = std::process::Command::new("/sbin/umount")
                .arg("-f")
                .arg(self.dir)
                .status();
        }
    }
}

fn a_failed_umount_keeps_the_mount_and_can_be_retried(fskit: bool) {
    if !enabled() {
        eprintln!("skipped: set FUSER_MACFUSE_TESTS=1 to run");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut session = mount(dir.path(), fskit);
    let mounted_dev = std::fs::metadata(dir.path()).unwrap().dev();
    let _cleanup = ForceUnmountOnPanic {
        dir: dir.path(),
        mounted_dev,
    };

    let root = std::fs::File::open(dir.path()).unwrap();
    let err = session.umount().unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EBUSY), "{err}");
    assert_eq!(
        std::fs::metadata(dir.path()).unwrap().dev(),
        mounted_dev,
        "still mounted and served after the failed umount"
    );

    drop(root);
    umount_when_idle(&mut session).unwrap();
    assert_ne!(std::fs::metadata(dir.path()).unwrap().dev(), mounted_dev);
    session.join().unwrap();
}

#[test]
fn kext_failed_umount_keeps_the_mount_and_can_be_retried() {
    a_failed_umount_keeps_the_mount_and_can_be_retried(false);
}

#[test]
fn fskit_failed_umount_keeps_the_mount_and_can_be_retried() {
    a_failed_umount_keeps_the_mount_and_can_be_retried(true);
}

/// `/sbin/umount dir`, retried while busy (see `umount_when_idle`).
fn external_umount(dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::process::Command::new("/sbin/umount")
        .arg(dir)
        .status()
        .unwrap()
        .success()
    {
        assert!(Instant::now() < deadline, "umount {} failed", dir.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn after_an_external_umount_drop_leaves_a_new_mount_alone(fskit: bool) {
    if !enabled() {
        eprintln!("skipped: set FUSER_MACFUSE_TESTS=1 to run");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut first = mount(dir.path(), fskit);
    external_umount(dir.path());
    let err = first.umount().unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "{err}");

    let mut second = mount(dir.path(), fskit);
    let mounted_dev = std::fs::metadata(dir.path()).unwrap().dev();
    let _cleanup = ForceUnmountOnPanic {
        dir: dir.path(),
        mounted_dev,
    };
    // Drops `first`'s mount.
    first.join().unwrap();
    assert_eq!(
        std::fs::metadata(dir.path()).unwrap().dev(),
        mounted_dev,
        "the second mount survives the first session's end"
    );

    umount_when_idle(&mut second).unwrap();
    assert_ne!(std::fs::metadata(dir.path()).unwrap().dev(), mounted_dev);
    second.join().unwrap();
}

#[test]
fn kext_drop_after_an_external_umount_leaves_a_new_mount_alone() {
    after_an_external_umount_drop_leaves_a_new_mount_alone(false);
}

#[test]
fn fskit_drop_after_an_external_umount_leaves_a_new_mount_alone() {
    after_an_external_umount_drop_leaves_a_new_mount_alone(true);
}
