//! `exec` mode: one command, one answer, one layer.

/// Run one command and exit with what it exited with, leaving what it wrote where the host
/// can read it.
///
/// No port and no protocol: a build step wants one answer, and the answer is the status. What
/// the command changed is in the overlay's upper, which goes out as a tar the same way a
/// session's commit does — the host turns it into a layer, and this end still has no idea
/// what an image is.
pub fn run(image: crate::contract::ImageSpec, argv: impl Iterator<Item = String>) -> anyhow::Result<()> {
    use std::path::{Path, PathBuf};

    let argv: Vec<String> = argv.collect();
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("`exec` was given no command to run"))?;

    // What the image states, with a `PATH` under it: a base that named none would otherwise
    // leave a command unable to find anything it did not spell absolutely.
    let mut command = std::process::Command::new(program);
    command.args(rest);
    command.env("PATH", crate::contract::GUEST_PATH);
    for stated in &image.env {
        if let Some((key, value)) = stated.split_once('=') {
            command.env(key, value);
        }
    }
    if let Some(at) = image.working_dir.as_deref()
        && Path::new(at).is_dir()
    {
        command.current_dir(at);
    }

    let status = command
        .status()
        .map_err(|e| anyhow::anyhow!("running {program}: {e}"))?;

    // Written before the status is reported, because a step that failed is a step whose
    // partial work the host may still want to look at.
    if let Ok(scratch) = std::env::var(crate::contract::COMMIT_ENV) {
        // Everything this side put in the filesystem that is not the command's work, and one
        // of them is this binary. A mount point rather than a plain exclusion for the scratch
        // itself: the directories on the way to one exist only to reach it.
        let mut excluded: Vec<PathBuf> = [
            crate::contract::GUEST_BIN_PATH,
            "/oldroot",
            crate::contract::ABIN_PATH,
            crate::contract::RESOLV_CONF,
        ]
        .iter()
        .map(|path| PathBuf::from(path.trim_start_matches('/')))
        .collect();
        excluded.push(PathBuf::from(crate::contract::COMMIT_PATH.trim_start_matches('/')));

        let into = Path::new(&scratch).join(crate::contract::LAYER_TAR);
        crate::layer::write(Path::new(crate::contract::UPPER_DIR), &into, &excluded, &[])
            .map_err(|e| anyhow::anyhow!("writing this step's layer: {e}"))?;
    }

    std::process::exit(status.code().unwrap_or(crate::NOT_EXECUTABLE as i32))
}
