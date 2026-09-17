//! The network a session gets, and the fact that it is *this process*.
//!
//! A guest with a network has a virtio-net device whose far end is a userspace TCP/IP stack
//! running right here — `microsandbox-network`'s, which is smoltcp with a policy engine over
//! it. The guest opens a connection to somewhere; the stack terminates it and opens it on the
//! host's behalf, if the policy says so.
//!
//! ```text
//! guest eth0 ──virtio-net──► this process ──policy──► the host's network
//! ```
//!
//! # Why in here rather than a proxy alongside
//!
//! `msb_krun` also speaks the unix-socket wire a network proxy uses, so a session could have
//! `gvproxy` or `passt` on the other end of a socket instead. It does not, for two reasons.
//!
//! One is distribution: a console server is a single binary that finds a kernel and otherwise
//! needs nothing, and a helper on `PATH` is a second thing to install, version and fail on.
//! The other is the reason that will outlast it — **a connection this process opens is a
//! connection this process can refuse.** Every guest connection terminates here, which is the
//! only place a policy about what a sandbox may reach can be enforced at all.
//!
//! # Why the stack is in the boot process and not the server
//!
//! Because what `VmBuilder::net` takes is a backend *object*, not a socket. The stack and the
//! device are two halves of one thing, so they live in the process that owns the VM — which is
//! also the process that dies with it, and so cannot leave a stack behind.

use anyhow::Context as _;
use cortex_uvm_boot::{Inject, Network, SecretSpec};
use microsandbox_network::builder::NetworkBuilder;
use microsandbox_network::config::NetworkConfig;
use microsandbox_network::network::SmoltcpNetwork;
use microsandbox_network::policy::{
    Action, Destination, DestinationGroup, Direction, NetworkPolicy, PortRange, Protocol, Rule,
};
use msb_krun::backends::net::NetBackend;

/// A running stack, and everything whose lifetime is the VM's.
///
/// The runtime is in here because dropping it would stop the poll loop the stack is driven by,
/// and `Vm::enter` never returns — so what holds this holds it until the process is gone.
pub struct Stack {
    /// Declared first so the poll loop is shut down before the runtime under it goes.
    stack: SmoltcpNetwork,
    _runtime: tokio::runtime::Runtime,
}

/// Bring a stack up under the policy `reach` and `host_ports` describe.
///
/// Never called for [`Network::Disabled`], which attaches no device at all: a policy that
/// refuses everything and a guest with no interface are not the same thing, and the second is
/// the one worth having when nothing was asked for.
pub fn start(reach: Network, host_ports: &[u16], secrets: &[SecretSpec]) -> anyhow::Result<Stack> {
    debug_assert!(reach != Network::Disabled);

    let config = config(reach, host_ports, secrets)?;

    // Its own runtime rather than a handle from somewhere: this process has no other async
    // work, and the stack's poll loop wants threads that are not competing with a VM's vCPUs
    // for a place to run.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("a runtime for the network stack")?;

    // The slot decides which addresses the stack hands out, so several sandboxes on one host
    // do not share a subnet. One VM per boot process, so this one is always the first.
    let mut stack = SmoltcpNetwork::new(config, 0)
        .map_err(|e| anyhow::anyhow!("building the network stack: {e:?}"))?;
    stack.start(runtime.handle().clone());

    Ok(Stack {
        stack,
        _runtime: runtime,
    })
}

impl Stack {
    /// The MAC and the backend `VmBuilder::net` wants, which the stack assigned itself.
    ///
    /// Taken rather than borrowed: the device is handed to the VMM, and there is one of it.
    pub fn device(&mut self) -> ([u8; 6], Box<dyn NetBackend + Send>) {
        (self.stack.guest_mac(), self.stack.take_backend())
    }

    /// What the guest is told, as the stack spells it — `MSB_NET`, `MSB_NET_IPV4` and the rest.
    ///
    /// Passed on rather than translated. These are `microsandbox-network`'s own names for its
    /// own numbers, and the guest is the end that applies them: a boot that rewrote them into a
    /// spelling of its own would be a third party to an agreement between two.
    pub fn guest_env(&self) -> Vec<(String, String)> {
        self.stack.guest_env_vars()
    }

    /// The interception CA as PEM, or `None` when this stack is not intercepting TLS.
    ///
    /// The guest is handed this to trust — see [`CA_PATH`](cortex_uvm_boot::CA_PATH). The private
    /// key it goes with never leaves this process, which is what keeps the injected credentials
    /// in here rather than in the VM.
    pub fn ca_cert_pem(&self) -> Option<Vec<u8>> {
        self.stack.ca_cert_pem()
    }
}

