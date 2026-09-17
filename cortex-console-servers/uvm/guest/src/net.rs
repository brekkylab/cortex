//! Bringing up the interface, when the boot said there is one.
//!
//! Every step here is an `ioctl` on a socket, for the reason every mount in
//! [`init`](crate::init) is the syscall: the base image is whatever the caller chose, and it
//! may ship no way to do this. `python:3.13-slim` has neither `ip` nor `ifconfig`, and an
//! `alpine` that has busybox's is a coincidence rather than a contract.
//!
//! ```text
//! SIOCSIFADDR     the address the host assigned
//! SIOCSIFNETMASK  the prefix it came with
//! SIOCSIFFLAGS    IFF_UP, which is what makes the other two take effect
//! SIOCADDRT       a default route through the gateway
//! ```
//!
//! # No DHCP
//!
//! There is nothing to discover. The other end of this device is a userspace stack in the
//! process that started this guest, and it assigned every address before the kernel came up —
//! they arrive in [`NET_ENV`] and [`NET_IPV4_ENV`]. A lease negotiation would be this guest
//! asking a question it was already told the answer to, with a client to write and a timeout to
//! get wrong.
//!
//! # The resolver
//!
//! Named by the stack, and answered by it: `/etc/resolv.conf` is one line and needs no upstream
//! from anywhere. Which is what makes this work on a base image that ships no `resolv.conf` at
//! all — most of them, since a container runtime normally writes it.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::time::{Duration, Instant};

use crate::contract::{CA_BUNDLE_PATH, NET_ENV, NET_IPV4_ENV, RESOLV_CONF};

/// The system trust bundles a base image is built with, newest-common first. The interception
/// CA is appended to whichever exists, so an intercepted host and a bypassed one both verify.
const SYSTEM_CA_BUNDLES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt", // Debian, Ubuntu, Alpine
    "/etc/pki/tls/certs/ca-bundle.crt",   // RHEL, Fedora, CentOS
    "/etc/ssl/ca-bundle.pem",             // openSUSE
    "/etc/ssl/cert.pem",                  // Alpine (libressl), BSD-derived
];

/// How long to wait for the interface to be probed.
///
/// The device is enumerated while this binary is mounting, so it is usually there before
/// anything asks. The wait is for the boot where it is not — the same race the console port
/// has, and the same answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_POLL: Duration = Duration::from_millis(20);

/// What the stack on the other side of the device said this guest's network is.
#[derive(Debug, PartialEq, Eq)]
struct Network {
    addr: [u8; 4],
    prefix: u8,
    gateway: [u8; 4],
    /// The resolver. The gateway in every configuration this has seen, and read separately
    /// anyway — the stack sends it as its own field, and a guest that assumed the two were the
    /// same would be deciding something it was told.
    dns: [u8; 4],
    /// The interface's hardware address, as `/sys` spells it — which is how the interface is
    /// found. Kept as the string it arrived as rather than parsed to bytes, because the only
    /// thing done with it is a comparison against a file's contents.
    mac: String,
}

/// Configure the interface the boot attached, or do nothing if it attached none.
///
/// A session with no network is the default and not a failure, so an absent [`NET_ENV`] is
/// silence. Everything after that point is a failure worth reporting: the boot said there is a
/// device, and a guest that cannot bring it up is one whose commands will fail at a name
/// resolution instead, several layers from the cause.
pub fn configure() -> anyhow::Result<()> {
    let (Ok(interface), Ok(addresses)) = (std::env::var(NET_ENV), std::env::var(NET_IPV4_ENV))
    else {
        return Ok(());
    };

    let network = parse(&interface, &addresses)?;
    let interface = probe(&network.mac)?;
    apply(&interface, &network)?;
    resolver(&network)?;
    Ok(())
}

