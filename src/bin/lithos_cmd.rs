extern crate rustc_serialize;
extern crate libc;
#[macro_use] extern crate log;
extern crate regex;

extern crate argparse;
extern crate quire;
extern crate unshare;
#[macro_use] extern crate lithos;


use std::env;
use std::str::FromStr;
use std::process::exit;
use std::path::{Path, PathBuf};
use std::io::{stderr, Write};
use std::default::Default;
use std::collections::BTreeMap;

use regex::Regex;
use quire::parse_config;
use argparse::{ArgumentParser, Parse, List, StoreTrue, StoreOption, Print};
use rustc_serialize::json;
use libc::funcs::posix88::unistd::getpid;
use unshare::{Command, Namespace};

use lithos::setup::{clean_child, init_logging};
use lithos::master_config::{MasterConfig, create_master_dirs};
use lithos::sandbox_config::SandboxConfig;
use lithos::container_config::ContainerKind::{Command};
use lithos::child_config::ChildConfig;


fn run(master_cfg: &Path, sandbox_name: String,
    command_name: String, args: Vec<String>,
    log_stderr: bool, log_level: Option<log::LogLevel>)
    -> Result<(), String>
{
    let master: MasterConfig = try!(parse_config(&master_cfg,
        &MasterConfig::validator(), Default::default())
        .map_err(|e| format!("Error reading master config: {}", e)));
    try!(create_master_dirs(&master));

    if !Regex::new(r"^[\w-]+$").unwrap().is_match(&sandbox_name) {
        return Err(format!("Wrong sandbox name: {}", sandbox_name));
    }
    if !Regex::new(r"^[\w-]+$").unwrap().is_match(&command_name) {
        return Err(format!("Wrong command name: {}", command_name));
    }

    let sandbox: SandboxConfig = try!(parse_config(
        &master_cfg.parent().unwrap()
         .join(&master.sandboxes_dir).join(sandbox_name.clone() + ".yaml"),
        &SandboxConfig::validator(), Default::default())
        .map_err(|e| format!("Error reading sandbox config: {}", e)));

    let log_file;
    if let Some(ref fname) = sandbox.log_file {
        log_file = master.default_log_dir.join(fname);
    } else {
        log_file = master.default_log_dir.join(format!("{}.log", sandbox_name));
    }
    try!(init_logging(&master, &log_file,
        &format!("{}-{}", master.syslog_app_name, sandbox_name),
        log_stderr,
        log_level
            .or(sandbox.log_level
                .and_then(|x| FromStr::from_str(&x).ok()))
            .or_else(|| FromStr::from_str(&master.log_level).ok())
            .unwrap_or(log::LogLevel::Warn)));

    let cfg = master_cfg.parent().unwrap()
        .join(&master.processes_dir)
        .join(sandbox.config_file.as_ref().unwrap_or(
            &PathBuf::from(&(sandbox_name.clone() + ".yaml"))));
    debug!("Children config {:?}", cfg);
    let sandbox_children: BTreeMap<String, ChildConfig>;
    sandbox_children = try!(parse_config(&cfg,
            &ChildConfig::mapping_validator(), Default::default())
        .map_err(|e| format!("Error reading children config: {}", e)));
    let child_cfg = try!(sandbox_children.get(&command_name)
        .ok_or(format!("Command {:?} not found", command_name)));



    if child_cfg.kind != Command {
        return Err(format!("The target container is: {:?}", child_cfg.kind));
    }


    let name = format!("{}/cmd.{}.{}", sandbox_name,
        command_name, unsafe { getpid() });

    let mut cmd = Command::new(env::current_exe().unwrap()
                     .parent().unwrap().join("lithos_knot"));

    // Name is first here, so it's easily visible in ps
    cmd.arg("--name");
    cmd.arg(&name);

    cmd.arg("--master");
    cmd.arg(master_cfg);
    cmd.arg("--config");
    cmd.arg(json::encode(&child_cfg).unwrap());
    cmd.env_clear();
    cmd.env("TERM", env::var("TERM").unwrap_or("dumb".to_string()));
    if let Ok(x) = env::var("RUST_LOG") {
        cmd.env("RUST_LOG", x);
    }
    if let Ok(x) = env::var("RUST_BACKTRACE") {
        cmd.env("RUST_BACKTRACE", x);
    }
    cmd.arg("--");
    cmd.args(&args);
    cmd.unshare([Namespace::Mount, Namespace::Uts,
                 Namespace::Ipc, Namespace::Pid].iter().cloned());

    info!("Running {:?}", cmd);

    match cmd.status() {
        Ok(x) => info!("Command {:?} {}", cmd, x),
        Err(e) => error!("Can't run {:?}: {}", cmd, e),
    }

    clean_child(&name, &master);

    return Ok(());
}

fn main() {
    let mut master_config = PathBuf::from("/etc/lithos/master.yaml");
    let mut command_name = "".to_string();
    let mut sandbox_name = "".to_string();
    let mut args = vec!();
    let mut log_stderr: bool = false;
    let mut log_level: Option<log::LogLevel> = None;
    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Runs single ad-hoc command");
        ap.refer(&mut master_config)
          .add_option(&["--master"], Parse,
            "Name of the master configuration file \
             (default /etc/lithos/master.yaml)")
          .metavar("FILE");
        ap.refer(&mut log_stderr)
          .add_option(&["--log-stderr"], StoreTrue,
            "Print debugging info to stderr");
        ap.refer(&mut log_level)
          .add_option(&["--log-level"], StoreOption,
            "Set log level (default info for now)");
        ap.refer(&mut sandbox_name)
          .add_argument("sandbox", Parse,
            "Name of the sandbox to run command for")
          .required();
        ap.refer(&mut command_name)
          .add_argument("name", Parse,
            "Name of the command to run")
          .required();
        ap.refer(&mut args)
          .add_argument("argument", List,
            "Arguments for the command");
        ap.add_option(&["--version"],
            Print(env!("CARGO_PKG_VERSION").to_string()),
            "Show version");
        ap.stop_on_first_argument(true);
        match ap.parse_args() {
            Ok(()) => {}
            Err(x) => {
                exit(x);
            }
        }
    }
    match run(&master_config, sandbox_name, command_name, args,
              log_stderr, log_level)
    {
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
