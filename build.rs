fn main() {
    // Register rustc cfg for switching between mount implementations.
    println!(
        "cargo::rustc-check-cfg=cfg(fuser_mount_impl, values(\"pure-rust\", \"libfuse2\", \"libfuse3\", \"macos-no-mount\"))"
    );

    let target_os =
        std::env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS should be set");

    if matches!(
        target_os.as_str(),
        "linux" | "freebsd" | "dragonfly" | "openbsd" | "netbsd"
    ) && cfg!(not(feature = "libfuse"))
    {
        println!("cargo::rustc-cfg=fuser_mount_impl=\"pure-rust\"");
    } else if target_os == "macos" {
        if cfg!(feature = "macos-no-mount") {
            println!("cargo::rustc-cfg=fuser_mount_impl=\"macos-no-mount\"");
        } else {
            // macFUSE 5.3.0+ broke `fuse_mount_compat25`, the entry point
            // the libfuse2 backend calls (returns -1 unconditionally --
            // https://github.com/cberner/fuser/issues/752). libfuse3's
            // `fuse_session_mount` is unaffected, so prefer it; fall back
            // to libfuse2 for older macFUSE/libfuse installs that don't
            // ship a libfuse3 pkg-config file at all.
            //
            // Either way, the wire protocol on the resulting /dev/macfuseN
            // fd is dictated by the macfuse.kext, not by which userspace
            // library performed the mount -- so `macfuse-4-compat`'s ABI
            // struct layout (fuse_abi.rs) is needed for both backends, not
            // just the libfuse2 one it was originally tied to.
            match configure_libfuse3() {
                Ok(()) => {}
                Err(e3) => {
                    pkg_config::Config::new()
                        .atleast_version("2.6.0")
                        .probe("fuse") // for macFUSE 4.x
                        .unwrap_or_else(|e2| {
                            panic!("Failed to configure libfuse3 or libfuse2 on macOS: {e3}; {e2}")
                        });
                    println!("cargo::rustc-cfg=fuser_mount_impl=\"libfuse2\"");
                }
            }
            println!("cargo::rustc-cfg=feature=\"macfuse-4-compat\"");
        }
    } else if cfg!(feature = "libfuse3") {
        configure_libfuse3().unwrap();
    } else if cfg!(feature = "libfuse2") {
        configure_libfuse2().unwrap();
    } else {
        // First try to link with libfuse3
        match configure_libfuse3() {
            Ok(()) => {}
            Err(e3) => {
                // Fallback to libfuse
                match configure_libfuse2() {
                    Ok(()) => {}
                    Err(e2) => {
                        panic!("Failed to configure libfuse3 or libfuse2: {e3}; {e2}");
                    }
                }
            }
        }
    }
}

fn configure_libfuse3() -> Result<(), pkg_config::Error> {
    pkg_config::Config::new()
        .atleast_version("3.0.0")
        .probe("fuse3")?;
    println!("cargo::rustc-cfg=fuser_mount_impl=\"libfuse3\"");
    Ok(())
}

fn configure_libfuse2() -> Result<(), pkg_config::Error> {
    pkg_config::Config::new()
        .atleast_version("2.6.0")
        .probe("fuse")?;
    println!("cargo::rustc-cfg=fuser_mount_impl=\"libfuse2\"");
    Ok(())
}
