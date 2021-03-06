extern crate nix;
extern crate rustc_serialize;
extern crate libc;
extern crate regex;
extern crate argparse;
extern crate quire;
extern crate lithos;
extern crate time;
extern crate fern;
extern crate syslog;
extern crate signal;
extern crate unshare;
extern crate scan_dir;
#[macro_use] extern crate log;


use std::env;
use std::rc::Rc;
use std::mem::replace;
use std::fs::{File, OpenOptions, metadata};
use std::io::{stderr, Read, Write};
use std::str::{FromStr};
use std::fs::{remove_dir};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::default::Default;
use std::process::exit;
use std::collections::{HashMap, BTreeMap, HashSet};
use std::os::unix::io::RawFd;

use time::{SteadyTime, Duration};
use nix::sys::signal::{SIGINT, SIGTERM, SIGCHLD};
use nix::sys::signal::kill;
use nix::sys::socket::{getsockname, SockAddr};
use nix::sys::socket::{setsockopt, bind, listen};
use nix::sys::socket::sockopt::{ReuseAddr, ReusePort};
use nix::sys::socket::{socket, AddressFamily, SockType, SockFlag, InetAddr};
use libc::pid_t;
use libc::funcs::posix88::unistd::{getpid};
use libc::close;
use regex::Regex;
use quire::parse_config;
use rustc_serialize::json;
use unshare::{Command, reap_zombies, Namespace};
use signal::exec_handler;
use signal::trap::Trap;

use lithos::setup::{clean_child, init_logging};
use lithos::master_config::{MasterConfig, create_master_dirs};
use lithos::sandbox_config::SandboxConfig;
use lithos::child_config::ChildConfig;
use lithos::container_config::{ContainerConfig, TcpPort};
use lithos::container_config::ContainerKind::Daemon;
use lithos::utils::{clean_dir, relative, ABNORMAL_TERM_SIGNALS};
use lithos::utils::{temporary_change_root};
use lithos::cgroup;
use lithos_tree_options::Options;
use lithos::timer_queue::Queue;

mod lithos_tree_options;

struct Process {
    restart_min: SteadyTime,
    cmd: Command,
    name: String,
    config: Rc<String>,
    inner_config: Rc<ContainerConfig>,
    ports: Vec<u16>,
}

struct Socket {
    fd: RawFd,
}

enum Child {
    Process(Process),
    Unidentified(String),
}

impl Child {
    fn get_name<'x>(&'x self) -> &'x str {
        match self {
            &Child::Process(ref p) => &p.name,
            &Child::Unidentified(ref name) => name,
        }
    }
}

fn new_child(bin: &Binaries, name: &str, master_fn: &Path,
    cfg: &str, options: &Options)
    -> Command
{
    let mut cmd = Command::new(&bin.lithos_knot);
    // Name is first here, so it's easily visible in ps
    cmd.arg("--name");
    cmd.arg(name);
    cmd.arg("--master");
    cmd.arg(master_fn);
    cmd.arg("--config");
    cmd.arg(cfg);
    if options.log_stderr {
        cmd.arg("--log-stderr");
    }
    if let Some(log_level) = options.log_level {
        cmd.arg(format!("--log-level={}", log_level));
    }
    cmd.env_clear();
    cmd.env("TERM", env::var_os("TERM").unwrap_or(From::from("dumb")));
    if let Some(x) = env::var_os("RUST_LOG") {
        cmd.env("RUST_LOG", x);
    }
    if let Some(x) = env::var_os("RUST_BACKTRACE") {
        cmd.env("RUST_BACKTRACE", x);
    }
    cmd.unshare([Namespace::Mount, Namespace::Uts,
                 Namespace::Ipc, Namespace::Pid].iter().cloned());
    cmd
}

fn check_master_config(cfg: &MasterConfig) -> Result<(), String> {
    if metadata(&cfg.devfs_dir).is_err() {
        return Err(format!(
            "Devfs dir ({:?}) must exist and contain device nodes",
            cfg.devfs_dir));
    }
    return Ok(());
}

