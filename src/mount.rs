#![allow(dead_code)]
use std::io::{IoError};
use std::io::BufferedReader;
use std::ffi::CString;
use std::ptr::null;
use std::io::fs::File;
use std::str::FromStr;
use std::path::BytesContainer;
use libc::{c_ulong, c_int};

// sys/mount.h
static MS_RDONLY: c_ulong = 1;                /* Mount read-only.  */
static MS_NOSUID: c_ulong = 2;                /* Ignore suid and sgid bits.  */
static MS_NODEV: c_ulong = 4;                 /* Disallow access to device special files.  */
static MS_NOEXEC: c_ulong = 8;                /* Disallow program execution.  */
static MS_SYNCHRONOUS: c_ulong = 16;          /* Writes are synced at once.  */
static MS_REMOUNT: c_ulong = 32;              /* Alter flags of a mounted FS.  */
static MS_MANDLOCK: c_ulong = 64;             /* Allow mandatory locks on an FS.  */
static MS_DIRSYNC: c_ulong = 128;             /* Directory modifications are synchronous.  */
static MS_NOATIME: c_ulong = 1024;            /* Do not update access times.  */
static MS_NODIRATIME: c_ulong = 2048;         /* Do not update directory access times.  */
static MS_BIND: c_ulong = 4096;               /* Bind directory at different place.  */
static MS_MOVE: c_ulong = 8192;
static MS_REC: c_ulong = 16384;
static MS_SILENT: c_ulong = 32768;
static MS_POSIXACL: c_ulong = 1 << 16;        /* VFS does not apply the umask.  */
static MS_UNBINDABLE: c_ulong = 1 << 17;      /* Change to unbindable.  */
static MS_PRIVATE: c_ulong = 1 << 18;         /* Change to private.  */
static MS_SLAVE: c_ulong = 1 << 19;           /* Change to slave.  */
static MS_SHARED: c_ulong = 1 << 20;          /* Change to shared.  */
static MS_RELATIME: c_ulong = 1 << 21;        /* Update atime relative to mtime/ctime.  */
static MS_KERNMOUNT: c_ulong = 1 << 22;       /* This is a kern_mount call.  */
static MS_I_VERSION: c_ulong =  1 << 23;      /* Update inode I_version field.  */
static MS_STRICTATIME: c_ulong = 1 << 24;     /* Always perform atime updates.  */
static MS_ACTIVE: c_ulong = 1 << 30;
static MS_NOUSER: c_ulong = 1 << 31;

static MNT_FORCE: c_int = 1;           /* Force unmounting.  */
static MNT_DETACH: c_int = 2;          /* Just detach from the tree.  */
static MNT_EXPIRE: c_int = 4;          /* Mark for expiry.  */
static UMOUNT_NOFOLLOW: c_int = 8;     /* Don't follow symlink on umount.  */


extern {
    fn mount(source: *const u8, target: *const u8,
        filesystemtype: *const u8, flags: c_ulong,
        data: *const u8) -> c_int;
    fn umount(target: *const u8) -> c_int;
    fn umount2(target: *const u8, flags: c_int) -> c_int;
}


pub struct MountRecord<'a> {
    pub mount_id: usize,
    pub parent_id: usize,
    _device: &'a str,  // TODO(tailhook) parse if ever need
    pub relative_root: &'a str,
    pub mount_point: &'a str,
    pub mount_options: &'a str,
    pub tag_shared: Option<usize>,
    pub tag_master: Option<usize>,
    pub tag_propagate_from: Option<usize>,
    pub tag_unbindable: Option<()>,
    pub fstype: &'a str,
    pub mount_source: &'a str,
    pub super_options: &'a str,
}

impl<'a> MountRecord<'a> {
    pub fn from_str<'x>(line: &'x str) -> Option<MountRecord<'x>> {
        let mut parts = line.words();
        let mount_id = try_opt!(parts.next().and_then(FromStr::from_str));
        let parent_id = try_opt!(parts.next().and_then(FromStr::from_str));
        let device = try_opt!(parts.next());
        let relative_root = try_opt!(parts.next());
        let mount_point = try_opt!(parts.next());
        let mount_options = try_opt!(parts.next());
        let mut tag_shared = None;
        let mut tag_master = None;
        let mut tag_propagate_from = None;
        let mut tag_unbindable = None;

        for name in parts {
            if name == "-" { break; }
            let mut pair = name.splitn(1, ':');
            let key = pair.next();
            let value = pair.next();
            match key {
                Some("shared") => {
                    tag_shared = Some(try_opt!(
                        value.and_then(FromStr::from_str)));
                }
                Some("master") => {
                    tag_master = Some(try_opt!(
                        value.and_then(FromStr::from_str)));
                }
                Some("propagate_from") => {
                    tag_propagate_from = Some(try_opt!(
                        value.and_then(FromStr::from_str)));
                }
                Some("unbindable") => tag_unbindable = Some(()),
                _ => {}
            }
        }

        let fstype = try_opt!(parts.next());
        let mount_source = try_opt!(parts.next());
        let super_options = try_opt!(parts.next());

        return Some(MountRecord {
            mount_id: mount_id,
            parent_id: parent_id,
            _device: device,
            relative_root: relative_root,
            mount_point: mount_point,
            mount_options: mount_options,
            tag_shared: tag_shared,
            tag_master: tag_master,
            tag_propagate_from: tag_propagate_from,
            tag_unbindable: tag_unbindable,
            fstype: fstype,
            mount_source: mount_source,
            super_options: super_options,
            });
    }
    pub fn is_private(&self) -> bool {
        return self.tag_shared.is_none()
            && self.tag_master.is_none()
            && self.tag_propagate_from.is_none()
            && self.tag_unbindable.is_none();
    }
}