/// The whole config a session's network gets: the reach policy, plus TLS interception and
/// credential injection for the session's `secrets`.
///
/// A secret is substituted into the request the stack opens on the guest's behalf, only when the
/// intercepted TLS identity is an allowed host — so the value leaves this machine with the request
/// and never enters the VM. Injection needs interception, so any secret turns it on for every 443
/// connection, which is why the guest is also handed the CA to trust. A `secrets` entry whose
/// value is absent from this process's environment is skipped, not an error.
fn config(
    reach: Network,
    host_ports: &[u16],
    secrets: &[SecretSpec],
) -> anyhow::Result<NetworkConfig> {
    // Policy stays the stack's own everywhere but here — the addresses, the MTU, the DNS
    // timeouts are its opinion and not this process's.
    let mut builder = NetworkBuilder::new().policy(policy(reach, host_ports));

    let mut injected = Vec::new();
    for spec in secrets {
        let value = match std::env::var(&spec.env_var) {
            Ok(value) if !value.is_empty() => value,
            _ => continue,
        };
        injected.push(spec.env_var.as_str());
        let placeholder = spec.placeholder();
        builder = builder.secret(move |mut secret| {
            secret = secret
                .env(&spec.env_var)
                .value(value)
                .placeholder(placeholder)
                // Substitute only over intercepted TLS whose SNI is an allowed host, never over
                // plain HTTP a guest could point anywhere.
                .require_tls_identity(true)
                .inject_headers(matches!(spec.inject, Inject::Header))
                .inject_query(matches!(spec.inject, Inject::Query))
                .inject_basic_auth(matches!(spec.inject, Inject::BasicAuth))
                .inject_body(false);
            for host in &spec.hosts {
                secret = secret.allow_host(host);
            }
            secret
        });
    }

    if !injected.is_empty() {
        // `verify_upstream` keeps the real server's certificate checked against the host's roots;
        // `block_quic` forces an HTTP/3 client back onto the TCP/TLS path interception can see.
        builder = builder.tls(|tls| tls.enabled(true).verify_upstream(true).block_quic(true));
        eprintln!(
            "cortex-uvm-boot: injecting credentials for {} outside the guest",
            injected.join(", ")
        );
    }

    builder
        .build()
        .map_err(|e| anyhow::anyhow!("building the network config: {e:?}"))
}

/// The policy a reach and a grant mean.
///
/// Deny by default in both directions, with the allowances added on top.
///
/// # Two axes, and why the host is the narrow one
///
/// `reach` says how far *out* a session goes; `host_ports` says which doors on **this machine**
/// are open to it. Neither implies the other, and that is deliberate: the convenient way to
/// spell host reach is `DestinationGroup::Host` with no ports on it, and a rule with no ports
/// matches every port. Written that way, a session granted one service on this machine would
/// have them all — and widening the outside would silently widen the inside.
///
/// So the host rules are built one port at a time, TCP only. A connection the guest opens to the
/// gateway is rewritten to the host's loopback when the stack dials it, which is what makes a
/// granted port an actual door onto whatever the operator is running there.
///
/// # `allow_dns` comes first and is load-bearing
///
/// It is the narrow rule (UDP and TCP :53, to the gateway only) that lets a name be resolved at
/// all. Under deny-by-default a policy without it refuses every lookup, and a caller who asked
/// for the public internet gets a sandbox that can open a connection to an address it has no way
/// to learn.
///
/// # What resolution costs
///
/// The stack forwards queries upstream, to whatever the host's own `/etc/resolv.conf` names. So
/// any reach that can resolve can also put bytes into a hostname and watch them leave. **The
/// only air gap here is [`Network::Disabled`]**, which attaches no device.
fn policy(reach: Network, host_ports: &[u16]) -> NetworkPolicy {
    if matches!(reach, Network::Full) {
        return NetworkPolicy::allow_all();
    }

    let mut rules = vec![Rule::allow_dns()];
    if matches!(reach, Network::Public) {
        rules.push(Rule::allow_egress(Destination::Group(
            DestinationGroup::Public,
        )));
    }
    rules.extend(host_ports.iter().map(|&port| Rule {
        direction: Direction::Egress,
        destination: Destination::Group(DestinationGroup::Host),
        protocols: vec![Protocol::Tcp],
        ports: vec![PortRange::single(port)],
        action: Action::Allow,
    }));

    NetworkPolicy {
        default_egress: Action::Deny,
        default_ingress: Action::Deny,
        rules,
    }
}
