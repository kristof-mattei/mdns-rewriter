use std::ffi::CString;
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::AsRawFd;

use color_eyre::{eyre, Section};
use libc::{ifreq, ioctl, sockaddr_in, IFNAMSIZ, IP_PKTINFO, SIOCGIFADDR, SIOCGIFNETMASK, SOL_IP};
use socket2::{Domain, Type};
use tokio::net::UdpSocket;
use tracing::{event, Level};

use crate::{unix, MDNS_ADDR, MDNS_PORT};

#[derive(Debug)]
pub(crate) struct InterfaceSocket {
    /// interface name
    pub(crate) name: String,
    /// socket
    pub(crate) socket: UdpSocket,
    /// interface address
    pub(crate) address: Ipv4Addr,
    /// interface mask
    pub(crate) mask: Ipv4Addr,
    /// interface network (computed)
    pub(crate) network: Ipv4Addr,
}

pub(crate) fn create_recv_sock() -> Result<UdpSocket, eyre::Error> {
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

pub(crate) fn create_send_sock(
    recv_sockfd: &UdpSocket,
    ifname: String,
) -> Result<InterfaceSocket, eyre::Error> {
    let (interface_address, interface_mask) = get_interface_details(&ifname)?;

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

    // compute network (address & mask)
    let interface_network = interface_address & interface_mask;

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

fn get_interface_details(ifname: &str) -> Result<(Ipv4Addr, Ipv4Addr), eyre::Report> {
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

    let c_ifname = CString::new(ifname)
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

    #[cfg(target_env = "musl")]
    let siocgifnetmask: i32 = SIOCGIFNETMASK.try_into().unwrap();
    #[cfg(not(target_env = "musl"))]
    let siocgifnetmask = SIOCGIFNETMASK;

    // get netmask
    let interface_mask = unsafe {
        if 0 == ioctl(socket.as_raw_fd(), siocgifnetmask, &ifr) {
            let mask_in_network_order = if_addr.s_addr;

            Ipv4Addr::from(u32::from_be(mask_in_network_order))
        } else {
            Ipv4Addr::UNSPECIFIED
        }
    };

    #[cfg(target_env = "musl")]
    let siocgifaddr: i32 = SIOCGIFADDR.try_into().unwrap();
    #[cfg(not(target_env = "musl"))]
    let siocgifaddr = SIOCGIFADDR;

    // .. and interface address
    let interface_address = unsafe {
        if 0 == ioctl(socket.as_raw_fd(), siocgifaddr, &ifr) {
            let addr_in_network_order = if_addr.s_addr;

            Ipv4Addr::from(u32::from_be(addr_in_network_order))
        } else {
            Ipv4Addr::UNSPECIFIED
        }
    };

    Ok((interface_address, interface_mask))
}