fn global_init(master: &MasterConfig, options: &Options)
    -> Result<(), String>
{
    try!(create_master_dirs(&master));
    try!(init_logging(&master, &master.log_file, &master.syslog_app_name,
          options.log_stderr,
          options.log_level
            .or_else(|| FromStr::from_str(&master.log_level).ok())
            .unwrap_or(log::LogLevel::Warn)));
    try!(check_process(&master));
    if let Some(ref name) = master.cgroup_name {
        try!(cgroup::ensure_in_group(name, &master.cgroup_controllers));
    }
    return Ok(());
}

fn global_cleanup(master: &MasterConfig) {
    clean_dir(&master.runtime_dir.join(&master.state_dir), false)
        .unwrap_or_else(|e| error!("Error removing state dir: {}", e));
}

fn discard<E>(_: E) { }

fn _read_args(pid: pid_t, global_config: &Path)
    -> Result<(String, String), ()>
{
    let mut buf = String::with_capacity(4096);
    try!(File::open(&format!("/proc/{}/cmdline", pid))
         .and_then(|mut f| f.read_to_string(&mut buf))
         .map_err(discard));
    let args: Vec<&str> = buf[..].splitn(8, '\0').collect();
    if args.len() != 8
       || Path::new(args[0]).file_name()
          .and_then(|x| x.to_str()) != Some("lithos_knot")
       || args[1] != "--name"
       || args[3] != "--master"
       || Path::new(args[4]) != global_config
       || args[5] != "--config"
       || args[7] != ""
    {
       return Err(());
    }
    return Ok((args[2].to_string(), args[6].to_string()));
}

fn _is_child(pid: pid_t, ppid: pid_t) -> bool {
    let mut buf = String::with_capacity(256);
    let ppid_regex = Regex::new(r"^\d+\s+\([^)]*\)\s+\S+\s+(\d+)\s").unwrap();
    if File::open(&format!("/proc/{}/stat", pid))
       .and_then(|mut f| f.read_to_string(&mut buf))
       .is_err() {
        return false;
    }
    return Some(ppid) == ppid_regex.captures(&buf)
                     .and_then(|c| FromStr::from_str(c.at(1).unwrap()).ok());
}


fn check_process(cfg: &MasterConfig) -> Result<(), String> {
    let mypid = unsafe { getpid() };
    let pid_file = cfg.runtime_dir.join("master.pid");
    if metadata(&pid_file).is_ok() {
        let mut buf = String::with_capacity(50);
        match File::open(&pid_file)
            .and_then(|mut f| f.read_to_string(&mut buf))
            .map_err(|_| ())
            .and_then(|_| FromStr::from_str(&buf[..].trim())
                            .map_err(|_| ()))
        {
            Ok::<pid_t, ()>(pid) if pid == mypid => {
                return Ok(());
            }
            Ok(pid) => {
                if kill(pid, 0).is_ok() {
                    return Err(format!(concat!("Master pid is {}. ",
                        "And there is alive process with ",
                        "that pid."), pid));

                }
            }
            _ => {
                warn!("Pid file exists, but cannot be read");
            }
        }
    }
    try!(File::create(&pid_file)
        .and_then(|mut f| write!(f, "{}\n", unsafe { getpid() }))
        .map_err(|e| format!("Can't write file {:?}: {}", pid_file, e)));
    return Ok(());
}

fn recover_sockets(sockets: &mut HashMap<u16, Socket>) {
    scan_dir::ScanDir::all().read("/proc/self/fd", |iter| {
        let fds = iter
            .filter_map(|(_, name)| FromStr::from_str(&name).ok())
            .filter(|&x| x >= 3);
        for fd in fds {
            match getsockname(fd) {
                Ok(SockAddr::Inet(addr)) => {
                    let sock = Socket {
                        fd: fd,
                    };
                    match sockets.insert(addr.port(), sock) {
                        None => {}
                        Some(old) => {
                            error!("Port {} has two sockets: fd={} and fd={}, \
                                discarding latter.",
                                addr.port(), fd, old.fd);
                        }
                    }
                }
                Ok(SockAddr::Unix(_)) => {
                    debug!("Fd {} is unix socket", fd);
                }
                Err(_) => {
                    debug!("Fd {} is not a socket", fd);
                }
            }
        }
    }).map_err(|e| error!("Error enumerating my fds: {}", e)).ok();
}

