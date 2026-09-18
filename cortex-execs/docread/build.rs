//! Where `libpdfium.a` comes from.
//!
//! Nothing here runs without the `pdfium` feature. Off — which is the default and what the
//! `/abin` release is built with — this script prints two `rerun-if` lines and stops, so a
//! plain `cargo build` of this workspace never learns that a PDF engine exists.
//!
//! # Static, which is the whole reason this file is complicated
//!
//! `/abin` is a directory of statically linked executables that a guest runs directly. A
//! static musl binary cannot `dlopen`, so the usual way of using pdfium — ship
//! `libpdfium.so` beside the program and bind to it at run time — would mean this one
//! program becoming dynamically linked, `/abin` carrying a 17 MB shared object, and the
//! guest's loader being in the path of a file read. Linking the archive in keeps the
//! property that makes `/abin` what it is: files that run.
//!
//! Which is why a prebuilt library is not enough. Every published pdfium build is a shared
//! one — <https://github.com/bblanchon/pdfium-binaries> ships `.so`, `.dylib` and `.dll` and
//! no `.a` for any platform, musl included — so the archive is something this machine makes.
//!
//! # What it does, in order
//!
//! 1. [`DIR_ENV`], if it is set: the library somebody already has, used as it stands.
//! 2. The cache: a library this script built before, under [`CACHE_ENV`] or `~/.cache`, keyed
//!    by [`REVISION`] and target, so a second `cargo build` links rather than rebuilds.
//! 3. [`BUILD_ENV`], if it is set: check pdfium out and build it, then cache it.
//! 4. Otherwise, refuse and say which of those three to reach for.
//!
//! **The source build is behind an environment variable and not merely behind the feature.**
//! It is a multi-gigabyte checkout and tens of minutes of C++, and a build system that starts
//! that because a feature was on is one nobody can predict the cost of. Asking for it is one
//! variable; not asking for it is a message that says so.
//!
//! # The host, not the guest
//!
//! What this builds is for the machine it runs on. Cross-building pdfium to
//! `*-unknown-linux-musl` is not a matter of another `gn` argument — it needs a musl sysroot
//! and a C++ toolchain aimed at it, which is the same wall `mem` and `index` meet with
//! `rusqlite`'s C. So a musl target here is refused with that said, rather than started and
//! failed twenty minutes later. Building the guest's `/abin` on a guest-shaped machine is
//! what answers it, and it answers all three programs at once.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "pdfium")]
    pdfium::link_it();
}

/// Everything the `pdfium` feature needs, and nothing a build without it compiles.
#[cfg(feature = "pdfium")]
mod pdfium {
    use std::{
        path::{Path, PathBuf},
        process::Command,
    };

    /// A directory holding a `libpdfium.a` to use as it stands.
    ///
    /// Looked in twice: `<dir>/<target>/libpdfium.a` first, so one directory can hold a library
    /// per architecture the way `/abin` does, and `<dir>/libpdfium.a` after, which is what
    /// somebody with exactly one has.
    const DIR_ENV: &str = "CORTEX_PDFIUM_DIR";

    /// Where built libraries are kept between builds. `~/.cache/cortex/pdfium` if unset.
    const CACHE_ENV: &str = "CORTEX_PDFIUM_CACHE";

    /// Set to build pdfium from source when there is no library to be found.
    const BUILD_ENV: &str = "CORTEX_PDFIUM_BUILD";

    /// A published library to fetch instead, and the digest it has to hash to.
    ///
    /// Two variables and not one, because a URL without a digest is a build that links whatever
    /// answered: what this fetches ends up *inside* every binary this crate produces, so it is
    /// checked or it is not used. They override [`PINNED`] and are how somebody uses an artifact
    /// before there is a release to pin.
    const URL_ENV: &str = "CORTEX_PDFIUM_URL";
    const SHA256_ENV: &str = "CORTEX_PDFIUM_SHA256";