pub fn mount_ro_recursive(target: &Path) -> Result<(), String> {
    let none = CString::from_slice("none".as_bytes());
    debug!("Remount readonly: {}", target.display());
    let c_target = CString::from_slice(target.container_as_bytes());
    let rc = unsafe { mount(
       none.as_bytes().as_ptr(),
       c_target.as_bytes().as_ptr(),
       null(), MS_BIND|MS_REMOUNT|MS_RDONLY, null()) };
    if rc != 0 {
        let err = IoError::last_error();
        return Err(format!("Remount readonly {}: {}", target.display(), err));
    }
    return Ok(());
}

pub fn mount_private(target: &Path) -> Result<(), String> {
    let none = CString::from_slice("none".container_as_bytes());
    let c_target = CString::from_slice(target.container_as_bytes());
    debug!("Making private {}", target.display());
    let rc = unsafe { mount(
        none.as_bytes().as_ptr(),
        c_target.as_bytes().as_ptr(),
        null(), MS_REC|MS_PRIVATE, null()) };
    if rc == 0 {
        return Ok(());
    } else {
        let err = IoError::last_error();
        return Err(format!("Can't make {} a slave: {}",
            target.display(), err));
    }
}

pub fn bind_mount(source: &Path, target: &Path) -> Result<(), String> {
    let c_source = CString::from_slice(source.container_as_bytes());
    let c_target = CString::from_slice(target.container_as_bytes());
    debug!("Bind mount {} -> {}", source.display(), target.display());
    let rc = unsafe {
        mount(c_source.as_bytes().as_ptr(), c_target.as_bytes().as_ptr(),
        null(), MS_BIND|MS_REC, null()) };
    if rc == 0 {
        return Ok(());
    } else {
        let err = IoError::last_error();
        return Err(format!("Can't mount bind {} to {}: {}",
            source.display(), target.display(), err));
    }
}

pub fn mount_pseudo(target: &Path, name: &str, options: &str, readonly: bool)
    -> Result<(), String>
{
    let c_name = CString::from_slice(name.container_as_bytes());
    let c_target = CString::from_slice(target.container_as_bytes());
    let c_opts = CString::from_slice(options.container_as_bytes());
    let mut flags = MS_NOSUID | MS_NOEXEC | MS_NODEV | MS_NOATIME;
    if readonly {
        flags |= MS_RDONLY;
    }
    debug!("Pseusofs mount {} {} {}", target.display(), name, options);
    let rc = unsafe { mount(
        c_name.as_bytes().as_ptr(),
        c_target.as_bytes().as_ptr(),
        c_name.as_bytes().as_ptr(),
        flags,
        c_opts.as_bytes().as_ptr()) };
    if rc == 0 {
        return Ok(());
    } else {
        let err = IoError::last_error();
        return Err(format!("Can't mount pseudofs {} ({}, options: {}): {}",
            target.display(), options, name, err));
    }
}

pub fn mount_tmpfs(target: &Path, options: &str) -> Result<(), String> {
    let c_tmpfs = CString::from_slice("tmpfs".as_bytes());
    let c_target = CString::from_slice(target.container_as_bytes());
    let c_opts = CString::from_slice(options.container_as_bytes());
    debug!("Tmpfs mount {} {}", target.display(), options);
    let rc = unsafe { mount(
        c_tmpfs.as_bytes().as_ptr(),
        c_target.as_bytes().as_ptr(),
        c_tmpfs.as_bytes().as_ptr(),
        MS_NOSUID | MS_NODEV | MS_NOATIME,
        c_opts.as_bytes().as_ptr()) };
    if rc == 0 {
        return Ok(());
    } else {
        let err = IoError::last_error();
        return Err(format!("Can't mount tmpfs {} (options: {}): {}",
            target.display(), options, err));
    }
}

pub fn unmount(target: &Path) -> Result<(), String> {
    let c_target = CString::from_slice(target.container_as_bytes());
    let rc = unsafe { umount2(c_target.as_bytes().as_ptr(), MNT_DETACH) };
    if rc == 0 {
        return Ok(());
    } else {
        let err = IoError::last_error();
        return Err(format!("Can't unmount {} : {}", target.display(), err));
    }
}