fn recover_processes(children: &mut HashMap<pid_t, Child>,
    configs: &mut HashMap<String, Process>, config_file: &Path)
{
    let mypid = unsafe { getpid() };

    // Recover old workers
    scan_dir::ScanDir::all().read("/proc", |iter| {
        let pids = iter.filter_map(|(_, pid)| FromStr::from_str(&pid).ok());
        for pid in pids {
            if !_is_child(pid, mypid) {
                continue;
            }
            if let Ok((name, cfg_text)) = _read_args(pid, config_file) {
                match configs.remove(&name) {
                    Some(child) => {
                        if &child.config[..] != &cfg_text[..] {
                            warn!("Config mismatch: {}, pid: {}. Upgrading...",
                                  name, pid);
                            kill(pid, SIGTERM)
                            .map_err(|e|
                                error!("Error sending TERM to {}: {:?}",
                                    pid, e)).ok();
                            // TODO(tailhook) add to unidentified list?
                        }
                        children.insert(pid, Child::Process(child));
                    }
                    None => {
                        warn!("Undefined child name: {}, pid: {}. \
                            Sending SIGTERM...", name, pid);
                        children.insert(pid, Child::Unidentified(name));
                        kill(pid, SIGTERM)
                        .map_err(|e| error!("Error sending TERM to {}: {:?}",
                            pid, e)).ok();
                    }
                };
            } else {
                warn!("Undefined child, pid: {}. Sending SIGTERM...", pid);
                kill(pid, SIGTERM)
                    .map_err(|e| error!("Error sending TERM to {}: {:?}",
                        pid, e)).ok();
                continue;
            }
        }
    }).map_err(|e| error!("Error reading /proc: {}", e)).ok();
}

fn remove_dangling_state_dirs(names: &HashSet<String>, master: &MasterConfig)
{
    let pid_regex = Regex::new(r"\.\(\d+\)$").unwrap();
    let master = master.runtime_dir.join(&master.state_dir);
    scan_dir::ScanDir::dirs().read(&master, |iter| {
        for (entry, sandbox_name) in iter {
            let path = entry.path();
            debug!("Checking sandbox dir: {:?}", path);
            let mut valid_dirs = 0;
            scan_dir::ScanDir::dirs().read(&path, |iter| {
                for (entry, proc_name) in iter {
                    let name = format!("{}/{}", sandbox_name, proc_name);
                    debug!("Checking process dir: {}", name);
                    if names.contains(&name) {
                        valid_dirs += 1;
                        continue;
                    } else if proc_name.starts_with("cmd.") {
                        debug!("Checking command dir: {}", name);
                        let pid = pid_regex.captures(&proc_name).and_then(
                            |c| FromStr::from_str(c.at(1).unwrap()).ok());
                        if let Some(pid) = pid {
                            if kill(pid, 0).is_ok() {
                                valid_dirs += 1;
                                continue;
                            }
                        }
                    }
                    let path = entry.path();
                    warn!("Dangling state dir {:?}. Deleting...", path);
                    clean_dir(&path, true)
                        .map_err(|e| error!(
                            "Can't remove dangling state dir {:?}: {}",
                            path, e))
                        .ok();
                }
            }).map_err(|e|
                error!("Error reading state dir {:?}: {}", path, e)).ok();
            debug!("Tree dir {:?} has {} valid subdirs", path, valid_dirs);
            if valid_dirs > 0 {
                continue;
            }
            warn!("Empty sandbox dir {:?}. Deleting...", path);
            clean_dir(&path, true)
                .map_err(|e| error!("Can't empty state dir {:?}: {}", path, e))
                .ok();
        }
    }).map_err(|e| error!("Error listing state dir: {}", e)).ok();
}