/// Trust the boot's TLS interception CA, given the PEM the boot left in the boot root.
///
/// The stack terminates every 443 connection and presents a certificate it signed, so a guest
/// that did not trust the signer would reject every HTTPS host. The CA is *appended* to the
/// image's own roots rather than replacing them — a bundle of only the intercept CA would break
/// the hosts the stack lets through untouched. The commands read it because
/// [`environment`](crate::agent) points the usual variables at [`CA_BUNDLE_PATH`].
///
/// `ca` is read before the pivot, by [`init`](crate::init), because the boot root it lives in is
/// gone by the time this writes to the new one. Not called at all for the ordinary session,
/// whose network intercepts nothing.
pub fn install_ca(ca: &[u8]) -> anyhow::Result<()> {
    // The image's roots first, so a bypassed host still verifies against them. An image that
    // ships none is a base with no HTTPS of its own; the CA alone is then the whole bundle.
    let mut bundle = SYSTEM_CA_BUNDLES
        .iter()
        .find_map(|path| std::fs::read(path).ok())
        .unwrap_or_default();
    if bundle.last().is_some_and(|byte| *byte != b'\n') {
        bundle.push(b'\n');
    }
    bundle.extend_from_slice(ca);

    if let Some(parent) = std::path::Path::new(CA_BUNDLE_PATH).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(CA_BUNDLE_PATH, bundle)
        .map_err(|e| anyhow::anyhow!("writing the trust bundle {CA_BUNDLE_PATH}: {e}"))
}

/// The two variables the stack sends, each a comma-separated list of `key=value`.
///
/// Read by name rather than by position, because that is how they are written and because the
/// stack sends fields this end has no use for — `mtu=` today, and whatever it adds next.
/// Anything unrecognised is skipped; anything *needed* and missing is an error, since an
/// interface configured from half a description is a guest that is up and unreachable.
fn parse(interface: &str, addresses: &str) -> anyhow::Result<Network> {
    let mac = field(interface, "mac")
        .ok_or_else(|| anyhow::anyhow!("{NET_ENV}={interface} names no `mac`"))?;

    let addr = field(addresses, "addr")
        .ok_or_else(|| anyhow::anyhow!("{NET_IPV4_ENV}={addresses} names no `addr`"))?;
    let (addr, prefix) = addr
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("{NET_IPV4_ENV}: {addr} has no prefix"))?;

    let gateway = field(addresses, "gw")
        .ok_or_else(|| anyhow::anyhow!("{NET_IPV4_ENV}={addresses} names no `gw`"))?;
    // The resolver, and the gateway when it was not said separately — which is what every
    // configuration of this stack does today, and a fallback rather than an assumption.
    let dns = field(addresses, "dns").unwrap_or(gateway);

    Ok(Network {
        addr: octets(addr)?,
        prefix: prefix
            .parse()
            .ok()
            .filter(|p| *p <= 32)
            .ok_or_else(|| anyhow::anyhow!("{NET_IPV4_ENV}: {prefix} is not a prefix length"))?,
        gateway: octets(gateway)?,
        dns: octets(dns)?,
        mac: mac.to_owned(),
    })
}

/// One `key=value` out of a comma-separated list, or `None` when it is not in there.
fn field<'a>(list: &'a str, key: &str) -> Option<&'a str> {
    list.split(',')
        .filter_map(|field| field.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
        .filter(|value| !value.is_empty())
}

fn octets(addr: &str) -> anyhow::Result<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut parts = addr.split('.');
    for octet in &mut octets {
        *octet = parts
            .next()
            .and_then(|part| part.parse().ok())
            .ok_or_else(|| anyhow::anyhow!("{addr} is not an IPv4 address"))?;
    }
    anyhow::ensure!(parts.next().is_none(), "{addr} is not an IPv4 address");
    Ok(octets)
}

/// The name of the interface the boot attached: the one whose hardware address is the one the
/// host assigned.
///
/// Not `eth0` — the name a device gets is the kernel's to choose. And not "the one that is not
/// `lo`" either, which is the version this had first and the version that was wrong: the kernel
/// a guest boots also carries a `dummy0`, so that rule names two interfaces and configures
/// whichever the directory happens to list first.
fn probe(mac: &str) -> anyhow::Result<String> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        if let Some(name) = candidate(mac)? {
            return Ok(name);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "no interface with the address {mac} appeared in {PROBE_TIMEOUT:?} — the boot said \
             there is a network, so the device is missing rather than absent by choice"
        );
        std::thread::sleep(PROBE_POLL);
    }
}

