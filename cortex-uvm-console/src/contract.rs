//! What a boot tells the guest, and what a boot child is told.
//!
//! Two cross-process contracts happen to be the same handful of strings, so they live
//! together:
//!
//! - **this process to the boot child** ([`KERNEL_ENV`] and the rest). The child is a copy
//!   of this binary in the [`Boot`](crate::Role::Boot) role, so both ends are this crate
//!   and nothing here can drift.
//! - **this process to the guest** ([`LOWER_ENV`], [`UPPER_ENV`], [`SHARE_ENV`],
//!   [`GUEST_BIN_PATH`], [`PORT_NAME`], [`HANDSHAKE`]). The other end of that one is
//!   `cortex-uvm-guest`'s `contract` module, which **has to change with this file** — the
//!   two crates are built for different targets, so neither can depend on the other and
//!   the compiler will not notice a mismatch.
//!
//! The environment is the channel because it is the only one libkrun's `exec` has that
//! survives: an argument becomes part of the guest's kernel command line, which is
//! size-limited and rejects a newline. Nothing that could grow is here — the session
//! itself arrives on [`PORT_NAME`] as protocol frames.

/// Where a boot writes the guest binary in the boot root, and therefore the path libkrun
/// execs. Also where the guest puts a copy of itself after its pivot, which is what the
/// delegated names end up symlinked to.
pub const GUEST_BIN_PATH: &str = "/.cortex-guest";

/// The virtio-console port carrying the console session.
pub const PORT_NAME: &str = "cortex-console";

/// Sent by the guest, once, as soon as it has the port open, and consumed here before the
/// first frame goes out.
///
/// A virtio-console port **discards** what the host writes while no process in the guest
/// has it open, rather than queueing it. So a boot cannot put `init` on the wire as soon
/// as the child connects — the socket is up long before the kernel is — and this is the
/// guest saying that something is reading.
pub const HANDSHAKE: &[u8; 8] = b"CORTEXUV";

/// The libkrunfw kernel the child boots.
pub const KERNEL_ENV: &str = "CORTEX_UVM_KERNEL";

/// The boot root: a directory served as the guest's virtio-fs root, holding the guest
/// binary and nothing else.
pub const BOOT_ROOT_ENV: &str = "CORTEX_UVM_BOOT_ROOT";

/// Where the child connects to reach this process. One socket, one connection, and its
/// descriptor becomes the guest's console port.
pub const CHANNEL_ENV: &str = "CORTEX_UVM_CHANNEL";

/// Guest vCPU count, if the caller overrode it.
pub const VCPUS_ENV: &str = "CORTEX_UVM_VCPUS";

/// Guest memory in MiB, if the caller overrode it.
pub const MEMORY_ENV: &str = "CORTEX_UVM_MEMORY_MIB";

/// The file holding this session's [`WorkspaceSpec`](cortex::fs::WorkspaceSpec), as BSON.
///
/// A file and not an environment value because a spec carries credentials: `environ` is
/// readable by any same-uid process for the life of the process, where a `0600` file stops
/// being readable once the boot child has unlinked it. It also has no size limit.
///
/// Unset means a session that declared no namespace, which is a session.
pub const SPEC_ENV: &str = "CORTEX_UVM_SPEC";

/// The read-only base image, as a host path (the child) — and as a guest block device
/// (the guest). Both ends of the overlay are named twice for that reason: one side has a
/// file, the other has a device.
pub const BASE_IMAGE_ENV: &str = "CORTEX_UVM_BASE_IMAGE";

/// The session's writable image, as a host path.
pub const SESSION_IMAGE_ENV: &str = "CORTEX_UVM_SESSION_IMAGE";

/// The overlay's lower, as the guest sees it. Attach order is what fixes the names, so
/// this is a promise [`crate::boot`] keeps rather than something either end computes.
pub const GUEST_LOWER_DEV: &str = "/dev/vdb";

/// The overlay's upper, as the guest sees it.
pub const GUEST_UPPER_DEV: &str = "/dev/vda";

/// Told to the guest as `CORTEX_UVM_LOWER`, which is the name its `contract` module reads.
pub const LOWER_ENV: &str = "CORTEX_UVM_LOWER";

/// Told to the guest as `CORTEX_UVM_UPPER`.
pub const UPPER_ENV: &str = "CORTEX_UVM_UPPER";

/// Told to the guest as `CORTEX_UVM_SHARE`, spelled `tag:/guest/path`.
pub const SHARE_ENV: &str = "CORTEX_UVM_SHARE";

/// The virtio-fs tag the workspace is attached under. Never seen by a caller: it is an
/// identifier two device configurations agree on, and the guest mounts it by this name.
pub const WORKSPACE_TAG: &str = "cortexws";

/// Where the workspace is mounted inside the guest.
///
/// A constant and not a caller's choice: a spec's mount paths are relative to the workspace's
/// own root, so where that root lands is this backend's business. Fixing it is also what gives
/// the guest agent a prefix to strip when it reports a `cwd`.
pub const GUEST_WORKSPACE_ROOT: &str = "/workspace";