fn _rm_cgroup(dir: &Path) {
    if let Err(e) = remove_dir(dir) {
        let mut buf = String::with_capacity(1024);
        File::open(&dir.join("cgroup.procs"))
            .and_then(|mut f| f.read_to_string(&mut buf))
            .ok();
        error!("Error removing cgroup: {} (processes {:?})",
            e, buf);
    }
}

fn remove_dangling_cgroups(names: &HashSet<String>, master: &MasterConfig)
{
    if master.cgroup_name.is_none() {
        return;
    }
    let cgroups = match cgroup::parse_cgroups(None) {
        Ok(cgroups) => cgroups,
        Err(e) => {
            error!("Can't parse my cgroups: {}", e);
            return;
        }
    };
    // TODO(tailhook) need to customize cgroup mount point?
    let cgroup_base = Path::new("/sys/fs/cgroup");
    let root_path = Path::new("/");
    let child_group_regex = Regex::new(r"^([\w-]+):([\w-]+\.\d+)\.scope$")
        .unwrap();
    let cmd_group_regex = Regex::new(r"^([\w-]+):cmd\.[\w-]+\.(\d+)\.scope$")
        .unwrap();
    let cgroup_filename = master.cgroup_name.as_ref().map(|x| &x[..]);

    // Loop over all controllers in case someone have changed config
    for cgrp in cgroups.all_groups.iter() {
        let cgroup::CGroupPath(ref folder, ref path) = **cgrp;
        let ctr_dir = cgroup_base.join(&folder).join(
            &relative(path, &root_path));
        if path.file_name().and_then(|x| x.to_str()) == cgroup_filename {
            debug!("Checking controller dir: {:?}", ctr_dir);
        } else {
            debug!("Skipping controller dir: {:?}", ctr_dir);
            continue;
        }
        scan_dir::ScanDir::dirs().read(&ctr_dir, |iter| {
            for (entry, filename) in iter {
                if let Some(capt) = child_group_regex.captures(&filename) {
                    let name = format!("{}/{}",
                        capt.at(1).unwrap(), capt.at(2).unwrap());
                    if !names.contains(&name) {
                        _rm_cgroup(&entry.path());
                    }
                } else if let Some(capt) = cmd_group_regex.captures(&filename)
                {
                    let pid = FromStr::from_str(capt.at(2).unwrap()).ok();
                    if pid.is_none() || !kill(pid.unwrap(), 0).is_ok() {
                        _rm_cgroup(&entry.path());
                    }
                } else {
                    warn!("Skipping wrong group {:?}", entry.path());
                    continue;
                }
            }
        }).map_err(|e| error!("Error reading cgroup dir {:?}: {}",
            ctr_dir, e)).ok();
    }
}

fn run(config_file: &Path, options: &Options)
    -> Result<(), String>
{
    let master: MasterConfig = try!(parse_config(&config_file,
        &MasterConfig::validator(), Default::default())
        .map_err(|e| format!("Error reading master config: {}", e)));
    try!(check_master_config(&master));
    try!(global_init(&master, &options));

    let bin = match get_binaries() {
        Some(bin) => bin,
        None => {
            exit(127);
        }
    };

    let mut trap = Trap::trap(&[SIGINT, SIGTERM, SIGCHLD]);
    let config_file = config_file.to_owned();

    let mut configs = read_sandboxes(&master, &bin, &config_file, options);

    info!("Recovering Sockets");
    let mut sockets = HashMap::new();
    recover_sockets(&mut sockets);
    info!("Recovering Processes");
    let mut children = HashMap::new();
    recover_processes(&mut children, &mut configs, &config_file);
    close_unused_sockets(&mut sockets, &mut children);

    let recovered = children.values()
        .map(|c| c.get_name().to_string()).collect();

    info!("Removing Dangling State Dirs");
    remove_dangling_state_dirs(&recovered, &master);

    info!("Removing Dangling CGroups");
    remove_dangling_cgroups(&recovered, &master);

    info!("Starting Processes");
    let mut queue = schedule_new_workers(configs);

    normal_loop(&mut queue, &mut children, &mut sockets, &mut trap, &master);
    if children.len() > 0 {
        shutdown_loop(&mut children, &mut sockets, &mut trap, &master);
    }

    global_cleanup(&master);

    return Ok(());
}

