//! What a skill *is*: [`Skill`], the definition, and `skill.md`, the file it is written as.
//!
//! A skill is a directory. `skill.md` is what makes it one — a manifest whose frontmatter says
//! the two things a caller has to know before reading any further (what the skill is called and
//! what it is for) and whose body is the instructions themselves. Everything beside it in the
//! directory is the skill's own material: scripts, templates, references the instructions point
//! at by relative path.
//!
//! That shape is the reason this type exists at all rather than a struct with the instructions
//! in it. The agent never sees a `Skill` — it sees a directory, and reads it with `cat` and
//! `ls`, the way it reads everything else in a [`WorkFs`](crate::fs::WorkFs). A `Skill` is what
//! a *caller* holds while assembling one, and [`write_to`](Skill::write_to) is the moment it
//! stops being a value in this process and becomes files.
//!
//! # The frontmatter is parsed here, not by a YAML library
//!
//! Only two keys are read, both of them scalars, out of a block delimited by `---`. That is a
//! dozen lines, against a dependency that would have to be compiled by every consumer of the
//! crate — including the ones that mount an object store and never touch a skill. Bare, single-
//! and double-quoted scalars are understood; block scalars, nesting and anchors are not, and a
//! key this module does not know is passed over rather than refused, so a manifest carrying
//! whatever else its ecosystem defines still reads here.

use std::{
    io,
    path::{Component, Path, PathBuf},
};

use crate::fs::{DirentKind, FileSystem};

/// The name a skill's manifest is *written* under.
pub const MANIFEST: &str = "skill.md";

/// The names a manifest is *read* under, in the order they are tried.
///
/// Reading accepts more spellings than writing produces, and deliberately: a skill directory
/// that already exists on a host was written by whatever tool made it, and the uppercase
/// spelling is what the surrounding ecosystem uses. Refusing it would make
/// [`from_passthrough`](super::SkillDir::from_passthrough) — the constructor whose whole purpose
/// is to take a directory this crate did not create — the one that usually fails.
const MANIFEST_NAMES: [&str; 2] = [MANIFEST, "SKILL.md"];

/// The largest manifest this module will read into memory.
///
/// A ceiling, not a capacity plan. The file comes from wherever the store points — a host
/// directory, an object store — so its size is not this process's to trust, and a listing that
/// reads every skill's manifest would otherwise be one bad file away from exhausting the heap.
const MAX_MANIFEST: usize = 1 << 20;

/// How much of a file one [`FileSystem::read_at`] asks for.
const CHUNK: usize = 64 * 1024;

/// One file that sits beside the manifest in a skill directory.
///
/// `path` is relative to the skill's own root and may name subdirectories; the directories on
/// the way are created when the skill is written.
#[derive(Clone, Debug)]
pub struct SkillFile {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

/// A skill: a name, what it is for, the instructions, and the files that come with them.
///
/// This is the *definition* — the value a caller builds and then materializes into a directory
/// with [`write_to`](Self::write_to), or hands to
/// [`SkillDir::from_inmem`](super::SkillDir::from_inmem) to get that directory as a store. It is
/// not a handle on a skill that already exists: once written, the directory is the skill, and
/// this value has no further connection to it.
///
/// ```
/// # use cortex::skill::Skill;
/// let skill = Skill::new("summarize", "Condense a long document into its claims.")
///     .with_instructions("Read the file, then write one bullet per claim.")
///     .try_with_file("templates/report.md", "# Claims\n")
///     .unwrap();
/// assert!(skill.manifest().starts_with("---\n"));
/// ```
#[derive(Clone, Debug)]
pub struct Skill {
    /// What the skill is called. This is the name a caller announces it by, and it is *not*
    /// taken from the directory: a skill mounted at `docs/tools/summarize` may still be called
    /// `summarize`, and the manifest is the one place that says so.
    pub name: String,

    /// One line on what the skill is for — the only part of a skill read before deciding
    /// whether to read the rest, which is why it is a separate field and not the first
    /// paragraph of the instructions.
    pub description: String,

    /// The body of the manifest: the instructions themselves, as markdown.
    pub instructions: String,

