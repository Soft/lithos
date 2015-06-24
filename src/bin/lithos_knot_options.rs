use std::env;
use std::io::{Write};
use std::io::{stdout, stderr};
use std::path::{PathBuf};

use argparse::{ArgumentParser, Store, Parse, List};

use lithos::child_config::ChildConfig;
use lithos::container_config::ContainerKind::Daemon;


pub struct Options {
    pub master_config: PathBuf,
    pub config: ChildConfig,
    pub name: String,
    pub args: Vec<String>,
}

impl Options {
    pub fn parse_args() -> Result<Options, i32> {
        Options::parse_specific_args(env::args().collect(),
                                     &mut stdout(), &mut stderr())
    }
    pub fn parse_specific_args(args: Vec<String>,
        stdout: &mut Write, stderr: &mut Write)
        -> Result<Options, i32>
    {
        let mut options = Options {
            master_config: PathBuf::from("/etc/lithos.yaml"),
            config: ChildConfig {
                instances: 0,
                image: "".to_string(),
                config: "".to_string(),
                kind: Daemon,
            },
            name: "".to_string(),
            args: vec!(),
        };
        let parse_result = {
            let mut ap = ArgumentParser::new();
            ap.set_description("Runs tree of processes");
            ap.refer(&mut options.name)
              .add_option(&["--name"], Store,
                "The process name");
            ap.refer(&mut options.master_config)
              .add_option(&["--master"], Parse,
                "Name of the master configuration file \
                 (default /etc/lithos.yaml)")
              .metavar("FILE");
            ap.refer(&mut options.config)
              .add_option(&["--config"], Store,
                "JSON-serialized container configuration")
              .required()
              .metavar("JSON");
            ap.refer(&mut options.args)
              .add_argument("argument", List,
                "Additional arguments for the command");
            ap.stop_on_first_argument(true);
            ap.parse(args, stdout, stderr)
        };
        match parse_result {
            Ok(()) => Ok(options),
            Err(x) => Err(x),
        }
    }
}