fn close_unused_sockets(sockets: &mut HashMap<u16, Socket>,
                        children: &HashMap<pid_t, Child>)
{
    let empty = Vec::new();
    let used_ports: HashSet<u16> = children.values().flat_map(|ch| {
        match ch {
            &Child::Process(ref p) => p.ports.iter().cloned(),
            &Child::Unidentified(_) => empty.iter().cloned(),
        }
    }).collect();
    *sockets = replace(sockets, HashMap::new())
        .into_iter().filter(|&(p, ref s)| {
            if used_ports.contains(&p) {
                true
            } else {
                unsafe { close(s.fd) };
                false
            }
        }).collect();
}

fn open_socket(port: u16, cfg: &TcpPort) -> Result<RawFd, String> {
    let sock = try!(socket(AddressFamily::Inet,
            SockType::Stream, SockFlag::empty())
            .map_err(|e| format!("Can't create socket: {:?}", e)));
    let mut result = Ok(());
    if cfg.reuse_addr {
        result = result.and_then(|_| setsockopt(sock, ReuseAddr, &true));
    }
    if cfg.reuse_port {
        result = result.and_then(|_| setsockopt(sock, ReusePort, &true));
    }
    let addr = SockAddr::Inet(InetAddr::from_std(
        &(&cfg.host[..], port).to_socket_addrs().unwrap().next().unwrap()
        ));
    result =  result.and_then(|_| bind(sock, &addr));
    result =  result.and_then(|_| listen(sock, cfg.listen_backlog));
    if let Err(e) = result {
        unsafe { close(sock) };
        Err(format!("Socket option error: {:?}", e))
    } else {
        Ok(sock)
    }
}

fn open_sockets_for(socks: &mut HashMap<u16, Socket>,
                    ports: &HashMap<u16, TcpPort>,
                    cmd: &mut Command)
    -> Result<(), String>
{
    for (&port, item) in ports {
        if !socks.contains_key(&port) {
            if !item.reuse_port {
                let sock = try!(open_socket(port, item));
                socks.insert(port, Socket {
                    fd: sock,
                });
            }
        }
    }

    cmd.reset_fds();
    if socks.len() > 0 {
        cmd.close_fds(socks.values().map(|x| x.fd).min().unwrap()
                      ..(socks.values().map(|x| x.fd).max().unwrap() + 1));
        for (p, item) in ports.iter() {
            unsafe {
                cmd.file_descriptor_raw(item.fd, socks.get(p).unwrap().fd);
            }
        }
    }
    Ok(())
}

