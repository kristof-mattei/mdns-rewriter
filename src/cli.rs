use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

use clap::{command, Arg, ArgAction, Command};
use color_eyre::eyre;
use const_format::concatcp;

use crate::PACKAGE;
const PID_FILE: &str = concatcp!("/var/run/", PACKAGE, ".pid");

fn parse_into_pathbuf(parameter: &str) -> Result<PathBuf, &'static str> {
    let pathbuf = PathBuf::from(parameter);

    if pathbuf.is_absolute() {
        Ok(pathbuf)
    } else {
        Err("pid file path must be absolute")
    }
}

fn build_clap_matcher() -> Command {
    command!()
        .help_template(
            "\
{before-help}{name} {version}
{author-with-newline}{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}
",
        )
        .disable_help_flag(true)
        .disable_version_flag(true)
        .color(clap::ColorChoice::Always)
        .arg(
            Arg::new("foreground")
                .short('f')
                .help("Runs the application in the foreground and logs to stdout")
                .group("daemonize")
                .display_order(0)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .help("Turns on verbose mode. Trace messages (very verbose!)")
                .display_order(1)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("pid_file")
                .short('p')
                .help("Specifies the PID file path")
                .group("daemonize")
                .display_order(2)
                .action(ArgAction::Set)
                .value_parser(parse_into_pathbuf)
                .default_value(PID_FILE),
        )
        .arg(
            Arg::new("interfaces")
                .help("interfaces")
                .long_help("A minimum of 2 interfaces must be specified")
                .display_order(3)
                .action(ArgAction::Set)
                .num_args(2..)
                .required(true)
                .value_name("interface")
                .trailing_var_arg(true),
        )
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .help("Print this help message and exit")
                .display_order(4)
                .action(ArgAction::Help),
        )
}

pub(crate) struct Config {
    pub(crate) foreground: bool,

    pub(crate) verbose: bool,

    // if (optarg[0] != '/')
    // 	log_message(LOG_ERR, "pid file path must be absolute");
    // else
    // 	pid_file = optarg;
    // break;
    pub(crate) pid_file: PathBuf,
}

pub(crate) fn parse_cli() -> Result<(Config, Vec<String>), eyre::Error> {
    parse_cli_from(env::args_os())
}

fn parse_cli_from<I, T>(from: I) -> Result<(Config, Vec<String>), eyre::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = build_clap_matcher().try_get_matches_from(from)?;

    let foreground = *matches.get_one("foreground").unwrap_or(&false);
    let verbose = *matches.get_one("verbose").unwrap_or(&false);
    let pid_file: PathBuf = matches
        .get_one("pid_file")
        .cloned()
        .unwrap_or_else(|| PathBuf::from(PID_FILE));

    let interfaces = matches
        .get_many::<String>("interfaces")
        .unwrap()
        .cloned()
        .collect();

    Ok((
        Config {
            foreground,
            verbose,
            pid_file,
        },
        interfaces,
    ))
}