    /// The files that come with the instructions.
    ///
    /// Private because their paths are checked on the way in — a `..` or an absolute path here
    /// would write outside the skill directory, and a second file named `skill.md` would
    /// overwrite the manifest. [`try_with_file`](Self::try_with_file) is the only way to add
    /// one, which is what makes those unrepresentable rather than merely documented.
    files: Vec<SkillFile>,
}

impl Skill {
    /// A skill with no instructions and no files yet — the two things every skill must say,
    /// and nothing else.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Skill {
            name: name.into(),
            description: description.into(),
            instructions: String::new(),
            files: Vec::new(),
        }
    }

    /// Builder-style: the markdown body of the manifest.
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    /// Builder-style: one file beside the manifest, at `path` relative to the skill's root.
    ///
    /// [`InvalidFilename`](io::ErrorKind::InvalidFilename) for a path that is absolute, walks
    /// out through `..`, or names the manifest — each of which would put bytes somewhere the
    /// skill does not own.
    pub fn try_with_file(
        mut self,
        path: impl AsRef<Path>,
        bytes: impl Into<Vec<u8>>,
    ) -> io::Result<Self> {
        let path = relative(path.as_ref())?;
        if MANIFEST_NAMES.iter().any(|name| path == Path::new(name)) {
            return Err(io::ErrorKind::InvalidFilename.into());
        }
        self.files.push(SkillFile {
            path,
            bytes: bytes.into(),
        });
        Ok(self)
    }

    /// The files that come with the instructions.
    ///
    /// Empty for a skill read back off a store: [`read`](Self::read) parses the manifest and
    /// stops, because the bytes are already on the store the caller read from and a listing has
    /// no use for them.
    pub fn files(&self) -> &[SkillFile] {
        &self.files
    }

    /// The manifest this skill is written as: frontmatter, then the instructions.
    ///
    /// Both scalars are double-quoted whatever they contain, so a description with a colon in
    /// it — which is most of them, sooner or later — cannot produce a file that reads back as
    /// something else.
    pub fn manifest(&self) -> String {
        format!(
            "---\nname: \"{}\"\ndescription: \"{}\"\n---\n\n{}\n",
            escape(&self.name),
            escape(&self.description),
            self.instructions.trim_end()
        )
    }

    /// Parse a manifest.
    ///
    /// [`InvalidData`](io::ErrorKind::InvalidData) if the frontmatter is missing, unterminated,
    /// or does not carry both a name and a description — all three describing a file that
    /// cannot answer what a skill is called or what it is for, which is the whole of what a
    /// manifest is read for.
    pub fn parse(text: &str) -> io::Result<Self> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut lines = text.lines();
        if lines.next().map(str::trim_end) != Some("---") {
            return Err(invalid(
                "a skill manifest must open with a `---` frontmatter block",
            ));
        }

        let (mut name, mut description) = (None, None);
        let mut closed = false;
        for line in lines.by_ref() {
            if line.trim_end() == "---" {
                closed = true;
                break;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            match key.trim() {
                "name" => name = Some(scalar(value)),
                "description" => description = Some(scalar(value)),
                // A key this module does not read. Not an error: a manifest may carry whatever
                // else the tool that wrote it defines, and refusing here would make this the
                // stricter reader of a file format it does not own.
                _ => {}
            }
        }
        if !closed {
            return Err(invalid("the frontmatter block is not closed by a `---`"));
        }

        let name = name.filter(|n: &String| !n.is_empty());
        let description = description.filter(|d: &String| !d.is_empty());
        let (Some(name), Some(description)) = (name, description) else {
            return Err(invalid(
                "a skill manifest needs a non-empty `name` and `description`",
            ));
        };

        Ok(Skill {
            name,
            description,
            // The blank line the renderer puts between the frontmatter and the body is
            // punctuation of the file, not the first line of the instructions.
            instructions: lines
                .collect::<Vec<_>>()
                .join("\n")
                .trim_matches('\n')
                .into(),
            files: Vec::new(),
        })
    }

    /// Read the skill defined by the directory `dir` on `fs`.
    ///
    /// The manifest alone — see [`files`](Self::files) for why the rest of the directory is
    /// left where it is. [`NotFound`](io::ErrorKind::NotFound) if the directory holds no
    /// manifest under any of the names one is read under, which is also the answer for a
    /// directory that is simply not a skill.
    pub async fn read(fs: &dyn FileSystem, dir: &Path) -> io::Result<Self> {
        let path = manifest_path(fs, dir).await?;
        let bytes = read_all(fs, &path).await?;
        let text = String::from_utf8(bytes)
            .map_err(|_| invalid("a skill manifest must be valid UTF-8"))?;
        Skill::parse(&text)
    }

    /// Write this skill into `dir` on `fs`, creating the directory and any parents its files
    /// need.
    ///
    /// Not atomic, and cannot be: a store's contract has no transaction in it, so a failure
    /// partway leaves the files written so far. What that costs is bounded by the order —
    /// the manifest goes down first, so a directory that has one has at least a readable skill,
    /// and one that does not is not a skill at all rather than a half-described one.
    ///
    /// An existing file at any of these names is replaced, so writing the same skill twice is
    /// the same as writing it once. The store must be writable; a read-only one answers
    /// [`ReadOnlyFilesystem`](io::ErrorKind::ReadOnlyFilesystem) here.
    pub async fn write_to(&self, fs: &dyn FileSystem, dir: &Path) -> io::Result<()> {
        mkdir_all(fs, dir).await?;
        write_file(fs, &dir.join(MANIFEST), self.manifest().as_bytes()).await?;
        for file in &self.files {
            let path = dir.join(&file.path);
            if let Some(parent) = path.parent() {
                mkdir_all(fs, parent).await?;
            }
            write_file(fs, &path, &file.bytes).await?;
        }
        Ok(())
    }
}