    /// Where a built library is published, by target, URL and digest — the same shape the base
    /// rootfs and `/abin` are pinned in.
    ///
    /// **Empty, because there is no release yet.** Building pdfium needs a toolchain per platform
    /// — Xcode on a Mac, a musl C++ compiler for the guest — and the whole point of a pin is that
    /// exactly one machine ever needs it. Until this has entries, everybody else's path is
    /// [`URL_ENV`] or [`DIR_ENV`], and [`unbuilt`] says so.
    const PINNED: &[(&str, &str, &str)] = &[];

    /// The pdfium to build, as a branch of its own repository.
    ///
    /// Pinned, and pinned *with* the `pdfium_7881` feature this crate takes from `pdfium-render`:
    /// those bindings are generated against one version of pdfium's headers, and a library built
    /// from another is a link that either fails outright or — for a function whose name survived a
    /// signature change — succeeds and is called wrongly. The two move together or neither moves.
    const REVISION: &str = "chromium/7881";

    /// pdfium's own source, and the toolchain that checks it out.
    const PDFIUM_GIT: &str = "https://pdfium.googlesource.com/pdfium.git";
    const DEPOT_TOOLS_GIT: &str =
        "https://chromium.googlesource.com/chromium/tools/depot_tools.git";

    /// The library, found or made, and the flags that link it.
    ///
    /// Called from a `main` that is three lines, because everything this needs — an HTTP
    /// client, a digest, a decompressor — is a dependency a build without the feature should
    /// not be compiling. A build script is compiled with its package's features, so leaving
    /// the whole of it behind one `cfg` is what makes that true rather than merely unused.
    pub fn link_it() {
        // Printed here rather than beside `rerun-if-changed`, because a variable that names
        // where a library comes from is only a reason to run again where there is a library
        // to look for.
        for var in [DIR_ENV, CACHE_ENV, BUILD_ENV, URL_ENV, SHA256_ENV] {
            println!("cargo:rerun-if-env-changed={var}");
        }

        let target = var("TARGET");
        let lib = locate(&target).unwrap_or_else(|said| panic!("{said}"));
        link(&lib, &target);
    }

