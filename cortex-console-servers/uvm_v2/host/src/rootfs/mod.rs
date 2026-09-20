//! `image` — the images a session boots from, tended without one.
//!
//! Three commands, each one a process that starts, does its work and exits: `build` makes an
//! image, `list` says which ones are here, `remove` deletes one. None of them speaks the
//! console protocol, so none of them touches stdin or stdout as a wire — a client that wants
//! an image spawns this binary a second time for the purpose, and the session half only ever
//! reads what these three leave behind.
//!
//! What is under [`home`] is content-addressed, which is what makes a rebuild cheap: a layer
//! whose digest is already there is not written again, and an image whose id is already
//! there is not built again.
//!
//! The shape of it is `microsandbox-image`'s, because that crate writes most of what is in
//! there: [`cache`] is a [`GlobalCache`] over [`home`] itself, so a pulled layer lands in
//! `layers/` under its diff_id and stays there, and the merged metadata and the descriptor
//! naming a stack of them land in `fsmeta/` and `vmdk/` beside it. What that layout has no
//! shelf for is an image as *this* end states it — a base with steps over it, which no
//! registry has heard of and nothing keys by a reference — so those go under [`images`].
//!
//! One shelf here is not addressed by what it holds, and cannot be: what a `RUN` leaves has a
//! fresh digest every time it runs, so it is filed under what was *asked* instead. That is
//! `runs/`, and `build::step` is the whole of it — including why the shape is the one the crate next
//! door already uses for the same problem.

mod build;
mod digest;
mod image;
mod layer;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::OnceLock;

use cortex::rootfs_v2::RootFsV2;
use microsandbox_image::GlobalCache;

pub use build::build;
pub use digest::Digest;
pub use image::{Image, Unknown};
pub use layer::Layer;

/// Where the images live: content-addressed, shared by every session, and kept.
pub fn home() -> PathBuf {
    crate::home().join("rootfs")
}

/// The store under [`home`], as the crate that writes into it sees it.
///
/// Opened once and held, because opening it makes the directories and a second caller has
/// nothing to say about a failure the first would already have died of — the same reason
/// [`home`] resolves the way it does.
pub fn cache() -> &'static GlobalCache {
    static CACHE: OnceLock<GlobalCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        GlobalCache::new(&home()).unwrap_or_else(|e| panic!("opening {}: {e}", home().display()))
    })
}

/// Where an image's manifest is kept, named by its digest.
pub fn images() -> PathBuf {
    home().join("images")
}

/// Dispatch `image <command>`, where `argv` is everything after `image`.
pub async fn run(argv: impl IntoIterator<Item = impl AsRef<str>>) -> anyhow::Result<()> {
    let mut argv = argv.into_iter();
    let command = argv.next();
    match command.as_ref().map(|c| c.as_ref()) {
        // `-r` carries a declaration written as JSON, `-d` carries the Dockerfile one would
        // be read out of. Both the thing itself rather than a path to it: this is a process
        // a client spawns, and what such a client has in hand is a value it built or a
        // document it was handed, which need not be a file anywhere this process can reach.
        Some("build") => {
            let mut recipe: Option<String> = None;
            let mut dockerfile: Option<String> = None;

            while let Some(argument) = argv.next() {
                match argument.as_ref() {
                    "-r" | "--recipe" => {
                        recipe = Some(
                            argv.next()
                                .map(|value| value.as_ref().to_owned())
                                .ok_or_else(|| anyhow::anyhow!("`-r` takes a value"))?,
                        )
                    }
                    "-d" | "--dockerfile" => {
                        dockerfile = Some(
                            argv.next()
                                .map(|value| value.as_ref().to_owned())
                                .ok_or_else(|| anyhow::anyhow!("`-d` takes a value"))?,
                        )
                    }
                    other => anyhow::bail!("{other:?} is not an argument `image build` takes"),
                }
            }

            let rootfs = match (recipe, dockerfile) {
                // Naming both is refused rather than resolved by an order nobody stated.
                (Some(_), Some(_)) => {
                    anyhow::bail!("both a recipe and a dockerfile were given; name one")
                }
                (None, None) => {
                    anyhow::bail!("neither a recipe nor a dockerfile was given")
                }
                (Some(json), None) => serde_json::from_str::<RootFsV2>(&json)
                    .map_err(|e| anyhow::anyhow!("reading the recipe: {e}"))?,
                // A Dockerfile that says something a declaration has no form for is refused
                // at the line that said it, by the reader — an image with a `CMD` or a
                // `USER` in it is not the image this would build, and the value handed back
                // is the whole of the answer, with no second channel for what was passed
                // over.
                (None, Some(text)) => RootFsV2::from_dockerfile(text)?,
            };

            // The digest on stdout, because that is what this command is for: a client
            // spawned it to learn what its declaration is called, and the answer is the one
            // line it reads back.
            println!("{}", build::build(rootfs).await?.digest()?);
            Ok(())
        }
        Some("list") => list(argv).await,
        Some("remove") => remove(argv).await,
        // No command a bare `image` should mean: the three differ in what they do to what is
        // on disk, and guessing between them is not a kindness.
        None => anyhow::bail!("`image` takes `build`, `list` or `remove`"),
        Some(other) => anyhow::bail!("{other:?} is not `build`, `list` or `remove`"),
    }
}

