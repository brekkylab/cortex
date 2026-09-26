use std::{
    ffi::OsStr,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use serde::{Deserialize, Deserializer, Serialize};
use tokio::process::Command;

use crate::{
    console::hang_up,
    protocol::{
        BuildImageCall, BuildImageResp, Client, Failure, RemoveImageCall, stdio::StdioClient,
    },
    stdio_server_dir,
};

/// A client for managing the images.
///
/// ```no_run
/// use cortex::{
///     console::stdio::StdioClient,
///     image::{ImageClient, Recipe},
/// };
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// let server = tokio::process::Command::new("cortex-uvm-console");
/// let mut images = ImageClient::new(StdioClient::new(server)?).await?;
///
/// let built = images
///     .build(Recipe::new("alpine:3.20").step("apk add jq"), Some("myimg:latest"))
///     .await?;
/// println!("{} is {}", built.reference, built.digest);
///
/// for image in images.list().await? {
///     println!("{} {:?}", image.digest, image.refs);
/// }
///
/// images.remove(cortex::image::ImageSource::reference("myimg:latest")).await?;
/// # Ok(())
/// # }
/// ```
///
/// Dropping it ends the channel, the way dropping a `ConsoleClient` does.
pub struct ImageClient {
    client: Box<dyn Client>,
}

impl ImageClient {
    pub async fn try_new() -> Result<Self, Failure> {
        Self::try_from_cmd(&[stdio_server_dir().join("cortex-krun")]).await
    }

    pub async fn try_from_cmd(cmd: &[impl AsRef<OsStr>]) -> Result<Self, Failure> {
        let (program, args) = cmd
            .split_first()
            .ok_or_else(|| Failure::broken("an image server needs a program to run"))?;

        let mut server = Command::new(program);
        server.args(args);

        let client = StdioClient::new(server)
            .context("starting the image server")
            .map_err(Failure::Broken)?;
        Self::try_from_client(client).await
    }

    pub async fn try_from_client(client: impl Client + 'static) -> Result<Self, Failure> {
        let mut client: Box<dyn Client> = Box::new(client);
        client.version().await?;
        Ok(ImageClient { client })
    }

    /// Which protocol version the server speaks.
    pub async fn version(&mut self) -> Result<String, Failure> {
        self.client.version().await.map(|answer| answer.version)
    }

    /// Build `recipe`, and store it under `reference` if one is given.
    ///
    /// Without one the server picks a ref. Either way the ref and the digest come back, and
    /// the digest is what [`ImageSource::digest`] takes to run on exactly this build.
    pub async fn build(
        &mut self,
        recipe: Recipe,
        reference: Option<&str>,
    ) -> Result<BuildImageResp, Failure> {
        let build = BuildImageCall {
            recipe,
            reference: reference.map(str::to_string),
        };
        self.client.build_image(build).await
    }

    /// Every image the server has built.
    pub async fn list(&mut self) -> Result<Vec<ImageEntry>, Failure> {
        self.client.list_images().await.map(|answer| answer.images)
    }

    /// Remove a built image, named by its ref or its digest.
    pub async fn remove(&mut self, image: impl Into<ImageSource>) -> Result<(), Failure> {
        let remove = RemoveImageCall {
            image: image.into(),
        };
        self.client.remove_image(remove).await.map(|_| ())
    }
}

impl Drop for ImageClient {
    /// Say `quit`, as [`ConsoleClient`](crate::console::ConsoleClient) does when it is dropped.
    fn drop(&mut self) {
        hang_up(&mut self.client);
    }
}

/// Specifies an image.
///
/// An image can be specified in three ways.
///
/// - [`Recipe`](Self::Recipe) gives a base image and the steps over it
/// - [`Ref`](Self::Ref) names an image by its reference, as `name:tag`
/// - [`Digest`](Self::Digest) names an image by its digest, as `algorithm:hex`
///
/// ## Example
///
/// ```
/// # use cortex::image::{ImageSource, Recipe};
/// let recipe: ImageSource = Recipe::new("alpine:3.20").step("apk add jq").into();
/// let reference = ImageSource::reference("myimg:latest");
/// let digest = ImageSource::digest("sha256:0123abcd");
/// ```
///
/// ## Serialization
///
/// The kind is named by `type`, and what it carries is under a key of the same name.
///
/// ```json
/// {"type": "recipe", "recipe": {"v": 1, "base": "alpine:3.20", "steps": [{"run": "apk add jq"}]}}
/// {"type": "ref", "ref": "myimg:latest"}
/// {"type": "digest", "digest": "sha256:0123abcd"}
/// ```
///
/// ## Notes
///
/// This only specifies an image and does not build one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// A declaration to build, then run on.
    Recipe { recipe: Recipe },

    /// The name a build was stored under, as `name:tag`.
    ///
    /// Resolved when the session asks for it, so a ref that has since been built again
    /// names the newer build.
    Ref {
        #[serde(rename = "ref")]
        reference: String,
    },

    /// A build itself, as `algorithm:hex`, which is what a `build` answers with.
    Digest { digest: String },
}

