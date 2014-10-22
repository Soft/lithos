#![allow(dead_code)]

use std::c_str::{CString, ToCStr};
use std::ptr::null;
use std::io::IoError;
use std::os::getcwd;
use std::collections::TreeMap;
use std::collections::enum_set::{EnumSet, CLike};

use libc::{c_int, c_char, pid_t};

#[deriving(Show)]
enum Namespace {
    NewMount,
    NewUts,
    NewIpc,
    NewUser,
    NewPid,
    NewNet,
}

impl CLike for Namespace {
    fn to_uint(&self) -> uint {
        match *self {
            NewMount => 0,
            NewUts => 1,
            NewIpc => 2,
            NewUser => 3,
            NewPid => 4,
            NewNet => 5,
        }
    }
    fn from_uint(val: uint) -> Namespace {
        match val {
            0 => NewMount,
            1 => NewUts,
            2 => NewIpc,
            3 => NewUser,
            4 => NewPid,
            5 => NewNet,
            _ => unreachable!(),
        }
    }
}

pub struct Command {
    name: String,
    chroot: CString,
    executable: CString,
    arguments: Vec<CString>,
    environment: TreeMap<String, String>,
    namespaces: EnumSet<Namespace>,
    restore_sigmask: bool,
    user_id: uint,
    workdir: CString,
}


impl Command {
    pub fn new<T:ToCStr>(name: String, cmd: T) -> Command {
        return Command {
            name: name,
            chroot: "/".to_c_str(),
            workdir: getcwd().to_c_str(),
            executable: cmd.to_c_str(),
            arguments: vec!(cmd.to_c_str()),
            namespaces: EnumSet::empty(),
            environment: TreeMap::new(),
            restore_sigmask: true,
            user_id: 0,
        };
    }
    pub fn set_user_id(&mut self, uid: uint) {
        self.user_id = uid;
    }
    pub fn chroot(&mut self, dir: &Path) {
        self.chroot = dir.to_c_str();
    }
    pub fn set_workdir(&mut self, dir: &Path) {
        self.workdir = dir.to_c_str();
    }
    pub fn keep_sigmask(&mut self) {
        self.restore_sigmask = false;
    }
    pub fn arg<T:ToCStr>(&mut self, arg: T) {
        self.arguments.push(arg.to_c_str());
    }
    pub fn args<T:ToCStr>(&mut self, arg: &[T]) {
        self.arguments.extend(arg.iter().map(|v| v.to_c_str()));
    }
    pub fn set_env(&mut self, key: String, value: String) {
        self.environment.insert(key, value);
    }

    pub fn update_env<'x, I: Iterator<(&'x String, &'x String)>>(&mut self,
        mut env: I)
    {
        for (k, v) in env {
            self.environment.insert(k.clone(), v.clone());
        }
    }

    pub fn container(&mut self, network: bool) {
        self.namespaces.add(NewMount);
        self.namespaces.add(NewUts);
        self.namespaces.add(NewIpc);
        self.namespaces.add(NewPid);
        if network {
            self.namespaces.add(NewNet);
        }
    }
    pub fn spawn(&self) -> Result<pid_t, IoError> {
        let mut exec_args: Vec<*const u8> = self.arguments.iter()
            .map(|a| a.as_bytes().as_ptr()).collect();
        exec_args.push(null());
        let environ_cstr: Vec<CString> = self.environment.iter()
            .map(|(k, v)| (*k + "=" + *v).to_c_str()).collect();
        let mut exec_environ: Vec<*const u8> = environ_cstr.iter()
            .map(|p| p.as_bytes().as_ptr()).collect();
        exec_environ.push(null());

        let logprefix = format!(
            // Only errors are logged from C code
            "ERROR:lithos::container.c: [{}]", self.name
            ).to_c_str();
        let pid = unsafe { execute_command(&CCommand {
            logprefix: logprefix.as_bytes().as_ptr(),
            fs_root: self.chroot.as_bytes().as_ptr(),
            exec_path: self.executable.as_bytes().as_ptr(),
            exec_args: exec_args.as_slice().as_ptr(),
            exec_environ: exec_environ.as_slice().as_ptr(),
            namespaces: convert_namespaces(self.namespaces),
            user_id: self.user_id as i32,
            restore_sigmask: if self.restore_sigmask { 1 } else { 0 },
            workdir: self.workdir.as_ptr(),
        }) };
        if pid < 0 {
            return Err(IoError::last_error());
        }
        return Ok(pid)
    }
}


fn convert_namespaces(set: EnumSet<Namespace>) -> c_int {
    let mut ns = 0;
    for i in set.iter() {
        ns |= match i {
            NewMount => CLONE_NEWNS,
            NewUts => CLONE_NEWUTS,
            NewIpc => CLONE_NEWIPC,
            NewUser => CLONE_NEWUSER,
            NewPid => CLONE_NEWPID,
            NewNet => CLONE_NEWNET,
        };
    }
    return ns;
}

static CLONE_NEWNS: c_int = 0x00020000;   /* Set to create new namespace.  */
static CLONE_NEWUTS: c_int = 0x04000000;  /* New utsname group.  */
static CLONE_NEWIPC: c_int = 0x08000000;  /* New ipcs.  */
static CLONE_NEWUSER: c_int = 0x10000000; /* New user namespace.  */
static CLONE_NEWPID: c_int = 0x20000000;  /* New pid namespace.  */
static CLONE_NEWNET: c_int = 0x40000000;  /* New network namespace.  */

#[repr(C)]
pub struct CCommand {
    namespaces: c_int,
    user_id: c_int,
    restore_sigmask: c_int,
    logprefix: *const u8,
    fs_root: *const u8,
    exec_path: *const u8,
    exec_args: *const*const u8,
    exec_environ: *const*const u8,
    workdir: *const c_char,
}

#[link(name="container", kind="static")]
extern {
    fn execute_command(cmd: *const CCommand) -> pid_t;
}