fn normal_loop(queue: &mut Queue<Process>,
    children: &mut HashMap<pid_t, Child>,
    sockets: &mut HashMap<u16, Socket>,
    trap: &mut Trap, master: &MasterConfig)
{
    loop {
        let now = SteadyTime::now();

        let mut buf = Vec::new();
        for mut child in queue.pop_until(now) {
            let restart_min = now + Duration::milliseconds(
                (child.inner_config.restart_timeout * 1000.) as i64);
            match open_sockets_for(sockets, &child.inner_config.tcp_ports,
                                   &mut child.cmd)
            {
                Ok(()) => {}
                Err(e) => {
                    error!("Error starting {:?}, error opening sockets: {}",
                        child.name, e);
                    buf.push((restart_min, child));
                    continue;
                }
            }
            match child.cmd.spawn() {
                Ok(c) => {
                    child.restart_min = restart_min;
                    children.insert(c.pid(), Child::Process(child));
                }
                Err(e) => {
                    error!("Error starting {:?}: {}", child.name, e);
                    buf.push((restart_min, child));
                }
            }
        }
        for (restart_min, v) in buf.into_iter() {
            queue.add(restart_min, v);
        }

        close_unused_sockets(sockets, children);
        let next_signal = match queue.peek_time() {
            Some(deadline) => trap.wait(deadline),
            None => trap.next(),
        };
        match next_signal {
            None => {
                continue;
            }
            Some(SIGINT) => {
                // SIGINT is usually a Ctrl+C so it's sent to whole
                // process group, so we don't need to do anything special
                debug!("Received SIGINT. Waiting process to stop..");
                return;
            }
            Some(SIGTERM) => {
                // SIGTERM is usually sent to a specific process so we
                // forward it to children
                debug!("Received SIGTERM signal, propagating");
                for (&pid, _) in children {
                    kill(pid, SIGTERM).ok();
                }
                return;
            }
            Some(SIGCHLD) => {
                for (pid, status) in reap_zombies() {
                    match children.remove(&pid) {
                        Some(Child::Process(child)) => {
                            error!("Process {:?} {}", child.name, status);
                            clean_child(&child.name, &master);
                            queue.add(child.restart_min, child);
                        }
                        Some(Child::Unidentified(name)) => {
                            clean_child(&name, &master);
                        }
                        None => {
                            info!("Unknown process {:?} {}", pid, status);
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
    }
}

fn shutdown_loop(children: &mut HashMap<pid_t, Child>,
    sockets: &mut HashMap<u16, Socket>,
    trap: &mut Trap,
    master: &MasterConfig)
{
    for sig in trap {
        match sig {
            SIGINT => {
                // SIGINT is usually a Ctrl+C so it's sent to whole
                // process group, so we don't need to do anything special
                debug!("Received SIGINT. Waiting process to stop..");
                continue;
            }
            SIGTERM => {
                // SIGTERM is usually sent to a specific process so we
                // forward it to children
                debug!("Received SIGTERM signal, propagating");
                for &pid in children.keys() {
                    kill(pid, SIGTERM).ok();
                }
                continue;
            }
            SIGCHLD => {
                for (pid, status) in reap_zombies() {
                    match children.remove(&pid) {
                        Some(child) => {
                            info!("Process {:?} {}", child.get_name(), status);
                            clean_child(child.get_name(), &master);
                        }
                        None => {
                            info!("Unknown process {:?} {}", pid, status);
                        }
                    }
                }
                // In case we will wait for some process for the long time
                // we want to close tcp ports as fast as possible, so that
                // our upstream/monitoring notice the socket is closed
                close_unused_sockets(sockets, children);
                if children.len() == 0 {
                    return;
                }
            }
            _ => unreachable!(),
        }
    }
}

fn read_sandboxes(master: &MasterConfig, bin: &Binaries,
    master_file: &Path, options: &Options)
    -> HashMap<String, Process>
{
    let dirpath = master_file.parent().unwrap().join(&master.sandboxes_dir);
    info!("Reading sandboxes from {:?}", dirpath);
    let sandbox_validator = SandboxConfig::validator();
    scan_dir::ScanDir::files().read(&dirpath, |iter| {
        let yamls = iter.filter(|&(_, ref name)| name.ends_with(".yaml"));
        yamls.filter_map(|(entry, name)| {
            let sandbox_config = entry.path();
            let sandbox_name = name[..name.len()-5].to_string();
            debug!("Reading config: {:?}", sandbox_config);
            parse_config(&sandbox_config, &sandbox_validator, Default::default())
                .map_err(|e| error!("Can't read config {:?}: {}",
                                    sandbox_config, e))
                .map(|cfg: SandboxConfig| (sandbox_name, cfg))
                .ok()
        }).flat_map(|(name, sandbox)| {
            read_subtree(master, bin, master_file, &name, &sandbox, options)
            .into_iter()
        }).collect()
    })
    .map_err(|e| error!("Error reading sandboxes directory: {}", e))
    .unwrap_or(HashMap::new())
}

fn read_subtree<'x>(master: &MasterConfig,
    bin: &Binaries, master_file: &Path,
    sandbox_name: &String, sandbox: &SandboxConfig,
    options: &Options)
    -> Vec<(String, Process)>
{
    let now = SteadyTime::now();
    let cfg = master_file.parent().unwrap()
        .join(&master.processes_dir)
        .join(sandbox.config_file.as_ref().map(Path::new)
            .unwrap_or(Path::new(&(sandbox_name.clone() + ".yaml"))));
    debug!("Reading child config {:?}", cfg);
    parse_config(&cfg, &ChildConfig::mapping_validator(), Default::default())
        .map(|cfg: BTreeMap<String, ChildConfig>| {
            OpenOptions::new().create(true).write(true).append(true)
            .open(master.config_log_dir.join(sandbox_name.clone() + ".log"))
            .and_then(|mut f| write!(&mut f, "{} {}\n",
                time::now_utc().rfc3339(),
                json::as_json(&cfg)))
            .map_err(|e| error!("Error writing config log: {}", e))
            .ok();
            cfg
        })
        .map_err(|e| warn!("Can't read config {:?}: {}",
                            sandbox.config_file, e))
        .unwrap_or(BTreeMap::new())
        .into_iter()
        .filter(|&(_, ref child)| child.kind == Daemon)
        .flat_map(|(child_name, mut child)| {
            let instances = child.instances;

            //  Child doesn't need to know how many instances it's run
            //  And for comparison on restart we need to have "one" always
            child.instances = 1;
            let image_dir = sandbox.image_dir.join(&child.image);
            let cfg_res = temporary_change_root(&image_dir, || {
                parse_config(&child.config,
                    &ContainerConfig::validator(), Default::default())
                .map_err(|e| format!("Error reading {:?} \
                    of sandbox {:?} of image {:?}: {}",
                    &child.config, sandbox_name, child.image,  e))
            });
            let cfg: Rc<ContainerConfig> = match cfg_res {
                Ok(cfg) => Rc::new(cfg),
                Err(e) => {
                    error!("{}", e);
                    return Vec::new().into_iter();
                }
            };
            let child_string = Rc::new(json::encode(&child).unwrap());

            let items: Vec<(String, Process)> = (0..instances)
                .map(|i| {
                    let name = format!("{}/{}.{}", sandbox_name, child_name, i);
                    let cmd = new_child(bin, &name, master_file,
                        &child_string, options);
                    let restart_min = now + Duration::milliseconds(
                        (cfg.restart_timeout * 1000.) as i64);
                    let process = Process {
                        cmd: cmd,
                        name: name.clone(),
                        restart_min: restart_min,
                        config: child_string.clone(), // should avoid cloning?
                        inner_config: cfg.clone(),
                        ports: cfg.tcp_ports.keys().cloned().collect(),
                    };
                    (name, process)
                })
                .collect();
            items.into_iter()
        }).collect()
}

fn schedule_new_workers(configs: HashMap<String, Process>)
    -> Queue<Process>
{
    let mut result = Queue::new();
    for (_, item) in configs.into_iter() {
        result.add(SteadyTime::now(), item);
    }
    return result;
}

struct Binaries {
    lithos_tree: PathBuf,
    lithos_knot: PathBuf,
}

fn get_binaries() -> Option<Binaries> {
    let dir = match env::current_exe().ok()
        .and_then(|x| x.parent().map(|y| y.to_path_buf()))
    {
        Some(dir) => dir,
        None => return None,
    };
    let bin = Binaries {
        lithos_tree: dir.join("lithos_tree"),
        lithos_knot: dir.join("lithos_knot"),
    };
    if !metadata(&bin.lithos_tree).map(|x| x.is_file()).unwrap_or(false) {
        write!(&mut stderr(), "Can't find lithos_tree binary").unwrap();
        return None;
    }
    if !metadata(&bin.lithos_knot).map(|x| x.is_file()).unwrap_or(false) {
        write!(&mut stderr(), "Can't find lithos_knot binary").unwrap();
        return None;
    }
    return Some(bin);
}

fn main() {
    exec_handler::set_handler(&ABNORMAL_TERM_SIGNALS, true)
        .ok().expect("Can't set singal handler");

    let options = match Options::parse_args() {
        Ok(options) => options,
        Err(x) => {
            exit(x);
        }
    };
    match run(&options.config_file, &options) {
        Ok(()) => {
            exit(0);
        }
        Err(e) => {
            write!(&mut stderr(), "Fatal error: {}\n", e).ok();
            error!("Fatal error: {}", e);
            exit(1);
        }
    }
}