impl ImageSource {
    /// A build looked up by the name it was stored under.
    pub fn reference(reference: impl Into<String>) -> Self {
        ImageSource::Ref {
            reference: reference.into(),
        }
    }

    /// A build looked up by its digest.
    pub fn digest(digest: impl Into<String>) -> Self {
        ImageSource::Digest {
            digest: digest.into(),
        }
    }
}

impl From<Recipe> for ImageSource {
    fn from(recipe: Recipe) -> Self {
        ImageSource::Recipe { recipe }
    }
}

/// A list of images, as `list_images` answers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageEntries {
    pub images: Vec<ImageEntry>,
}

/// One built image in an [`ImageEntries`].
///
/// A build is named by its digest, and any number of refs may point at it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageEntry {
    /// The build, as `algorithm:hex`.
    pub digest: String,

    /// Every ref that points at this build, as `name:tag`. Empty once every ref it had has
    /// moved to a later build.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
}

/// Which spelling of this format a declaration is written in.
///
/// About how a build is *written down* and not about what it is called: a store that names
/// images by a digest of this type is free to move its digest independently, and neither
/// change need make the other's stored values unreadable.
const FORMAT_VERSION: u32 = 1;

/// Defines how an image is to be composed, and stops there.
///
/// The definition is a base and the [`Step`]s over it. Building what it describes is each
/// console server's, and they do not agree on how, so nothing here resolves a base, writes
/// a layer or runs a command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    /// Written first and read first, so a declaration from a newer cortex is refused by name
    /// rather than by whatever member it happens to disagree about.
    #[serde(rename = "v", deserialize_with = "known_version")]
    version: u32,

    /// The base, as the caller spelled it.
    ///
    /// Never empty. A document with an empty one is refused when it is read, for the reason
    /// [`new`](Self::new) takes one.
    #[serde(deserialize_with = "some_base")]
    pub base: String,

    /// The steps, in the order they will run.
    pub steps: Vec<Step>,
}

impl Recipe {
    /// A declaration over `base`, with no steps yet.
    ///
    /// The base is named as a registry spells one. A tag works and a digest is better: a
    /// store that names this build by what it declares sees the string and not what the
    /// string resolved to, so a tag that moves keeps serving the image built before it moved.
    ///
    /// Required, because there is no build without one. An image built from nothing says
    /// so with `scratch`, the way a Dockerfile does.
    ///
    /// ```
    /// # use cortex::image::{Recipe, Step};
    /// let declared = Recipe::new("alpine:3.20")
    ///     .step("apk add --no-cache jq")
    ///     .step(Step::env("TZ", "UTC"));
    ///
    /// assert_eq!(declared.base, "alpine:3.20");
    /// assert_eq!(declared.steps.len(), 2);
    /// ```
    pub fn new(base: impl Into<String>) -> Self {
        Recipe {
            version: FORMAT_VERSION,
            base: base.into(),
            steps: Vec::new(),
        }
    }