    /// The `libpdfium.a` to link, found or made.
    fn locate(target: &str) -> Result<PathBuf, String> {
        if let Some(dir) = std::env::var_os(DIR_ENV) {
            let dir = PathBuf::from(dir);
            // The per-target name first: a directory holding one library per architecture is the
            // shape anybody building for a guest ends up with, and the bare name would be
            // whichever architecture they built last.
            for candidate in [dir.join(target).join(LIB), dir.join(LIB)] {
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
            return Err(format!(
                "{DIR_ENV} is {}, which holds neither {target}/{LIB} nor {LIB}",
                dir.display()
            ));
        }

        // A published library before a built one: fetching is seconds and needs nothing installed,
        // where building needs a toolchain and an hour. The source build is what *makes* one of
        // these, so it is the last resort and not the first.
        //
        // Kept under its own digest, and that is the whole of why this is asked before the cache
        // below: keyed by revision alone, a build pointed at another artifact reads the last one
        // out of the cache and never fetches, so what is linked and what the environment names
        // are two different libraries.
        if let Some((url, sha256)) = published(target) {
            let cached = cache()?
                .join(REVISION.replace('/', "-"))
                .join(target)
                .join(&sha256);
            if cached.join(LIB).is_file() {
                return Ok(cached.join(LIB));
            }
            return fetch(&url, &sha256, &cached);
        }

        let cached = cache()?.join(REVISION.replace('/', "-")).join(target);
        if cached.join(LIB).is_file() {
            return Ok(cached.join(LIB));
        }

        if std::env::var_os(BUILD_ENV).is_none() {
            return Err(unbuilt(target, &cached));
        }
        build(target, &cached)
    }

    /// The name the archive has everywhere it is looked for.
    const LIB: &str = "libpdfium.a";

    /// The library published for `target`, if there is one — the environment first, then [`PINNED`].
    ///
    /// The environment wins so that somebody can point a build at an artifact that is not a
    /// release yet, which is every artifact until there is one.
    fn published(target: &str) -> Option<(String, String)> {
        match (std::env::var(URL_ENV), std::env::var(SHA256_ENV)) {
            (Ok(url), Ok(sha256)) => return Some((url, sha256)),
            // Half of the pair is a mistake worth making loud rather than a reason to fall through
            // to an hour of C++.
            (Ok(_), Err(_)) => panic!(
                "{URL_ENV} is set and {SHA256_ENV} is not: a library that goes into every binary this builds is checked or it is not used"
            ),
            (Err(_), Ok(_)) => panic!("{SHA256_ENV} is set and {URL_ENV} is not"),
            (Err(_), Err(_)) => {}
        }
        PINNED
            .iter()
            .find(|(pinned, _, _)| *pinned == target)
            .map(|(_, url, sha256)| ((*url).to_owned(), (*sha256).to_owned()))
    }

    /// The most [`fetch`] will download. Room for an archive several times pdfium's size, and a
    /// bound all the same: what answers a URL is not something to let name this build's memory.
    const MAX_FETCH: u64 = 512 * 1024 * 1024;

    /// A published library, downloaded and checked, into `into`.
    ///
    /// Gzip is unwrapped and nothing else is: what is fetched is one file, so there is no archive
    /// to walk and no path inside it to be wrong about. A `.gz` suffix is what says so, because
    /// the alternative — sniffing — would make an unlucky `.a` into something to unpack.
    ///
    /// **The digest is of what was downloaded**, before any of that. It is the bytes somebody
    /// published that are being agreed to, and hashing what came out of a decompressor instead
    /// would be agreeing to whatever the decompressor made of them.
    fn fetch(url: &str, sha256: &str, into: &Path) -> Result<PathBuf, String> {
        say(&format!("fetching {url}"));
        let body = ureq::get(url)
            .call()
            .map_err(|e| format!("{url}: {e}"))?
            .body_mut()
            // `read_to_vec()` on its own is ureq's 10 MiB default, which no `libpdfium.a` is
            // under: the fetch then fails on size before the digest is ever looked at.
            .with_config()
            .limit(MAX_FETCH)
            .read_to_vec()
            .map_err(|e| format!("{url}: {e}"))?;

        let got = {
            use sha2::{Digest as _, Sha256};
            format!("{:x}", Sha256::digest(&body))
        };
        if !got.eq_ignore_ascii_case(sha256) {
            return Err(format!(
                "{url} hashes to {got} and not to {sha256}. Nothing was written: what this fetches              ends up inside every binary built from here, so a digest that does not match is              the end of the build and not a warning"
            ));
        }

        let library = if url.ends_with(".gz") {
            use std::io::Read as _;

            let mut out = Vec::new();
            flate2::read::GzDecoder::new(body.as_slice())
                .read_to_end(&mut out)
                .map_err(|e| format!("{url}: {e}"))?;
            out
        } else {
            body
        };

        std::fs::create_dir_all(into).map_err(|e| format!("{}: {e}", into.display()))?;
        let path = into.join(LIB);
        std::fs::write(&path, library).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }

    /// What there is to say when there is no library and nobody asked for one to be made.
    ///
    /// Three ways out and the cost of each, because the expensive one is the one this cannot
    /// decide on somebody's behalf.
    fn unbuilt(target: &str, cached: &Path) -> String {
        format!(
            "the `pdfium` feature is on and there is no {LIB} for {target}.\n\
             \n\
             Point {DIR_ENV} at a directory holding one (<dir>/{target}/{LIB} or <dir>/{LIB}),\n\
             or {URL_ENV} and {SHA256_ENV} at a published one — a `.a`, or a `.gz` of one,\n\
             fetched and checked against that digest,\n\
             or set {BUILD_ENV}=1 to check pdfium {REVISION} out and build it here — several GB\n\
             of source, tens of minutes of C++, and a toolchain per platform (Xcode on a Mac),\n\
             kept afterwards at {}.\n\
             \n\
             The build is how one of these is *made*, and it is meant to happen once: what the\n\
             next person needs is the artifact it produced, not the toolchain that made it.\n\
             \n\
             Or build without the feature, which is what the /abin release is.",
            cached.display()
        )
    }

    /// Where built libraries live between builds.
    ///
    /// Not `OUT_DIR`: that is emptied by `cargo clean` and is per-profile besides, so a debug
    /// build and a release build would each pay for pdfium once. What is cached here is not this
    /// crate's output — it is a third-party library at a pinned revision, which is the same for
    /// every profile and every checkout on this machine.
    fn cache() -> Result<PathBuf, String> {
        if let Some(dir) = std::env::var_os(CACHE_ENV) {
            return Ok(PathBuf::from(dir));
        }
        let home = std::env::var_os("HOME").ok_or_else(|| {
            format!(
                "neither {CACHE_ENV} nor HOME is set, so there is nowhere to keep a built library"
            )
        })?;
        Ok(PathBuf::from(home)
            .join(".cache")
            .join("cortex")
            .join("pdfium"))
    }

    /// pdfium, checked out and built, and the archive that came out of it.
    fn build(target: &str, into: &Path) -> Result<PathBuf, String> {
        let (target_os, target_cpu) = gn_target(target)?;

        // pdfium's mac configuration reads the SDK version out of Xcode, and Command Line Tools
        // cannot stand in: `gn gen` runs `xcodebuild -version`, which those do not have. Resolved
        // before the checkout, so a machine that cannot finish is told now rather than after
        // several gigabytes.
        let xcode = if target_os == "mac" { xcode()? } else { None };
        if let Some(dir) = &xcode {
            // Only when it had to be found — an environment somebody set themselves is theirs.
            say(&format!("using the Xcode at {dir}"));
            // SAFETY-adjacent, and deliberate: a build script is one process doing one thing, and
            // this is how every child of it — gn, ninja, and the python they run — finds Xcode.
            unsafe { std::env::set_var("DEVELOPER_DIR", dir) };
        }

        let work = into.parent().unwrap_or(into).join("src");
        std::fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;

        // depot_tools is how pdfium is checked out: `gclient` reads the DEPS file that names the
        // dozen or so repositories a pdfium tree is made of, and there is no tarball that has
        // them. It goes on PATH ahead of everything, which is what its own documentation asks
        // for and what makes `gclient` and `gn` here the ones just cloned.
        let depot = work.join("depot_tools");
        if !depot.is_dir() {
            say("cloning depot_tools");
            git(
                &work,
                &["clone", "--depth", "1", DEPOT_TOOLS_GIT, "depot_tools"],
            )?;
        }
        let path = {
            let existing = std::env::var("PATH").unwrap_or_default();
            format!("{}:{existing}", depot.display())
        };

        // `--unmanaged`, so that the checkout stays at the revision named below rather than being
        // moved to whatever the tree's own DEPS thinks is current.
        let pdfium = work.join("pdfium");
        if !pdfium.is_dir() {
            say(&format!(
                "checking out pdfium {REVISION} — this is the slow part"
            ));
            run(Command::new("gclient")
                .args(["config", "--unmanaged", "--name", "pdfium", PDFIUM_GIT])
                .current_dir(&work)
                .env("PATH", &path))?;
        }
        run(Command::new("gclient")
            .args([
                "sync",
                "--no-history",
                "--shallow",
                "--revision",
                &format!("pdfium@refs/heads/{REVISION}"),
            ])
            .current_dir(&work)
            .env("PATH", &path)
            // Nothing here is being uploaded and nothing depends on the metrics prompt, which
            // otherwise waits on a terminal that a build script does not have.
            .env("DEPOT_TOOLS_UPDATE", "0")
            .env("DEPOT_TOOLS_METRICS", "0"))?;

        let out = pdfium.join("out").join("Release");
        std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        std::fs::write(out.join("args.gn"), args_gn(target_os, target_cpu))
            .map_err(|e| format!("{}: {e}", out.join("args.gn").display()))?;

        say("gn gen");
        run(Command::new("gn")
            .args(["gen", "out/Release"])
            .current_dir(&pdfium)
            .env("PATH", &path))?;

        say("ninja — tens of minutes");
        run(Command::new("ninja")
            .args(["-C", "out/Release", "pdfium"])
            .current_dir(&pdfium)
            .env("PATH", &path))?;

        // Where a complete lib lands. Copied out rather than linked in place, because the cache
        // is what the next build looks in and a source tree is a thing people delete.
        let built = out.join("obj").join(LIB);
        if !built.is_file() {
            return Err(format!(
                "ninja finished and there is no {}: `pdf_is_complete_lib` is what makes one, and \
                 it is set in the args below — so this is pdfium's build having changed shape",
                built.display()
            ));
        }
        std::fs::create_dir_all(into).map_err(|e| format!("{}: {e}", into.display()))?;
        let cached = into.join(LIB);
        std::fs::copy(&built, &cached).map_err(|e| format!("{}: {e}", cached.display()))?;
        Ok(cached)
    }

    /// The build pdfium is asked for, argument by argument.
    ///
    /// Every line is load-bearing:
    ///
    /// - `pdf_is_complete_lib` is the one that produces an `.a` at all, with pdfium's own
    ///   dependencies — freetype, the image codecs — inside it rather than beside it.
    /// - `use_custom_libcxx = false` links the platform's C++ standard library instead of the one
    ///   Chromium vendors. Two C++ runtimes in one binary is a link that succeeds and then
    ///   crashes, and the Rust side has to name the same one — see [`link`].
    /// - `pdf_enable_v8` and `pdf_enable_xfa` are what a JavaScript engine and XFA forms would
    ///   cost. Text out of a page needs neither, and V8 alone is most of the size.
    /// - `pdf_use_partition_alloc = false` keeps pdfium on the system allocator, which is the one
    ///   the Rust half of the binary already has.
    fn args_gn(target_os: &str, target_cpu: &str) -> String {
        format!(
            "is_debug = false\n\
             pdf_is_standalone = true\n\
             pdf_is_complete_lib = true\n\
             pdf_enable_v8 = false\n\
             pdf_enable_xfa = false\n\
             pdf_use_partition_alloc = false\n\
             use_custom_libcxx = false\n\
             clang_use_chrome_plugins = false\n\
             treat_warnings_as_errors = false\n\
             target_os = \"{target_os}\"\n\
             target_cpu = \"{target_cpu}\"\n"
        )
    }

    /// A Rust target triple as `gn` spells it, or the reason this one is not built here.
    fn gn_target(target: &str) -> Result<(&'static str, &'static str), String> {
        let cpu = match target.split('-').next() {
            Some("aarch64") => "arm64",
            Some("x86_64") => "x64",
            _ => return Err(format!("no pdfium build here for {target}")),
        };
        if target.ends_with("-apple-darwin") {
            return Ok(("mac", cpu));
        }
        if target.contains("-linux-gnu") {
            return Ok(("linux", cpu));
        }
        // The guest's target, and the one this cannot do from here.
        if target.contains("-linux-musl") {
            return Err(format!(
                "{target} needs a musl sysroot and a C++ toolchain aimed at it, which this build \
                 script does not have and cannot conjure. Build the library on a musl machine — \
                 the same one that has to build `mem` and `index`, whose C has the same problem — \
                 and point {DIR_ENV} at it."
            ));
        }
        Err(format!("no pdfium build here for {target}"))
    }

    /// The flags that put the archive in the binary.
    ///
    /// The C++ standard library is named here and not left to chance: pdfium is C++ and its
    /// archive holds undefined symbols for whichever runtime it was compiled against, which
    /// `use_custom_libcxx = false` in [`args_gn`] makes the platform's. The frameworks are what a
    /// macOS build of pdfium calls for text and graphics.
    fn link(lib: &Path, target: &str) {
        let dir = lib.parent().expect("a library is in a directory");
        println!("cargo:rerun-if-changed={}", lib.display());
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=pdfium");

        if target.ends_with("-apple-darwin") {
            println!("cargo:rustc-link-lib=dylib=c++");
            for framework in ["CoreFoundation", "CoreGraphics", "CoreText", "AppKit"] {
                println!("cargo:rustc-link-lib=framework={framework}");
            }
        } else {
            // Static, to match the binary this ends up in: a `/abin` program that needed
            // `libstdc++.so` in the guest would be the dynamic linking this whole file exists to
            // avoid. It has to be findable — an Alpine build machine has it in `g++`.
            println!("cargo:rustc-link-lib=static=stdc++");
        }
    }

    /// A build-script variable that is always there, or a panic naming it.
    fn var(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("cargo sets {name}"))
    }

