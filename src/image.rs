use std::{
    ffi::OsStr,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use serde::{Deserialize, Deserializer, Serialize};
use tokio::process::Command;

use crate::{
    cache_root,
    console::hang_up,
    protocol::{
        BuildImageCall, BuildImageResp, Client, Failure, RemoveImageCall, stdio::StdioClient,
    },
};

/// A client for building, listing and removing images on an image server.
///
/// ```no_run
/// use cortex::{
///     image::{ImageClient, Recipe},
///     protocol::stdio::StdioClient,
/// };
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// let server = tokio::process::Command::new("cortex-krun");
/// let mut images = ImageClient::try_from_client(StdioClient::new(server)?).await?;
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
/// Dropping it says `quit` and ends the channel.
pub struct ImageClient {
    client: Box<dyn Client>,
}

impl ImageClient {
    pub async fn try_new() -> Result<Self, Failure> {
        Self::try_from_cmd(&[cache_root().join("bin").join("cortex-krun")]).await
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
    /// Without one the server picks a ref. The returned digest, via [`ImageSource::digest`],
    /// runs on exactly this build.
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
    /// Say `quit`, so the server ends the session.
    fn drop(&mut self) {
        hang_up(&mut self.client, ());
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
    /// Resolved when the session asks, so a rebuilt ref names the newer build.
    Ref {
        #[serde(rename = "ref")]
        reference: String,
    },

    /// A build itself, as `algorithm:hex`, as `build` returns.
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

    /// Every ref pointing at this build, as `name:tag`; empty once all moved to later builds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
}

/// Which spelling of this format a declaration is written in.
///
/// Versions how a build is *written down*, not how it is named: a store digesting this type
/// may change its digest independently, without making either's stored values unreadable.
const FORMAT_VERSION: u32 = 1;

/// Defines how an image is to be composed, and stops there.
///
/// A base and the [`Step`]s over it. Console servers build it, each differently, so nothing
/// here resolves a base, writes a layer or runs a command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    /// Serialized first, so a declaration from a newer cortex is refused by version rather than
    /// by whichever member it disagrees on.
    #[serde(rename = "v", deserialize_with = "known_version")]
    version: u32,

    /// The base, as the caller spelled it.
    ///
    /// Never empty; a document with an empty base is refused on read, as there is no build
    /// without one.
    #[serde(deserialize_with = "some_base")]
    pub base: String,

    /// The steps, in the order they will run.
    pub steps: Vec<Step>,
}

impl Recipe {
    /// A declaration over `base`, with no steps yet.
    ///
    /// `base` is spelled as a registry does. A digest is better than a tag: a store naming this
    /// build by what it declares sees the string, not what it resolved to, so a moved tag keeps
    /// serving the image built before it moved.
    ///
    /// Required; an image built from nothing uses `scratch`, as in a Dockerfile.
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
    /// Takes text rather than a path, since its origin (a file, a request, a rendered template)
    /// is the caller's business; refusal line numbers point into the text given.
    ///
    /// One stage, and only `FROM`, `RUN`, `COPY`, `ENV` and `WORKDIR`. Anything else is refused
    /// at its line: an image with `CMD` or `USER` is not what this would build, and there is no
    /// side channel to report what was skipped.
    ///
    /// A `COPY` source stays as spelled, relative to a build context the builder supplies.
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
        // Lines into instructions: comments and blanks dropped, `\` continuations joined. A
        // comment inside a continuation is dropped without ending it, as `docker build` does
        // (annotated package lists rely on it). Collected first so a file ending
        // mid-continuation is one case here, not a second copy of the dispatch below.
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
        // Ended mid-continuation: keep it so it is still checked, not silently dropped.
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
                // No steps before a base. After the `FROM` arm, the one instruction allowed
                // first.
                _ if base.is_none() => anyhow::bail!(
                    "Dockerfile:{line}: {instruction} before any FROM — a build has to start \
                     from a base"
                ),
                "RUN" => {
                    // An empty one would run `sh -c ""` and exit 0, a do-nothing step that
                    // usually means a broken edited continuation, worth reporting.
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
                // `ENV k v`, or one or more `ENV k=v`; real Dockerfiles use both.
                //
                // The spelling is decided by the **first word only**: an `=` anywhere would
                // misread `ENV JAVA_OPTS -Dfoo=bar` (bare form, `=` in the value) as pairs.
                "ENV"
                    if rest
                        .split_whitespace()
                        .next()
                        .is_some_and(|w| w.contains('=')) =>
                {
                    // Split on whitespace outside quotes: `ENV MESSAGE="hello world"` is one
                    // pair, not a pair plus a bare `world"`.
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
                        // Strip one matching pair of surrounding quotes only:
                        // `ENV TZ="Asia/Seoul"` loses them, `ENV A=""x""` keeps the inner pair.
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
                // Anything else has no form here, and skipping it would yield a build other
                // than the one the Dockerfile describes.
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
    /// Takes anything convertible into a [`Step`], including a bare command:
    /// `.step("apk add jq")` is a `RUN`.
    pub fn step(mut self, step: impl Into<Step>) -> Self {
        self.steps.push(step.into());
        self
    }

    /// Append every step in `steps`, in order.
    pub fn steps(mut self, steps: impl IntoIterator<Item = impl Into<Step>>) -> Self {
        self.steps.extend(steps.into_iter().map(Into::into));
        self
    }
}

/// Refuse a version this cortex does not speak.
///
/// A missing version is refused too (by serde): it is not one of our declarations, and
/// assuming version 1 would invent its provenance.
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
/// The declared form, not the wire form (where `RUN` becomes an argv prefixed by the
/// accumulated environment and `ENV` becomes nothing), so a build is named by the digest of
/// what it *declares*: identical declarations get the same image.
///
/// Only these four; other Dockerfile instructions are refused rather than represented.
/// Serialized by name (`{"run": …}`, `{"copy": {…}}`), not position, so a new variant cannot
/// change what a stored [`Recipe`] means.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// A command, run through `sh -c` with the environment accumulated so far.
    Run(String),

    /// A path in the build context, copied into the image.
    ///
    /// `src` is relative to the context directory. `dst` is an absolute image path, a `String`
    /// because this host must not normalise a path in another filesystem.
    Copy { src: PathBuf, dst: String },

    /// A variable for every later [`Run`](Self::Run) and the built image's config.
    Env { key: String, value: String },

    /// Where later steps run, and the built image's working directory.
    Workdir(String),
}

impl Step {
    /// A command, run through `sh -c` with every earlier [`env`](Self::env).
    pub fn run(cmd: impl Into<String>) -> Self {
        Step::Run(cmd.into())
    }

    /// Something from the build context, copied into the image.
    ///
    /// `src` is relative to the context directory; an absolute one is refused at build time,
    /// since it lies outside the context.
    pub fn copy(src: impl AsRef<Path>, dst: impl Into<String>) -> Self {
        Step::Copy {
            src: src.as_ref().to_path_buf(),
            dst: dst.into(),
        }
    }

    /// A variable for every later [`run`](Self::run) and the built image's config.
    pub fn env(key: impl Into<String>, value: impl Into<String>) -> Self {
        Step::Env {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Where later steps run, and the built image's working directory.
    pub fn workdir(dir: impl Into<String>) -> Self {
        Step::Workdir(dir.into())
    }
}

/// A bare string is a `RUN`.
///
/// `RUN` is the only instruction that is a single string, so a lone command is unambiguous.
/// Other steps have no conversion (`("TZ", "UTC")` could be `ENV` or `COPY`); use
/// [`Step::env`], [`Step::copy`] and [`Step::workdir`].
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
    /// As the Dockerfile instruction, so build progress shows lines the caller recognises.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Run(command) => write!(f, "RUN {command}"),
            Step::Copy { src, dst } => write!(f, "COPY {} {dst}", src.display()),
            Step::Env { key, value } => write!(f, "ENV {key}={value}"),
            Step::Workdir(dir) => write!(f, "WORKDIR {dir}"),
        }
    }
}