    /// A declaration read from what a Dockerfile says.
    ///
    /// The text rather than a path to it, because where those bytes came from is the
    /// caller's business: a file beside a build context, a string in a request, a template
    /// something else just rendered. A line number in a refusal still points into what was
    /// handed over, which is the part this can be sure of.
    ///
    /// One stage, and the five instructions a declaration has: `FROM`, `RUN`, `COPY`, `ENV`
    /// and `WORKDIR`. Anything else is refused at the line that said it — an image with a
    /// `CMD` or a `USER` in it is not the image this would build, and the value handed back
    /// here is the whole of the answer, with no second channel for what was passed over.
    ///
    /// A `COPY` source stays as the Dockerfile spelled it, relative to a build context this
    /// does not carry — whoever builds this names the directory it is read from.
    ///
    /// ```
    /// # use cortex::image::{Recipe, Step};
    /// let declared = Recipe::from_dockerfile("FROM alpine:3.20\nRUN apk add jq\n")?;
    ///
    /// assert_eq!(declared.base, "alpine:3.20");
    /// assert_eq!(declared.steps, [Step::run("apk add jq")]);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn from_dockerfile(content: impl AsRef<str>) -> anyhow::Result<Self> {
        // What the text says, as instructions rather than as lines: comments and blanks
        // dropped, and a `\` continuation joined onto the line it continues. A comment
        // *inside* a continuation goes too and does not end it, which is what `docker build`
        // does and what a Dockerfile with an annotated package list relies on. Collected
        // first so that a file ending mid-continuation is one case here instead of the whole
        // dispatch below written twice.
        let mut instructions: Vec<(usize, String)> = Vec::new();
        let mut current: Option<(usize, String)> = None;
        for (index, raw) in content.as_ref().lines().enumerate() {
            let trimmed = raw.trim();
            if trimmed.starts_with('#') || (trimmed.is_empty() && current.is_none()) {
                continue;
            }
            let (body, continues) = match trimmed.strip_suffix('\\') {
                Some(body) => (body.trim_end(), true),
                None => (trimmed, false),
            };
            match current.as_mut() {
                Some((_, text)) if !body.is_empty() => {
                    text.push(' ');
                    text.push_str(body);
                }
                Some(_) => {}
                None => current = Some((index + 1, body.to_string())),
            }
            if !continues
                && let Some(instruction) = current.take()
                && !instruction.1.is_empty()
            {
                instructions.push(instruction);
            }
        }
        // Text that ends mid-continuation: take what there is rather than dropping it
        // silently, so the instruction is still checked and still refused if it is one this
        // does not have.
        if let Some(instruction) = current.take() {
            instructions.push(instruction);
        }