/// The manifest inside `dir`, under whichever of [`MANIFEST_NAMES`] is there.
///
/// A name that stats as a directory is passed over rather than accepted, since what follows is
/// a read of its bytes; anything other than [`NotFound`](io::ErrorKind::NotFound) surfaces,
/// because a store that cannot say whether the file is there has not said it is absent.
async fn manifest_path(fs: &dyn FileSystem, dir: &Path) -> io::Result<PathBuf> {
    for name in MANIFEST_NAMES {
        let path = dir.join(name);
        match fs.stat(&path).await {
            Ok(stat) if stat.kind == DirentKind::File => return Ok(path),
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no `{MANIFEST}` in {}", dir.display()),
    ))
}

/// Read a whole file, refusing one larger than [`MAX_MANIFEST`].
///
/// The loop is not decoration: a short read means end of file *and nothing else*, so a store
/// that answers a partial chunk has to be asked again rather than believed.
async fn read_all(fs: &dyn FileSystem, path: &Path) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let start = out.len();
        // Strictly greater, so a file of exactly the ceiling is read rather than refused: the
        // limit is on what this will hold, and it holds that.
        if start > MAX_MANIFEST {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        out.resize(start + CHUNK, 0);
        let n = fs.read_at(path, &mut out[start..], start as u64).await?;
        out.truncate(start + n);
        if n == 0 {
            return Ok(out);
        }
    }
}

/// Replace the file at `path` with `bytes`, creating it if it is not there.
///
/// `create` is exclusive, so `AlreadyExists` is the ordinary answer for a rewrite rather than a
/// failure — the truncate that follows is what makes the second write leave the file saying
/// only what this one wrote, instead of these bytes over the tail of the last.
async fn write_file(fs: &dyn FileSystem, path: &Path, bytes: &[u8]) -> io::Result<()> {
    match fs.create(path).await {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => fs.truncate(path, 0).await?,
        Err(err) => return Err(err),
    }
    let mut written = 0;
    while written < bytes.len() {
        // A short write is legal, and draining the buffer is the caller's job.
        let n = fs.write_at(path, &bytes[written..], written as u64).await?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        written += n;
    }
    Ok(())
}

/// Create `dir` and every directory above it, treating one that is already there as done.
///
/// A component that exists as a *file* is passed over here and surfaces at the create that
/// follows, as `ENOTDIR` — the error that names what is actually wrong, where an `AlreadyExists`
/// raised here would blame the directory.
///
/// `AlreadyExists` is not the only refusal a directory that is already there produces, which is
/// why a failure asks before giving up. A store's own root is a name it may not consider
/// creatable at all — `InMemFs` calls it `InvalidFilename`, having no parent to put an entry in
/// — and that is exactly the path a skill written at a mount point walks through first. The
/// original error is what surfaces if the `stat` does not vindicate it, since a store that
/// cannot say what is there has not said a directory is.
async fn mkdir_all(fs: &dyn FileSystem, dir: &Path) -> io::Result<()> {
    let mut built = PathBuf::new();
    for component in dir.components() {
        built.push(component);
        match fs.mkdir(&built).await {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => match fs.stat(&built).await {
                Ok(stat) if stat.kind == DirentKind::Dir => {}
                _ => return Err(err),
            },
        }
    }
    Ok(())
}

/// A path a skill may own: relative, and made only of plain names.
fn relative(path: &Path) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            _ => return Err(io::ErrorKind::InvalidFilename.into()),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(io::ErrorKind::InvalidFilename.into());
    }
    Ok(out)
}

/// One frontmatter value, unquoted if it was quoted.
///
/// Anything that is not a matched pair of quotes is taken literally, which is what a bare YAML
/// scalar is. An unterminated quote therefore reads as part of the value rather than as an
/// error: this reader's job is to recover the name and the description, and a stray quote in
/// one of them is not a reason to refuse the whole skill.
fn scalar(value: &str) -> String {
    let value = value.trim();
    if let Some(inner) = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return unescape(inner);
    }
    if let Some(inner) = value
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return inner.replace("''", "'");
    }
    value.to_string()
}

/// Undo [`escape`], leaving an escape this module does not write as the character it names —
/// a `\d` is a `d`, the way a double-quoted YAML scalar reads it.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            // A trailing backslash: nothing follows it to escape.
            None => out.push('\\'),
        }
    }
    out
}