/// Write out the images that are here.
///
/// One line per image on stdout, tab-separated: its digest, how many layers it is made of, how
/// many bytes those layers occupy, and the reference its base was pulled from. Tabs and not
/// columns padded to a width, because the caller is a client that spawned this binary — `cut
/// -f1` is the whole of reading it, where a width is a format that changes with its contents.
///
/// Sorted by digest, which is an order and not a ranking: nothing here is newer or better than
/// anything else, and a listing that moves between runs is one nobody can diff.
///
/// The bytes are the layers' own, so two images sharing a base are each counted the whole of
/// it. What a listing says is what each image is made of; what removing one would give back is
/// a different question, and `remove` is what answers it — by doing it.
async fn list(argv: impl IntoIterator<Item = impl AsRef<str>>) -> anyhow::Result<()> {
    if let Some(unexpected) = argv.into_iter().next() {
        anyhow::bail!(
            "`list` takes no arguments, and {:?} is one",
            unexpected.as_ref()
        );
    }

    // No directory and an empty one are one answer: nothing has been built here.
    let entries = match std::fs::read_dir(images()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => anyhow::bail!("reading {}: {e}", images().display()),
    };

    let mut lines = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension() != Some("json".as_ref()) {
            continue;
        }

        // A manifest that does not parse is said and passed over. One file nobody can read is
        // not a reason for a listing to have nothing to say about the rest, and the listing is
        // where somebody finds out it is there at all.
        let image = match Image::read(&path) {
            Ok(image) => image,
            Err(e) => {
                eprintln!("{}: {e:#}", env!("CARGO_BIN_NAME"));
                continue;
            }
        };

        // Its own digest rather than the name of the file it was read from: that is what
        // `build` printed, what `remove` takes, and what a manifest somebody edited is no
        // longer called.
        let digest = image.digest()?;
        let mut bytes = 0u64;
        for layer in &image.layers {
            match std::fs::metadata(layer.path()) {
                Ok(found) => bytes += found.len(),
                // Not a boot that fails somewhere further in: an image missing a layer cannot
                // be made into a disk, and this is the command somebody ran to find out what
                // they have.
                Err(e) => eprintln!(
                    "{}: {digest} is missing {}: {e}",
                    env!("CARGO_BIN_NAME"),
                    layer.digest()
                ),
            }
        }

        lines.push(format!(
            "{digest}\t{}\t{bytes}\t{}",
            image.layers.len(),
            // A base nothing pulled — a declaration built over a digest already here — has no
            // reference to name, and a column with nothing in it is a field that reads as
            // missing rather than as empty.
            image.from.as_deref().unwrap_or("-")
        ));
    }

    lines.sort();
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// Delete an image, and give back whatever only it was holding.
///
/// The manifest goes first, and then a sweep: of everything the removed images named — layers,
/// the merged metadata and descriptor for the stacks they are, the notes about what their
/// `RUN`s left — whatever no remaining image names goes too. An image store that only ever
/// grows is not one anybody can keep, and deleting the manifest alone would leave the whole
/// cost of the image on disk under a name nothing points at.
///
/// **Only what the removed images named.** A layer some other image also lists stays, which is
/// what content-addressing is for; and a layer a pull is writing right now for an image not yet
/// stored is never a candidate, because it is not in any removed image's list. What that leaves
/// is the narrow case of a build reusing a layer of an image being removed underneath it, at
/// the moment it is removed — so this is a command to run against a store, not one to run
/// alongside a build in it.
async fn remove(argv: impl IntoIterator<Item = impl AsRef<str>>) -> anyhow::Result<()> {
    let mut removing = Vec::new();
    for argument in argv {
        let digest: Digest = argument.as_ref().parse()?;
        // Read before anything is deleted, so a call naming one image that is not here removes
        // none of the others: a client that mistyped the second of three digests is told so,
        // rather than told so after two are gone.
        let image = Image::load(&digest)?;
        removing.push((digest, image));
    }
    anyhow::ensure!(
        !removing.is_empty(),
        "`remove` takes the digest of an image, which is what `build` printed"
    );

    for (digest, _) in &removing {
        let held = Image::path(digest);
        std::fs::remove_file(&held)
            .map_err(|e| anyhow::anyhow!("removing {}: {e}", held.display()))?;
    }

    // What is left, read after the manifests are gone so that nothing in flight is counted as
    // keeping something alive that it does not.
    let (mut kept_layers, mut kept_stacks) = (HashSet::new(), HashSet::new());
    if let Ok(entries) = std::fs::read_dir(images()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension() != Some("json".as_ref()) {
                continue;
            }
            // An unreadable manifest counts as keeping what it names. It is the reading that
            // failed and not the image, and the cost of being wrong here is a layer that stays
            // — where the cost of the other guess is one that goes.
            let Ok(image) = Image::read(&path) else {
                continue;
            };
            kept_layers.extend(image.layers.iter().map(|layer| layer.digest().clone()));
            kept_stacks.extend(disks(&image));
        }
    }

    let (mut layers, mut stacks, mut images_removed) =
        (HashSet::new(), HashSet::new(), HashSet::new());
    for (digest, image) in &removing {
        layers.extend(image.layers.iter().map(|layer| layer.digest().clone()));
        stacks.extend(disks(image));
        images_removed.insert(digest.to_string());
    }

    let cache = cache();
    let mut freed = 0u64;
    for layer in layers.difference(&kept_layers) {
        freed += gone(&cache.layer_erofs_path(&layer.oci()));
        let _ = std::fs::remove_file(cache.layer_erofs_lock_path(&layer.oci()));
    }
    for stack in stacks.difference(&kept_stacks) {
        freed += gone(&cache.fsmeta_erofs_path(&stack.oci()));
        freed += gone(&cache.vmdk_path(&stack.oci()));
        let _ = std::fs::remove_file(cache.fsmeta_erofs_lock_path(&stack.oci()));
        let _ = std::fs::remove_file(cache.vmdk_lock_path(&stack.oci()));
    }
    let forgotten = build::forget_runs(&images_removed);

    // On stderr, because stdout is what a client reads: `build` prints a digest there and this
    // prints nothing, so a caller scripting the two never has to tell an answer from a remark.
    eprintln!(
        "{}: removed {} image{}, {freed} bytes, {forgotten} remembered RUN{}",
        env!("CARGO_BIN_NAME"),
        removing.len(),
        if removing.len() == 1 { "" } else { "s" },
        if forgotten == 1 { "" } else { "s" },
    );
    Ok(())
}

/// The digests naming the disks an image is read through: the stack it is, and — for one that
/// was pulled — the manifest a pull wrote a pair of its own under.
///
/// Two and not one because the two are written by different halves for the same image, under
/// names that cannot be derived from each other: `Image::disk` names a merge by the layers it
/// merged, and a registry pull names one by the manifest it resolved.
fn disks(image: &Image) -> Vec<Digest> {
    let mut named = vec![image.stack()];
    // `<reference>@<digest>`, as `pull` writes it. Split at the last `@`, since a reference may
    // carry one of its own.
    if let Some(from) = &image.from
        && let Some((_, manifest)) = from.rsplit_once('@')
        && let Ok(manifest) = manifest.parse()
    {
        named.push(manifest);
    }
    named
}

/// Delete `path` and say how many bytes that gave back, and nothing for one that was not there.
fn gone(path: &std::path::Path) -> u64 {
    let size = std::fs::metadata(path)
        .map(|found| found.len())
        .unwrap_or(0);
    match std::fs::remove_file(path) {
        Ok(()) => size,
        Err(_) => 0,
    }
}