        let mut base: Option<String> = None;
        let mut steps: Vec<Step> = Vec::new();
        for (line, text) in instructions {
            let (instruction, rest) = match text.split_once(char::is_whitespace) {
                Some((instruction, rest)) => (instruction.to_uppercase(), rest.trim()),
                None => (text.to_uppercase(), ""),
            };
            match instruction.as_str() {
                "FROM" => {
                    anyhow::ensure!(
                        base.is_none(),
                        "Dockerfile:{line}: a second FROM — this builds one stage only, and a \
                         multi-stage Dockerfile would silently build the last stage over the \
                         wrong base"
                    );
                    anyhow::ensure!(!rest.is_empty(), "Dockerfile:{line}: FROM names no image");
                    anyhow::ensure!(
                        !rest.contains(" AS ") && !rest.contains(" as "),
                        "Dockerfile:{line}: a named stage — this builds one stage only"
                    );
                    base = Some(rest.to_string());
                }
                // Before any FROM the base is still empty, which is not a base to put steps
                // over. Ordered after `FROM` so that the one instruction allowed to arrive
                // first is not caught by it.
                _ if base.is_none() => anyhow::bail!(
                    "Dockerfile:{line}: {instruction} before any FROM — a build has to start \
                     from a base"
                ),
                "RUN" => {
                    // Checked like `FROM` and `WORKDIR` are. An empty one goes out as
                    // `sh -c ""` and exits 0, so it would take a place in what this declares
                    // without doing anything — and the way a Dockerfile grows one is an
                    // edited continuation, which is precisely the case worth being told
                    // about.
                    anyhow::ensure!(!rest.is_empty(), "Dockerfile:{line}: RUN names no command");
                    steps.push(Step::Run(rest.to_string()));
                }
                "COPY" => {
                    anyhow::ensure!(
                        !rest.starts_with("--"),
                        "Dockerfile:{line}: COPY with a flag ({rest}) — this translates \
                         `COPY <src> <dst>` and nothing else"
                    );
                    let parts: Vec<&str> = rest.split_whitespace().collect();
                    anyhow::ensure!(
                        parts.len() == 2,
                        "Dockerfile:{line}: COPY takes one source and one destination here, \
                         and this has {}",
                        parts.len()
                    );
                    steps.push(Step::copy(parts[0], parts[1]));
                }
                // `ENV k v`, or one or more `ENV k=v`. Both spellings, because Dockerfiles in
                // the wild use both and refusing one would make the claim this makes — that
                // it reads a Dockerfile — false for a large share of them.
                //
                // Which spelling it is comes from the **first word only**. Looking for an `=`
                // anywhere in the line reads `ENV JAVA_OPTS -Dfoo=bar` — the bare form, whose
                // value happens to contain one — as the pair form, and then refuses it for
                // naming no key.
                "ENV"
                    if rest
                        .split_whitespace()
                        .next()
                        .is_some_and(|w| w.contains('=')) =>
                {
                    // Split on whitespace, except inside quotes: `ENV MESSAGE="hello world"`
                    // is one pair and not two words, and a split that did not know about the
                    // quotes would refuse `world"` as a bare word — for a spelling the
                    // Dockerfile did not use.
                    let mut pairs: Vec<String> = Vec::new();
                    let mut pair = String::new();
                    let mut quote: Option<char> = None;
                    for c in rest.chars() {
                        match quote {
                            Some(q) => {
                                quote = (c != q).then_some(q);
                                pair.push(c);
                            }
                            None if c == '"' || c == '\'' => {
                                quote = Some(c);
                                pair.push(c);
                            }
                            None if c.is_whitespace() => {
                                if !pair.is_empty() {
                                    pairs.push(std::mem::take(&mut pair));
                                }
                            }
                            None => pair.push(c),
                        }
                    }
                    if !pair.is_empty() {
                        pairs.push(pair);
                    }
                    for pair in pairs {
                        let (key, value) = pair.split_once('=').with_context(|| {
                            format!(
                                "Dockerfile:{line}: ENV mixes `k=v` pairs with a bare word \
                                 ({pair}), which is two spellings in one instruction"
                            )
                        })?;
                        // One matching pair of surrounding quotes taken off, and not every
                        // quote: `ENV TZ="Asia/Seoul"` is one value rather than a value with
                        // quotes in it, while `ENV A=""x""` keeps the pair it is left with.
                        let value = value
                            .strip_prefix('"')
                            .and_then(|v| v.strip_suffix('"'))
                            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                            .unwrap_or(value);
                        steps.push(Step::env(key, value));
                    }
                }
                "ENV" => {
                    let (key, value) = rest.split_once(char::is_whitespace).with_context(|| {
                        format!("Dockerfile:{line}: ENV names a variable and no value")
                    })?;
                    steps.push(Step::env(key, value.trim()));
                }
                "WORKDIR" => {
                    anyhow::ensure!(
                        !rest.is_empty(),
                        "Dockerfile:{line}: WORKDIR names no directory"
                    );
                    steps.push(Step::Workdir(rest.to_string()));
                }
                // Everything else, refused where it was written. A declaration is a base and
                // these five, so an instruction outside them has no form here — and one
                // passed over quietly would leave the caller holding a build that is not the
                // one their Dockerfile describes.
                other => anyhow::bail!(
                    "Dockerfile:{line}: {other} is not one of FROM, RUN, COPY, ENV and \
                     WORKDIR, which is all a declaration has"
                ),
            }
        }
        let base = base.context("this Dockerfile has no FROM, so there is no base to build on")?;
        Ok(Recipe::new(base).steps(steps))
    }

    /// One step, after everything declared so far.
    ///
    /// Takes anything that converts into a [`Step`], which is every step spelled out and a
    /// bare command besides — `.step("apk add jq")` is the `RUN` it reads as.
    pub fn step(mut self, step: impl Into<Step>) -> Self {
        self.steps.push(step.into());
        self
    }

    /// Every step in `steps`, appended in the order they arrive.
    ///
    /// The same thing [`step`](Self::step) does, said once for a sequence a caller already
    /// has — steps read back from somewhere, or built up before there was anything to put
    /// them on.
    pub fn steps(mut self, steps: impl IntoIterator<Item = impl Into<Step>>) -> Self {
        self.steps.extend(steps.into_iter().map(Into::into));
        self
    }
}