    /// A line the person waiting on this can see.
    ///
    /// `cargo:warning` because it is the only channel a build script has to somebody's terminal,
    /// and a checkout that takes twenty minutes in silence looks like one that has hung.
    fn say(said: &str) {
        println!("cargo:warning=pdfium: {said}");
    }

    /// The Xcode this build has to be pointed at, when what is chosen will not do.
    ///
    /// `None` means nothing needs overriding: either `DEVELOPER_DIR` is already set — somebody's
    /// deliberate choice, and not this script's to second-guess — or `xcode-select` already points
    /// at an Xcode.
    ///
    /// The fallback is a look through `/Applications` rather than a hard-coded path, because a
    /// machine whose only Xcode is `Xcode-beta.app` has that as its Xcode.
    fn xcode() -> Result<Option<String>, String> {
        if std::env::var_os("DEVELOPER_DIR").is_some() {
            return Ok(None);
        }
        if let Ok(chosen) = Command::new("xcode-select").arg("-p").output()
            && chosen.status.success()
            && String::from_utf8_lossy(&chosen.stdout).contains("Xcode")
        {
            return Ok(None);
        }

        let mut found: Vec<PathBuf> = std::fs::read_dir("/Applications")
            .map_err(|e| format!("/Applications: {e}"))?
            .flatten()
            .map(|app| app.path())
            .filter(|app| {
                app.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("Xcode") && name.ends_with(".app"))
            })
            .map(|app| app.join("Contents").join("Developer"))
            .filter(|developer| developer.is_dir())
            .collect();
        // Sorted, so that a machine with two of them picks the same one twice rather than whichever
        // the directory happened to list first.
        found.sort();

        match found.first() {
            Some(developer) => Ok(Some(developer.display().to_string())),
            None => Err(format!(
                "building pdfium on a Mac needs Xcode, and this machine has Command Line Tools: \
                 `gn gen` runs `xcodebuild`, which those do not have.\n\
                 \n\
                 Which is the reason a built library is meant to be fetched rather than rebuilt — \
                 see {URL_ENV}. To build one here anyway, install Xcode, or point DEVELOPER_DIR at \
                 one that is already installed."
            )),
        }
    }

    fn git(dir: &Path, args: &[&str]) -> Result<(), String> {
        run(Command::new("git").args(args).current_dir(dir))
    }

    /// One command, with its failure said in full.
    ///
    /// Inherited stdio rather than captured: this is a build that prints for half an hour, and
    /// what it prints is the only sign of where it is.
    fn run(command: &mut Command) -> Result<(), String> {
        let name = format!("{command:?}");
        match command.status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("{name} exited with {status}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
                "{name}: not found — a pdfium build needs git, python3 and the depot_tools this \
                 fetches on PATH"
            )),
            Err(e) => Err(format!("{name}: {e}")),
        }
    }
}
