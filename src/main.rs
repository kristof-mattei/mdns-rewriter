mod cli;
mod reflector;
mod signal_handlers;
use std::ffi::CString;
use std::fs::{remove_file, File};
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::process::exit;
use std::time::Duration;
use std::{env, sync::Arc};

use cli::{parse_cli, Config};
use color_eyre::{eyre, Section};
use reflector::reflect;
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{event, Level};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
mod unix;
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddrV4};

use libc::{
    chdir, fork, getpid, ifreq, ioctl, kill, pid_t, setsid, signal, sockaddr_in, umask, IFNAMSIZ,
    IP_PKTINFO, SIGCHLD, SIGHUP, SIG_IGN, SIOCGIFADDR, SIOCGIFNETMASK, SOL_IP,
};
use socket2::{Domain, Type};

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

fn create_recv_sock() -> Result<UdpSocket, eyre::Error> {
    let socket =
        socket2::Socket::new(Domain::IPV4, Type::DGRAM, None /* IPPROTO_IP */).map_err(|err| {
            event!(Level::ERROR, ?err, "recv socket()");

            err
        })?;

    socket.set_nonblocking(true).map_err(|err| {
        event!(Level::ERROR, ?err, "recv setsockopt(SO_NONBLOCK)");

        err
    })?;

    socket.set_reuse_address(true).map_err(|err| {
        event!(Level::ERROR, ?err, "recv setsockopt(SO_REUSEADDR)");

        err
    })?;

    // bind to an address
    let server_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MDNS_PORT);
    socket.bind(&server_addr.into()).map_err(|err| {
        event!(Level::ERROR, ?err, "recv bind()");

        err
    })?;

    // enable loopback in case someone else needs the data
    socket.set_multicast_loop_v4(true).map_err(|err| {
        event!(Level::ERROR, ?err, "recv setsockopt(IP_MULTICAST_LOOP)");

        err
    })?;

    // #ifdef IP_PKTINFO
    // do we support any OS that doesn't have `IP_PKTINFO?`
    unsafe {
        unix::setsockopt(&socket, SOL_IP, IP_PKTINFO, true).map_err(|err| {
            event!(Level::ERROR, ?err, "recv setsockopt(IP_PKTINFO)");

            err
        })?;
    }
    // #endif

    Ok(UdpSocket::from_std(socket.into())?)
}

#[derive(Debug)]
struct InterfaceSocket {
    /// interface name
    name: String,
    /// socket
    socket: UdpSocket,
    /// interface address
    address: Ipv4Addr,
    /// interface mask
    mask: Ipv4Addr,
    /// interface network (computed)
    network: Ipv4Addr,
}

fn create_send_sock(
    recv_sockfd: &UdpSocket,
    ifname: String,
) -> Result<InterfaceSocket, eyre::Error> {
    let socket = socket2::Socket::new(
        Domain::IPV4,
        Type::DGRAM,
        // IPPROTO_IP
        None,
    )
    .map_err(|err| {
        event!(Level::ERROR, ?err, ifname, "send socket()");

        err
    })?;

    let mut ifr = unsafe { MaybeUninit::<ifreq>::zeroed().assume_init() };

    let c_ifname = CString::new(ifname.as_str())
        .map_err(|err| eyre::Error::new(err).with_note(|| "Failed to convert ifname to CString"))?;

    let len = std::cmp::min(
        c_ifname.as_bytes_with_nul().len(),
        IFNAMSIZ - 1, // leave one to ensure there's a null terminator
    );

    unsafe {
        std::ptr::copy_nonoverlapping(c_ifname.as_ptr(), ifr.ifr_name.as_mut_ptr(), len);
    };

    let s = (&raw mut ifr.ifr_ifru).cast::<sockaddr_in>();

    let if_addr = &mut (unsafe { &mut *s }).sin_addr;

    // get netmask
    let interface_mask = unsafe {
        if 0 == ioctl(socket.as_raw_fd(), SIOCGIFNETMASK, &ifr) {
            let mask_in_network_order = if_addr.s_addr;

            Ipv4Addr::from(u32::from_be(mask_in_network_order))
        } else {
            Ipv4Addr::UNSPECIFIED
        }
    };

    // .. and interface address
    let interface_address = unsafe {
        if 0 == ioctl(socket.as_raw_fd(), SIOCGIFADDR, &ifr) {
            let addr_in_network_order = if_addr.s_addr;

            Ipv4Addr::from(u32::from_be(addr_in_network_order))
        } else {
            Ipv4Addr::UNSPECIFIED
        }
    };

    // compute network (address & mask)
    let interface_network = interface_mask & interface_address;

    socket.set_nonblocking(true).map_err(|err| {
        event!(Level::ERROR, ?err, ifname, "send setsockopt(SO_NONBLOCK)");

        err
    })?;

    socket.set_reuse_address(true).map_err(|err| {
        event!(Level::ERROR, ?err, ifname, "send setsockopt(SO_REUSEADDR)");

        err
    })?;

    // #ifdef SO_BINDTODEVICE
    // do we support any OS that doesn't have `SO_BINDTODEVICE?`
    socket.bind_device(Some(ifname.as_bytes())).map_err(|err| {
        event!(
            Level::ERROR,
            ?err,
            ifname,
            "send setsockopt(SO_BINDTODEVICE)"
        );

        err
    })?;
    // #endif

    // bind to an address
    let server_addr = SocketAddrV4::new(interface_address, MDNS_PORT);

    socket.bind(&server_addr.into()).map_err(|err| {
        event!(Level::ERROR, ?err, ifname, "send bind()");

        err
    })?;

    // add membership to receiving socket
    recv_sockfd
        .join_multicast_v4(MDNS_ADDR, interface_address)
        .map_err(|err| {
            event!(
                Level::ERROR,
                ?err,
                ifname,
                "recv setsockopt(IP_ADD_MEMBERSHIP)"
            );

            err
        })?;

    // enable loopback in case someone else needs the data
    socket.set_multicast_loop_v4(true).map_err(|err| {
        event!(
            Level::ERROR,
            ?err,
            ifname,
            "send setsockopt(IP_MULTICAST_LOOP)"
        );

        err
    })?;

    let interface_socket = InterfaceSocket {
        name: ifname,
        socket: UdpSocket::from_std(socket.into())?,
        address: interface_address,
        mask: interface_mask,
        network: interface_network,
    };

    println!(
        "dev {} addr {} mask {} net {}",
        interface_socket.name,
        interface_socket.address,
        interface_socket.mask,
        interface_socket.network
    );

    Ok(interface_socket)
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
