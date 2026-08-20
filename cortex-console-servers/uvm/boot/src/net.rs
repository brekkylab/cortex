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
use cortex_uvm_boot::Network;
use microsandbox_network::network::SmoltcpNetwork;
use microsandbox_network::policy::{Action, Destination, DestinationGroup, NetworkPolicy, Rule};
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

/// Bring a stack up under the policy `reach` describes.
///
/// Never called for [`Network::Disabled`], which attaches no device at all: a policy that
/// refuses everything and a guest with no interface are not the same thing, and the second is
/// the one worth having when nothing was asked for.
pub fn start(reach: Network) -> anyhow::Result<Stack> {
    debug_assert!(reach != Network::Disabled);

    // Everything but the policy left as the stack's own default: the addresses, the MTU, the
    // DNS timeouts. This process has an opinion about what a sandbox may reach and none about
    // how the stack goes about it.
    let config = microsandbox_network::config::NetworkConfig {
        policy: policy(reach),
        ..Default::default()
    };

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
}

/// The policy a reach means.
///
/// Deny by default in both directions, with the allowances added on top. Which makes the order
/// below load-bearing in one place: **`Rule::allow_dns` has to be there for any of the rest to
/// be reachable**, because it is the narrow rule (UDP and TCP :53, to the gateway only) that
/// lets a name be resolved at all. A policy without it refuses every lookup, and a caller who
/// asked for the public internet gets a sandbox that can open a connection to an address it has
/// no way to learn.
fn policy(reach: Network) -> NetworkPolicy {
    if matches!(reach, Network::Full) {
        return NetworkPolicy::allow_all();
    }

    let mut rules = vec![Rule::allow_dns()];
    if matches!(reach, Network::Public) {
        rules.push(Rule::allow_egress(Destination::Group(
            DestinationGroup::Public,
        )));
    }

    NetworkPolicy {
        default_egress: Action::Deny,
        default_ingress: Action::Deny,
        rules,
    }
}