/// Make `value` safe inside the double quotes the renderer puts it in. The backslash goes first,
/// or the escapes added after it would be escaped in turn.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::InMemFs;

    fn skill() -> Skill {
        Skill::new("summarize", "Condense a document: claims only.")
            .with_instructions("# Summarize\n\nRead it, then write the claims.")
    }

    #[test]
    fn a_rendered_manifest_parses_back_to_the_same_skill() {
        let parsed = Skill::parse(&skill().manifest()).unwrap();
        assert_eq!(parsed.name, "summarize");
        assert_eq!(parsed.description, "Condense a document: claims only.");
        assert_eq!(
            parsed.instructions,
            "# Summarize\n\nRead it, then write the claims."
        );
    }

    /// The one character that would end the scalar early if the renderer did not quote.
    #[test]
    fn a_quote_in_a_description_survives_the_round_trip() {
        let skill = Skill::new("q", "He said \"no\", then a backslash: \\");
        let parsed = Skill::parse(&skill.manifest()).unwrap();
        assert_eq!(parsed.description, "He said \"no\", then a backslash: \\");
    }

    #[test]
    fn bare_and_single_quoted_scalars_read_as_written() {
        let parsed =
            Skill::parse("---\nname: plain\ndescription: 'it''s fine'\n---\nbody").unwrap();
        assert_eq!(parsed.name, "plain");
        assert_eq!(parsed.description, "it's fine");
        assert_eq!(parsed.instructions, "body");
    }

    /// A manifest carrying keys this module does not know still reads — the format is not this
    /// crate's to police.
    #[test]
    fn an_unknown_key_is_passed_over() {
        let parsed =
            Skill::parse("---\nname: n\nallowed-tools: Read\ndescription: d\n---\n").unwrap();
        assert_eq!(parsed.name, "n");
        assert_eq!(parsed.instructions, "");
    }

    #[test]
    fn a_manifest_without_frontmatter_is_refused() {
        assert_eq!(
            Skill::parse("# just markdown").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn an_unterminated_frontmatter_is_refused() {
        assert_eq!(
            Skill::parse("---\nname: n\ndescription: d\n")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_manifest_missing_a_description_is_refused() {
        assert_eq!(
            Skill::parse("---\nname: n\n---\nbody").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_file_may_not_escape_the_skill_directory() {
        let skill = skill();
        assert!(skill.clone().try_with_file("../outside", "x").is_err());
        assert!(skill.clone().try_with_file("/etc/passwd", "x").is_err());
        // The manifest is not the caller's to write twice, under either spelling.
        assert!(skill.clone().try_with_file("skill.md", "x").is_err());
        assert!(skill.try_with_file("SKILL.md", "x").is_err());
    }

    #[tokio::test]
    async fn a_written_skill_reads_back_with_its_files_in_place() {
        let fs = InMemFs::new();
        let skill = skill()
            .try_with_file("templates/report.md", "# Claims\n")
            .unwrap();
        skill
            .write_to(&fs, Path::new("skills/summarize"))
            .await
            .unwrap();

        let read = Skill::read(&fs, Path::new("skills/summarize"))
            .await
            .unwrap();
        assert_eq!(read.name, "summarize");
        // Not carried back: the bytes are on the store the caller just read from.
        assert!(read.files().is_empty());
        assert_eq!(
            read_all(&fs, Path::new("skills/summarize/templates/report.md"))
                .await
                .unwrap(),
            b"# Claims\n"
        );
    }

    /// Writing the same skill twice leaves the file saying what the second write said, not the
    /// second write over the tail of the first.
    #[tokio::test]
    async fn a_rewrite_replaces_the_manifest() {
        let fs = InMemFs::new();
        let dir = Path::new("s");
        skill().write_to(&fs, dir).await.unwrap();
        Skill::new("s", "short").write_to(&fs, dir).await.unwrap();

        let read = Skill::read(&fs, dir).await.unwrap();
        assert_eq!(read.description, "short");
        assert_eq!(read.instructions, "");
    }

    #[tokio::test]
    async fn a_directory_without_a_manifest_is_not_a_skill() {
        let fs = InMemFs::new();
        fs.mkdir(Path::new("plain")).await.unwrap();
        assert_eq!(
            Skill::read(&fs, Path::new("plain"))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    /// The spelling this crate does not write, which is the one a host directory usually has.
    #[tokio::test]
    async fn the_uppercase_manifest_is_read_too() {
        let fs = InMemFs::new();
        fs.mkdir(Path::new("s")).await.unwrap();
        write_file(
            &fs,
            Path::new("s/SKILL.md"),
            b"---\nname: n\ndescription: d\n---\nb",
        )
        .await
        .unwrap();
        assert_eq!(Skill::read(&fs, Path::new("s")).await.unwrap().name, "n");
    }
}