fn candidate(mac: &str) -> anyhow::Result<Option<String>> {
    // `/sys` is mounted by `init::mount_pseudo` before this runs. Its absence would be that
    // ordering broken, not an interface missing, so it is an error rather than a retry.
    let entries = std::fs::read_dir("/sys/class/net")
        .map_err(|e| anyhow::anyhow!("reading /sys/class/net: {e}"))?;

    for entry in entries.flatten() {
        // Unreadable rather than absent for anything without one, which is what `lo` and the
        // kernel's own virtual devices look like from here.
        let Ok(address) = std::fs::read_to_string(entry.path().join("address")) else {
            continue;
        };
        if address.trim().eq_ignore_ascii_case(mac) {
            return Ok(Some(entry.file_name().to_string_lossy().into_owned()));
        }
    }
    Ok(None)
}

/// Address, netmask, up, default route — in that order, because the flags are what commit the
/// two before them and a route cannot be added through an interface that is down.
fn apply(interface: &str, network: &Network) -> anyhow::Result<()> {
    let socket = inet_socket()?;
    let fd = socket.as_raw_fd();

    let mut request = ifreq(interface)?;

    request.ifr_ifru.ifru_addr = sockaddr(network.addr);
    ioctl(fd, libc::SIOCSIFADDR, &request, "setting the address")?;

    request.ifr_ifru.ifru_netmask = sockaddr(mask(network.prefix));
    ioctl(fd, libc::SIOCSIFNETMASK, &request, "setting the netmask")?;

    // Read the flags before writing them: the kernel keeps state in there that is none of
    // this function's business, and a blind write would clear it.
    ioctl(fd, libc::SIOCGIFFLAGS, &request, "reading the flags")?;
    // SAFETY: the union was last written by the ioctl above, which writes `ifru_flags`.
    let flags = unsafe { request.ifr_ifru.ifru_flags };
    request.ifr_ifru.ifru_flags = flags | libc::IFF_UP as libc::c_short;
    ioctl(
        fd,
        libc::SIOCSIFFLAGS,
        &request,
        "bringing the interface up",
    )?;

    route(fd, interface, network.gateway)?;
    Ok(())
}

/// A default route through the gateway: destination and mask both zero, which is what makes
/// it the route of last resort.
fn route(fd: libc::c_int, interface: &str, gateway: [u8; 4]) -> anyhow::Result<()> {
    // Held for the length of the call, because `rt_dev` is a borrow the kernel reads through.
    let device = CString::new(interface)?;

    // SAFETY: an all-zero `rtentry` is a valid one; every field it needs is written below.
    let mut route: libc::rtentry = unsafe { std::mem::zeroed() };
    route.rt_dst = sockaddr([0, 0, 0, 0]);
    route.rt_genmask = sockaddr([0, 0, 0, 0]);
    route.rt_gateway = sockaddr(gateway);
    route.rt_flags = (libc::RTF_UP | libc::RTF_GATEWAY) as libc::c_ushort;
    route.rt_dev = device.as_ptr() as *mut libc::c_char;

    ioctl(fd, libc::SIOCADDRT, &route, "adding the default route")
}

/// Name the gateway as the resolver.
///
/// The file is removed first rather than truncated. A base image may ship it as a symlink into
/// somewhere a runtime was expected to manage, and writing through that would either fail or
/// land somewhere nothing reads.
fn resolver(network: &Network) -> anyhow::Result<()> {
    let gateway = network.dns.map(|o| o.to_string()).join(".");

    if let Some(parent) = std::path::Path::new(RESOLV_CONF).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(RESOLV_CONF);

    std::fs::write(RESOLV_CONF, format!("nameserver {gateway}\n"))
        .map_err(|e| anyhow::anyhow!("writing {RESOLV_CONF}: {e}"))
}

/// A socket to carry the ioctls. Nothing is ever sent on it — the address family is how the
/// kernel knows which of its tables the request is about.
fn inet_socket() -> anyhow::Result<OwnedFd> {
    // SAFETY: a plain socket call; the descriptor it returns is owned by nothing else.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    anyhow::ensure!(
        fd >= 0,
        "opening a socket to configure the interface: {}",
        io::Error::last_os_error()
    );
    // SAFETY: `fd` was just returned by `socket` and is not held anywhere else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ifreq(interface: &str) -> anyhow::Result<libc::ifreq> {
    anyhow::ensure!(
        interface.len() < libc::IFNAMSIZ,
        "{interface} is too long to name an interface"
    );

    // SAFETY: an all-zero `ifreq` is a valid one, and the name is written below.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (slot, byte) in request.ifr_name.iter_mut().zip(interface.bytes()) {
        *slot = byte as libc::c_char;
    }
    Ok(request)
}

