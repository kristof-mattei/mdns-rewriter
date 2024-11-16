mod cli;
mod reflector;
mod signal_handlers;
mod sockets;
mod unix;

use std::env;
use std::fs::{remove_file, File};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;

use cli::{parse_cli, Config};
use color_eyre::eyre;
use libc::{chdir, fork, getpid, kill, pid_t, setsid, signal, umask, SIGCHLD, SIGHUP, SIG_IGN};
use reflector::reflect;
use sockets::{create_recv_sock, create_send_sock};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{event, Level};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

// TODO this should come from cargo
const PACKAGE: &str = env!("CARGO_PKG_NAME");
const PACKET_SIZE: usize = 65536;
const MDNS_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

const BROADCAST_MDNS: SocketAddr = SocketAddr::V4(SocketAddrV4::new(MDNS_ADDR, MDNS_PORT));

fn main() -> Result<(), eyre::Report> {
    color_eyre::config::HookBuilder::default()
        .capture_span_trace_by_default(false)
        .install()?;

    let rust_log_value = env::var(EnvFilter::DEFAULT_ENV)
        .unwrap_or_else(|_| format!("INFO,{}=TRACE", env!("CARGO_PKG_NAME").replace('-', "_")));

    // set up logger
    // from_env defaults to RUST_LOG
    tracing_subscriber::registry()
        .with(EnvFilter::builder().parse(rust_log_value).unwrap())
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_error::ErrorLayer::default())
        .init();

    // initialize the runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(start_tasks())
}

async fn start_tasks() -> Result<(), eyre::Error> {
    let (config, interfaces) = parse_cli().inspect_err(|error| {
        // this prints the error in color and exits
        // can't do anything else until
        // https://github.com/clap-rs/clap/issues/2914
        // is merged in
        if let Some(clap_error) = error.downcast_ref::<clap::error::Error>() {
            clap_error.exit();
        }
    })?;

    let config = Arc::new(config);

    // unsure what to do here for now
    // openlog(PACKAGE, LOG_PID | LOG_CONS, LOG_DAEMON);

    let cancellation_token = CancellationToken::new();

    if config.foreground {
        // check for pid file when running in foreground
        let running_pid = already_running(&config);

        if let Some(running_pid) = running_pid {
            event!(Level::ERROR, "already running as pid {}", running_pid);
            return Ok(());
        }
    } else {
        daemonize(&config, &cancellation_token);
    }

    // create receiving socket
    let server_socket = create_recv_sock().map_err(|err| {
        event!(Level::ERROR, ?err, "unable to create server socket");

        err
    })?;

    let mut sockets = Vec::with_capacity(interfaces.len());

    // create sending sockets
    for interface in interfaces {
        let send_socket = create_send_sock(&server_socket, interface).map_err(|err| {
            event!(Level::ERROR, ?err, "unable to create socket for interface");

            err
        })?;

        sockets.push(send_socket);
    }

    let task_tracker = TaskTracker::new();

    {
        let cancellation_token = cancellation_token.clone();

        task_tracker.spawn(reflect(
            server_socket,
            sockets,
            config.clone(),
            cancellation_token,
        ));
    }

    // now we wait forever for either
    // * SIGTERM
    // * ctrl + c (SIGINT)
    // * a message on the shutdown channel, sent either by the server task or
    // another task when they complete (which means they failed)
    tokio::select! {
        _ = signal_handlers::wait_for_sigint() => {
            // we completed because ...
            event!(Level::WARN, message = "CTRL+C detected, stopping all tasks");
        },
        _ = signal_handlers::wait_for_sigterm() => {
            // we completed because ...
            event!(Level::WARN, message = "Sigterm detected, stopping all tasks");
        },
        () = cancellation_token.cancelled() => {
            event!(Level::WARN, "Underlying task stopped, stopping all others tasks");
        },
    };

    cancellation_token.cancel();

    task_tracker.close();

    // wait for the task that holds the server to exit gracefully
    // it listens to shutdown_send
    if timeout(Duration::from_millis(10000), task_tracker.wait())
        .await
        .is_err()
    {
        event!(Level::ERROR, "Tasks didn't stop within allotted time!");
    }

    // remove pid file if it belongs to us
    if already_running(&config).is_some_and(|pid| pid == unsafe { getpid() }) {
        if let Err(err) = remove_file(&config.pid_file) {
            event!(
                Level::ERROR,
                ?err,
                pid_file = ?config.pid_file,
                "Failed to remove pid_file, manual deletion required",
            );
        }
    }

    event!(Level::INFO, "Goodbye");

    Ok(())
}

fn daemonize(config: &Config, _cancellation_token: &CancellationToken) {
    // pid_t running_pid;
    let pid: pid_t = unsafe { fork() };

    if pid < 0 {
        let err = std::io::Error::last_os_error();
        event!(Level::ERROR, ?err, "fork()");

        exit(1);
    }

    // exit parent process
    if pid > 0 {
        exit(0);
    }

    // let closure = |signal| {
    //     mdns_rewriter_shutdown(signal, &cancellation_token);
    // };

    // signals
    unsafe {
        signal(SIGCHLD, SIG_IGN);
        signal(SIGHUP, SIG_IGN);
        // signal(SIGTERM, closure as usize);

        setsid();
        umask(0o0027);
        chdir(c"/".as_ptr());
    }

    // close all std fd and reopen /dev/null for them
    // int i;
    // for (i = 0; i < 3; i++)
    // {
    //     close(i);
    //     if (open("/dev/null", O_RDWR) != i)
    //     {
    //         log_message(LOG_ERR, "unable to open /dev/null for fd %d", i);
    //         exit(1);
    //     }
    // }

    // check for pid file
    let running_pid = already_running(config);
    if let Some(running_pid) = running_pid {
        event!(Level::ERROR, "already running as pid {}", running_pid);
        exit(1);
    } else if let Err(err) = write_pidfile(config) {
        event!(
            Level::ERROR,
            ?err,
            "unable to write pid file {:?}",
            config.pid_file
        );
        exit(1);
    }
}

fn already_running(config: &Config) -> Option<i32> {
    let file = File::open(&config.pid_file).ok()?;

    let mut reader = BufReader::new(file);

    let mut line = String::with_capacity(5);
    let _r = reader.read_line(&mut line);

    let pid = line.parse::<i32>().ok()?;

    if 0 == unsafe { kill(pid, 0) } {
        return Some(pid);
    }

    None
}

#[expect(unused)]
fn mdns_rewriter_shutdown(_signal: i32, cancellation_token: &CancellationToken) {
    cancellation_token.cancel();
}

fn write_pidfile(config: &Config) -> Result<(), eyre::Report> {
    let mut file = File::create(&config.pid_file)?;

    let pid = unsafe { getpid() };

    file.write_fmt(format_args!("{}", pid))?;

    Ok(())
}
