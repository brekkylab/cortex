use anyhow::Context as _;
use serde::{Deserialize, Deserializer, Serialize};

use super::Step;

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
pub struct Image {
    /// Written first and read first, so a declaration from a newer cortex is refused by name
    /// rather than by whatever member it happens to disagree about.
    #[serde(rename = "v", deserialize_with = "known_version")]
    version: u32,

    /// The base, as the caller spelled it.
    pub base: String,

    /// The steps, in the order they will run.
    pub steps: Vec<Step>,
}

impl Image {
    /// An empty declaration in this cortex's format: no base, no steps.
    ///
    /// The base starts empty, which is not a base — [`base`](Self::base) states one. Empty
    /// rather than a default like `scratch`, because a build over an unnamed base and a
    /// build over a deliberately empty one are different intentions, and only the caller
    /// knows which this is.
    ///
    /// ```
    /// # use cortex::image::{Image, Step};
    /// let declared = Image::new()
    ///     .base("alpine:3.20")
    ///     .step("apk add --no-cache jq")
    ///     .step(Step::env("TZ", "UTC"));
    ///
    /// assert_eq!(declared.base, "alpine:3.20");
    /// assert_eq!(declared.steps.len(), 2);
    /// ```
    pub fn new() -> Self {
        Image {
            version: FORMAT_VERSION,
            base: String::new(),
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
    /// # use cortex::image::{Image, Step};
    /// let declared = Image::from_dockerfile("FROM alpine:3.20\nRUN apk add jq\n")?;
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

        let mut declared = Image::new();
        for (line, text) in instructions {
            let (instruction, rest) = match text.split_once(char::is_whitespace) {
                Some((instruction, rest)) => (instruction.to_uppercase(), rest.trim()),
                None => (text.to_uppercase(), ""),
            };
            match instruction.as_str() {
                "FROM" => {
                    anyhow::ensure!(
                        declared.base.is_empty(),
                        "Dockerfile:{line}: a second FROM — this builds one stage only, and a \
                         multi-stage Dockerfile would silently build the last stage over the \
                         wrong base"
                    );
                    anyhow::ensure!(!rest.is_empty(), "Dockerfile:{line}: FROM names no image");
                    anyhow::ensure!(
                        !rest.contains(" AS ") && !rest.contains(" as "),
                        "Dockerfile:{line}: a named stage — this builds one stage only"
                    );
                    declared.base = rest.to_string();
                }
                // Before any FROM the base is still empty, which is not a base to put steps
                // over. Ordered after `FROM` so that the one instruction allowed to arrive
                // first is not caught by it.
                _ if declared.base.is_empty() => anyhow::bail!(
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
                    declared.steps.push(Step::Run(rest.to_string()));
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
                    declared.steps.push(Step::copy(parts[0], parts[1]));
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
                        declared.steps.push(Step::env(key, value));
                    }
                }
                "ENV" => {
                    let (key, value) = rest.split_once(char::is_whitespace).with_context(|| {
                        format!("Dockerfile:{line}: ENV names a variable and no value")
                    })?;
                    declared.steps.push(Step::env(key, value.trim()));
                }
                "WORKDIR" => {
                    anyhow::ensure!(
                        !rest.is_empty(),
                        "Dockerfile:{line}: WORKDIR names no directory"
                    );
                    declared.steps.push(Step::Workdir(rest.to_string()));
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
        anyhow::ensure!(
            !declared.base.is_empty(),
            "this Dockerfile has no FROM, so there is no base to build on"
        );
        Ok(declared)
    }

    /// The base to build over, named as a registry spells one.
    ///
    /// A tag works and a digest is better: a store that names this build by what it declares
    /// sees the string and not what the string resolved to, so a tag that moves keeps
    /// serving the image built before it moved.
    ///
    /// Stated once. Saying it again replaces it rather than stacking, because a build has
    /// one base and two `FROM`s are two builds.
    pub fn base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
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

/// The empty declaration, which is what [`new`](Image::new) hands back.
impl Default for Image {
    fn default() -> Self {
        Image::new()
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