/// An IPv4 address in the shape every field above wants it: a `sockaddr_in` seen as the
/// generic `sockaddr` the structures declare.
fn sockaddr(addr: [u8; 4]) -> libc::sockaddr {
    // SAFETY: both are plain data of the same size, and every byte of the result is written.
    let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    // `s_addr` is network order, which is these octets in the order they are written.
    sin.sin_addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(addr),
    };

    // SAFETY: `sockaddr_in` and `sockaddr` are the same 16 bytes; this is the cast every
    // caller of these ioctls performs.
    unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin) }
}

/// The netmask a prefix length means.
fn mask(prefix: u8) -> [u8; 4] {
    match prefix {
        0 => [0, 0, 0, 0],
        // Shifting by 32 is undefined for a `u32`, which is why zero is answered above.
        prefix => (u32::MAX << (32 - prefix as u32)).to_be_bytes(),
    }
}

/// One ioctl, with the errno reported as what it was being asked to do.
fn ioctl<T>(fd: libc::c_int, request: u64, argument: &T, doing: &str) -> anyhow::Result<()> {
    // SAFETY: `request` is one of the SIOC* constants above and `argument` is the structure
    // that request is defined over, borrowed for the length of the call.
    let rc = unsafe { libc::ioctl(fd, request as _, argument as *const T) };
    anyhow::ensure!(rc == 0, "{doing}: {}", io::Error::last_os_error());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const IFACE: &str = "iface=eth0,mac=52:54:00:c0:a8:02,mtu=1500";
    const IPV4: &str = "addr=192.168.127.2/30,gw=192.168.127.1,dns=192.168.127.1";

    #[test]
    fn a_network_is_read_off_what_the_stack_sent() {
        assert_eq!(
            parse(IFACE, IPV4).expect("a network"),
            Network {
                addr: [192, 168, 127, 2],
                prefix: 30,
                gateway: [192, 168, 127, 1],
                dns: [192, 168, 127, 1],
                mac: "52:54:00:c0:a8:02".to_owned(),
            }
        );
    }

    /// Fields are read by name, so one this end does not use is one it does not trip over —
    /// which is what lets the stack add to these strings without this file changing.
    #[test]
    fn an_unknown_field_is_ignored() {
        let extra = format!("{IPV4},something=else");
        assert_eq!(parse(IFACE, &extra).expect("a network").prefix, 30);
    }

    /// And the resolver falls back to the gateway rather than to nothing, since a `dns=` is
    /// what the stack happens to send and not something the format promises.
    #[test]
    fn a_missing_resolver_is_the_gateway() {
        let network = parse(IFACE, "addr=10.0.0.2/30,gw=10.0.0.1").expect("a network");
        assert_eq!(network.dns, [10, 0, 0, 1]);
    }

    /// Every one of these would otherwise be applied as something else — a prefix of 33 as a
    /// shift past the end of a `u32`, a missing gateway as an address of zero — and an
    /// interface configured from a misread string is a guest that is up and unreachable.
    #[test]
    fn a_description_that_is_not_one_is_refused() {
        for (iface, ipv4) in [
            // Nothing to find the interface by.
            ("iface=eth0,mtu=1500", IPV4),
            ("mac=", IPV4),
            ("", IPV4),
            // No address, no prefix, or a prefix that is not one.
            (IFACE, "gw=192.168.127.1"),
            (IFACE, "addr=192.168.127.2,gw=192.168.127.1"),
            (IFACE, "addr=192.168.127.2/33,gw=192.168.127.1"),
            // No gateway, or addresses that are not addresses.
            (IFACE, "addr=192.168.127.2/30"),
            (IFACE, "addr=192.168.127/30,gw=192.168.127.1"),
            (IFACE, "addr=192.168.127.2.5/30,gw=192.168.127.1"),
            (IFACE, "addr=192.168.127.2/30,gw=not.an.address.at"),
        ] {
            assert!(
                parse(iface, ipv4).is_err(),
                "{iface:?} + {ipv4:?} was accepted"
            );
        }
    }

    #[test]
    fn a_prefix_is_the_mask_it_names() {
        assert_eq!(mask(0), [0, 0, 0, 0]);
        assert_eq!(mask(8), [255, 0, 0, 0]);
        assert_eq!(mask(24), [255, 255, 255, 0]);
        assert_eq!(mask(32), [255, 255, 255, 255]);
    }
}