/// Refuse a version this cortex does not speak.
///
/// A missing member is refused too, and by serde rather than here: a document with no
/// version is not a declaration of ours, and guessing that it means version 1 would be
/// inventing a provenance for something whose provenance is the whole question.
fn known_version<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let found = u32::deserialize(d)?;
    if found != FORMAT_VERSION {
        return Err(serde::de::Error::custom(format!(
            "this image is written in format {found}, and this cortex reads {FORMAT_VERSION}"
        )));
    }
    Ok(found)
}

/// Refuse an empty base, which is not a base to build on.
fn some_base<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let base = String::deserialize(d)?;
    if base.is_empty() {
        return Err(serde::de::Error::custom(
            "this image names no base, and there is no build without one",
        ));
    }
    Ok(base)
}

/// One instruction of a build.
///
/// A step is what the caller declared, not what went on the wire: `RUN` becomes an argv with
/// the accumulated environment in front of it, and `ENV` becomes nothing at all. Keeping the
/// declared form is what lets a build be named by the digest of what it *declares* — two
/// callers who declared the same thing get the same image whatever the wire did.
///
/// The four the design settled on, and no others. Anything a Dockerfile can say that is not
/// one of these is warned about or refused by the adapter rather than represented here.
/// Written under its own name — `{"run": …}`, `{"copy": {…}}` — rather than by position, so
/// a variant added later cannot change what a stored [`Recipe`](Recipe) means.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// A command, run through `sh -c` with the environment accumulated so far.
    Run(String),

    /// A path in the build context, copied into the image.
    ///
    /// `src` is relative to the context directory; `dst` is an absolute path in the image
    /// and is a `String` rather than a `PathBuf` because it names a place in someone else's
    /// filesystem, which this host has no business normalising.
    Copy { src: PathBuf, dst: String },

    /// A variable, added to every later [`Run`](Self::Run) and to what the built image
    /// states.
    Env { key: String, value: String },

    /// Where later steps run, and what the built image states.
    Workdir(String),
}

impl Step {
    /// A command, run through `sh -c` with everything an earlier [`env`](Self::env) said.
    pub fn run(cmd: impl Into<String>) -> Self {
        Step::Run(cmd.into())
    }

    /// Something from the build context, copied into the image.
    ///
    /// `src` is relative to the context directory; an absolute one is refused when the build
    /// runs, because it names a place the context does not contain and so is not part of
    /// what this build declared.
    pub fn copy(src: impl AsRef<Path>, dst: impl Into<String>) -> Self {
        Step::Copy {
            src: src.as_ref().to_path_buf(),
            dst: dst.into(),
        }
    }

    /// A variable, for every later [`run`](Self::run) and for what the image states.
    pub fn env(key: impl Into<String>, value: impl Into<String>) -> Self {
        Step::Env {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Where later steps run, and what the image states.
    pub fn workdir(dir: impl Into<String>) -> Self {
        Step::Workdir(dir.into())
    }
}

/// A bare string is a `RUN`.
///
/// [`Recipe::step`](Recipe::step) takes anything that converts, so this is what
/// decides that `.step("apk add jq")` compiles and what it means. `RUN` is the one
/// instruction whose entire declaration *is* a single string — the other three name two
/// pieces or a place — so there is nothing else a lone command could have been read as, and
/// nothing for the reader to look up.
///
/// No conversion for the others, for the same reason: `("TZ", "UTC")` could be an `ENV` or a
/// `COPY` with equal grammar, and a conversion that picked one would be picking it silently.
/// Those are spelled [`Step::env`], [`Step::copy`] and [`Step::workdir`].
impl From<&str> for Step {
    fn from(command: &str) -> Self {
        Step::run(command)
    }
}

impl From<String> for Step {
    fn from(command: String) -> Self {
        Step::Run(command)
    }
}

impl fmt::Display for Step {
    /// As the instruction it came from. A caller showing a build's progress hands this on,
    /// and the line the caller wrote is what it will recognise.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Run(command) => write!(f, "RUN {command}"),
            Step::Copy { src, dst } => write!(f, "COPY {} {dst}", src.display()),
            Step::Env { key, value } => write!(f, "ENV {key}={value}"),
            Step::Workdir(dir) => write!(f, "WORKDIR {dir}"),
        }
    }
}
