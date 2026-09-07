//! A Dockerfile, read as a list of steps.
//!
//! A translator with nothing behind it: a Dockerfile and the hand-written builder it
//! corresponds to produce the same [`BuildId`](super::BuildId). There is a test for exactly
//! that, and it is the claim this module lives or dies by.
//!
//! # Three ways an instruction is treated
//!
//! | instruction | what happens |
//! |---|---|
//! | `FROM`, `RUN`, `COPY`, `ENV`, `WORKDIR` | translated to the builder call it corresponds to |
//! | `ENTRYPOINT`, `CMD`, `USER`, `ARG` | warned about and skipped |
//! | anything else | refused, by name and line number |
//!
//! # Two of the four skipped are not inert
//!
//! `ENTRYPOINT` and `CMD` genuinely mean nothing here — a cortex session runs the commands
//! its client sends it, there is no default command for an image to carry, and nothing
//! downstream would read one.
//!
//! The other two change what the build would have produced:
//!
//! - **`USER`** decides who the steps after it run as, and therefore what owns the files
//!   they write. Skipping it runs them as root.
//! - **`ARG`** declares a variable later instructions interpolate. This adapter does no
//!   substitution at all, so a `${FOO}` in a later `RUN` reaches `sh -c` and expands to
//!   nothing.
//!
//! Their warnings say that. Adding substitution, or a `USER` the protocol could honour, are
//! both real features and neither is here — until they are, the warning is the whole of the
//! honesty available.

use std::path::Path;

use anyhow::Context as _;

use super::{Rootfs, Step, Warning};

/// What a Dockerfile said, in the terms this crate has.
///
/// `Debug`, because a test that expects a refusal asks for one with `unwrap_err`, and that
/// wants to be able to print what came back instead.
#[derive(Debug)]
pub(crate) struct Parsed {
    pub base: String,
    pub steps: Vec<Step>,
    pub warnings: Vec<Warning>,
}

/// Instructions that are skipped, and what skipping each one means for the build.
///
/// A table rather than a match arm apiece, because the four differ only in their message and
/// the point of the message is that it is not boilerplate.
const SKIPPED: &[(&str, &str)] = &[
    (
        "ARG",
        "is not applied — this build does no variable substitution, so a `${…}` in a later \
         instruction expands to the empty string",
    ),
    (
        "CMD",
        "is not applied — a session runs the commands its client sends it, so an image \
         carries no default command",
    ),
    (
        "ENTRYPOINT",
        "is not applied — a session runs the commands its client sends it, so an image \
         carries no entrypoint",
    ),
    (
        "USER",
        "is not applied — the steps after it run as root, and root will own the files they \
         write",
    ),
];

impl Rootfs {
    /// A build read from a Dockerfile, and what reading it skipped.
    ///
    /// The [`context`](Self::context) defaults to the Dockerfile's own directory, which is
    /// what a caller with a `COPY app /srv/app` beside their Dockerfile means. Override it
    /// with `context` if the tree is somewhere else.
    ///
    /// The [`Warning`]s are returned rather than printed, because where a diagnostic goes is
    /// the caller's to decide — a library reaching for stderr on its own has decided for
    /// them, and one reaching for it *as well* has said the same thing twice. They are empty
    /// for a Dockerfile that said nothing this build does not act on.
    ///
    /// ```no_run
    /// # use cortex::rootfs::Rootfs;
    /// # fn f() -> anyhow::Result<()> {
    /// let (recipe, skipped) = Rootfs::from_dockerfile("app/Dockerfile")?;
    /// for warning in &skipped {
    ///     eprintln!("{warning}");
    /// }
    /// # Ok(()) }
    /// ```
    pub fn from_dockerfile(path: impl AsRef<Path>) -> anyhow::Result<(Self, Vec<Warning>)> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the Dockerfile at {}", path.display()))?;
        let parsed = parse(&text)?;

        let mut rootfs = Rootfs::from_image(parsed.base);
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            rootfs = rootfs.context(dir.to_path_buf());
        }
        for step in parsed.steps {
            rootfs = match step {
                Step::Run(command) => rootfs.run(command),
                Step::Copy { src, dst } => rootfs.copy(src, dst),
                Step::Env { key, value } => rootfs.env(key, value),
                Step::Workdir(dir) => rootfs.workdir(dir),
            };
        }
        Ok((rootfs, parsed.warnings))
    }
}

/// One logical instruction: what it says, and the line it started on.
struct Logical {
    line: usize,
    text: String,
}

/// Join continuations, drop comments and blanks.
///
/// A comment *inside* a continuation is dropped too and does not end it, which is what
/// `docker build` does and what a Dockerfile with an annotated package list relies on.
fn logical_lines(text: &str) -> Vec<Logical> {
    let mut lines = Vec::new();
    let mut current: Option<Logical> = None;

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw.trim();

        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed.is_empty() && current.is_none() {
            continue;
        }

        let (body, continues) = match trimmed.strip_suffix('\\') {
            Some(body) => (body.trim_end(), true),
            None => (trimmed, false),
        };

        match current.as_mut() {
            Some(logical) => {
                if !body.is_empty() {
                    logical.text.push(' ');
                    logical.text.push_str(body);
                }
            }
            None => {
                current = Some(Logical {
                    line,
                    text: body.to_string(),
                });
            }
        }

        if !continues
            && let Some(logical) = current.take()
            && !logical.text.is_empty()
        {
            lines.push(logical);
        }
    }

    // A file ending mid-continuation: take what there is rather than dropping it silently,
    // so the instruction is still checked and still refused if it is one this cannot do.
    if let Some(logical) = current.take() {
        lines.push(logical);
    }
    lines
}

/// Read `text` as a Dockerfile.
pub(crate) fn parse(text: &str) -> anyhow::Result<Parsed> {
    let mut base: Option<String> = None;
    let mut steps = Vec::new();
    let mut warnings = Vec::new();

    for Logical { line, text } in logical_lines(text) {
        let (instruction, rest) = match text.split_once(char::is_whitespace) {
            Some((instruction, rest)) => (instruction.to_uppercase(), rest.trim()),
            None => (text.to_uppercase(), ""),
        };

        if let Some((_, message)) = SKIPPED.iter().find(|(name, _)| *name == instruction) {
            warnings.push(Warning {
                line,
                instruction,
                message: (*message).to_string(),
            });
            continue;
        }

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
            _ if base.is_none() => anyhow::bail!(
                "Dockerfile:{line}: {instruction} before any FROM — a build has to start \
                 from a base"
            ),
            "RUN" => {
                // Checked like `FROM` and `WORKDIR` are. An empty one goes out as `sh -c ""`
                // and exits 0, so it would take a place in the build's id without doing
                // anything — and the way a Dockerfile grows one is an edited continuation,
                // which is precisely the case worth being told about.
                anyhow::ensure!(!rest.is_empty(), "Dockerfile:{line}: RUN names no command");
                steps.push(Step::Run(rest.to_string()));
            }
            "COPY" => steps.push(copy(line, rest)?),
            "ENV" => steps.extend(env(line, rest)?),
            "WORKDIR" => {
                anyhow::ensure!(
                    !rest.is_empty(),
                    "Dockerfile:{line}: WORKDIR names no directory"
                );
                steps.push(Step::Workdir(rest.to_string()));
            }
            other => anyhow::bail!(
                "Dockerfile:{line}: {other} is not an instruction this can build from"
            ),
        }
    }

    Ok(Parsed {
        base: base.context("this Dockerfile has no FROM, so there is no base to build on")?,
        steps,
        warnings,
    })
}

/// `COPY src dst`, and nothing else.
fn copy(line: usize, rest: &str) -> anyhow::Result<Step> {
    anyhow::ensure!(
        !rest.starts_with("--"),
        "Dockerfile:{line}: COPY with a flag ({rest}) — this translates `COPY <src> <dst>` \
         and nothing else"
    );
    let parts: Vec<&str> = rest.split_whitespace().collect();
    anyhow::ensure!(
        parts.len() == 2,
        "Dockerfile:{line}: COPY takes one source and one destination here, and this has {}",
        parts.len()
    );
    Ok(Step::Copy {
        src: parts[0].into(),
        dst: parts[1].to_string(),
    })
}

/// `ENV k v`, or one or more `ENV k=v`.
///
/// Both spellings, because Dockerfiles in the wild use both and refusing one would make the
/// adapter's claim — that it translates a Dockerfile — false for a large share of them.
///
/// Which spelling it is comes from the **first word only**. Looking for an `=` anywhere in
/// the line reads `ENV JAVA_OPTS -Dfoo=bar` — the bare form, whose value happens to contain
/// one — as the pair form, and then refuses it for naming no key.
fn env(line: usize, rest: &str) -> anyhow::Result<Vec<Step>> {
    let first = rest.split_whitespace().next().unwrap_or_default();
    if first.contains('=') {
        let pairs = split_quoted(rest);
        return pairs
            .iter()
            .map(|pair| {
                let (key, value) = pair.split_once('=').with_context(|| {
                    format!(
                        "Dockerfile:{line}: ENV mixes `k=v` pairs with a bare word ({pair}), \
                         which is two spellings in one instruction"
                    )
                })?;
                Ok(Step::Env {
                    key: key.to_string(),
                    // A quoted value, unquoted. `ENV TZ="Asia/Seoul"` is one value and not a
                    // value with quotes in it.
                    value: unquoted(value),
                })
            })
            .collect();
    }

    let (key, value) = rest
        .split_once(char::is_whitespace)
        .with_context(|| format!("Dockerfile:{line}: ENV names a variable and no value"))?;
    Ok(vec![Step::Env {
        key: key.to_string(),
        value: value.trim().to_string(),
    }])
}

/// Split on whitespace, except inside quotes.
///
/// `ENV MESSAGE="hello world"` is one pair and not two words, so a split that did not know
/// about the quotes would hand `world"` to the pair reader and have it refused as a bare
/// word — for a spelling the Dockerfile did not use.
/// The quotes stay in the token: what a quote *means* is [`unquoted`]'s to say, and two
/// places deciding it separately is how they come to disagree.
fn split_quoted(rest: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for c in rest.chars() {
        match quote {
            Some(open) if c == open => {
                quote = None;
                current.push(c);
            }
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                current.push(c);
            }
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// A value with one matching pair of surrounding quotes taken off.
///
/// One pair, not every quote: `trim_matches` would turn `""x""` into `x` and an empty
/// `ENV A="` into nothing at all.
fn unquoted(value: &str) -> String {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner.to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Dockerfile on disk, in a directory of its own.
    fn written(text: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a directory");
        std::fs::write(dir.path().join("Dockerfile"), text).expect("writing it");
        dir
    }

    /// What was skipped comes back to the caller, rather than being printed at them. A
    /// library that prints has decided where a diagnostic goes; this one has not.
    #[test]
    fn from_dockerfile_hands_back_what_it_skipped() {
        let dir = written("FROM alpine\nUSER node\nRUN true\n");
        let (rootfs, skipped) = Rootfs::from_dockerfile(dir.path().join("Dockerfile"))
            .expect("a Dockerfile this can read");

        assert_eq!(rootfs.base(), "alpine");
        assert_eq!(rootfs.steps(), [Step::Run("true".into())]);

        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(skipped[0].instruction, "USER");
        assert_eq!(skipped[0].line, 2);
    }

    /// And a Dockerfile with nothing to skip hands back nothing — an empty `Vec`, not a
    /// silence the caller has to interpret.
    #[test]
    fn a_dockerfile_with_nothing_to_skip_warns_about_nothing() {
        let dir = written("FROM alpine\nRUN true\n");
        let (_, skipped) =
            Rootfs::from_dockerfile(dir.path().join("Dockerfile")).expect("a clean Dockerfile");
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    /// The context defaults to the Dockerfile's own directory, which is what a `COPY app …`
    /// beside it means.
    ///
    /// Asserted through `id`, which is what actually depends on it: the id hashes what every
    /// `COPY` reads, so it can only be answered if the source resolves — and `app` exists
    /// beside the Dockerfile and nowhere near this test's working directory.
    #[test]
    fn the_context_defaults_to_the_dockerfiles_own_directory() {
        let dir = written("FROM alpine\nCOPY app /srv/app\n");
        std::fs::write(dir.path().join("app"), b"x").expect("something to copy");

        let (rootfs, _) =
            Rootfs::from_dockerfile(dir.path().join("Dockerfile")).expect("a Dockerfile");
        rootfs
            .id()
            .expect("the COPY source resolved against the Dockerfile's directory");
    }

    /// The five that are translated, each to the call it corresponds to.
    #[test]
    fn the_five_become_steps() {
        let parsed = parse(
            "FROM alpine:3.20\n\
             RUN apk add jq\n\
             COPY app /srv/app\n\
             ENV TZ UTC\n\
             WORKDIR /srv/app\n",
        )
        .unwrap();

        assert_eq!(parsed.base, "alpine:3.20");
        assert_eq!(
            parsed.steps,
            [
                Step::Run("apk add jq".into()),
                Step::Copy {
                    src: "app".into(),
                    dst: "/srv/app".into()
                },
                Step::Env {
                    key: "TZ".into(),
                    value: "UTC".into()
                },
                Step::Workdir("/srv/app".into()),
            ]
        );
        assert!(parsed.warnings.is_empty());
    }

    /// `ENV k=v` is the other spelling, and several pairs on one line is a third.
    #[test]
    fn env_takes_both_spellings() {
        let parsed = parse("FROM alpine\nENV TZ=UTC LANG=C\nENV PATH /usr/bin\n").unwrap();
        assert_eq!(
            parsed.steps,
            [
                Step::Env {
                    key: "TZ".into(),
                    value: "UTC".into()
                },
                Step::Env {
                    key: "LANG".into(),
                    value: "C".into()
                },
                Step::Env {
                    key: "PATH".into(),
                    value: "/usr/bin".into()
                },
            ]
        );
    }

    /// A line ending in a backslash carries on, and the instruction is what they make
    /// together.
    #[test]
    fn a_continuation_is_one_instruction() {
        let parsed = parse("FROM alpine\nRUN apk add \\\n    jq \\\n    curl\n").unwrap();
        assert_eq!(parsed.steps, [Step::Run("apk add jq curl".into())]);
    }

    /// Comments and blank lines are not instructions, including inside a continuation.
    #[test]
    fn comments_and_blanks_are_not_instructions() {
        let parsed = parse(
            "# what this builds\n\
             FROM alpine\n\
             \n\
             RUN apk add \\\n\
             # the one we need\n\
                 jq\n",
        )
        .unwrap();
        assert_eq!(parsed.steps, [Step::Run("apk add jq".into())]);
    }

    /// An instruction is recognised whatever its case, which is how Dockerfiles are read.
    #[test]
    fn an_instruction_is_case_insensitive() {
        let parsed = parse("from alpine\nrun true\n").unwrap();
        assert_eq!(parsed.base, "alpine");
        assert_eq!(parsed.steps, [Step::Run("true".into())]);
    }

    /// The four that are skipped, each with exactly one warning naming it and its line.
    #[test]
    fn the_four_are_warned_about_and_skipped() {
        let parsed = parse(
            "FROM alpine\n\
             ARG VERSION=1\n\
             USER node\n\
             RUN true\n\
             ENTRYPOINT [\"/bin/sh\"]\n\
             CMD [\"-c\", \"true\"]\n",
        )
        .unwrap();

        assert_eq!(parsed.steps, [Step::Run("true".into())]);
        let named: Vec<_> = parsed
            .warnings
            .iter()
            .map(|w| (w.line, w.instruction.as_str()))
            .collect();
        assert_eq!(
            named,
            [(2, "ARG"), (3, "USER"), (5, "ENTRYPOINT"), (6, "CMD")]
        );
    }

    /// The two that change what would have been built say so. A warning that read like the
    /// inert ones is how a silently-wrong image gets made.
    #[test]
    fn the_two_that_are_not_inert_say_what_happens_instead() {
        let parsed = parse("FROM alpine\nUSER node\nARG V\n").unwrap();
        let user = &parsed.warnings[0];
        assert!(
            user.message.contains("root"),
            "the USER warning does not say the steps run as root: {}",
            user.message
        );
        let arg = &parsed.warnings[1];
        assert!(
            arg.message.contains("empty"),
            "the ARG warning does not say substitution does not happen: {}",
            arg.message
        );
    }

    /// Anything else is refused, by name and line.
    #[test]
    fn anything_else_is_refused_by_name_and_line() {
        let refused = parse("FROM alpine\nRUN true\nHEALTHCHECK CMD true\n").unwrap_err();
        let said = refused.to_string();
        assert!(said.contains("HEALTHCHECK"), "{said}");
        assert!(said.contains('3'), "the line is not named: {said}");
    }

    /// A Dockerfile has to start somewhere.
    #[test]
    fn a_dockerfile_without_from_is_refused() {
        assert!(
            parse("RUN true\n")
                .unwrap_err()
                .to_string()
                .contains("FROM")
        );
    }

    /// One stage only. A second `FROM` is a multi-stage build, which this does not do — and
    /// silently building the last stage over the wrong base is worse than saying so.
    #[test]
    fn a_second_from_is_refused() {
        let refused = parse("FROM alpine\nRUN true\nFROM debian\n").unwrap_err();
        assert!(refused.to_string().contains("one stage"), "{refused}");
    }

    /// `COPY` takes exactly two paths here. `--from` is a multi-stage build and several
    /// sources is a shape this does not translate.
    #[test]
    fn a_copy_this_cannot_translate_is_refused() {
        assert!(parse("FROM alpine\nCOPY --from=build /a /b\n").is_err());
        assert!(parse("FROM alpine\nCOPY a b c\n").is_err());
        assert!(parse("FROM alpine\nCOPY a\n").is_err());
    }

    /// The adapter's whole claim: it is a translator with nothing behind it.
    #[test]
    fn a_dockerfile_and_the_builder_it_translates_to_have_one_id() {
        let dockerfile = crate::rootfs::Rootfs::from_image("alpine:3.20")
            .run("apk add jq")
            .env("TZ", "UTC")
            .workdir("/srv");
        let parsed = parse("FROM alpine:3.20\nRUN apk add jq\nENV TZ UTC\nWORKDIR /srv\n").unwrap();
        let translated = crate::rootfs::Rootfs::from_image(parsed.base)
            .run("apk add jq")
            .env("TZ", "UTC")
            .workdir("/srv");

        assert_eq!(parsed.steps, translated.steps());
        assert_eq!(dockerfile.id().unwrap(), translated.id().unwrap());
    }

    /// A skipped instruction produces no step, so it is not in the id — two Dockerfiles
    /// differing only in a `CMD` describe the same filesystem and share a cache entry.
    ///
    /// The consequence worth knowing: editing a `USER` line changes nothing and rebuilds
    /// nothing.
    #[test]
    fn a_skipped_instruction_is_not_in_the_id() {
        let with = parse("FROM alpine\nCMD [\"sh\"]\nRUN true\n").unwrap();
        let without = parse("FROM alpine\nRUN true\n").unwrap();
        assert_eq!(with.steps, without.steps);
        assert_eq!(with.base, without.base);
    }

    /// A quoted value is one value. Splitting on whitespace before looking at the quotes
    /// tears it in half and then blames the Dockerfile for a spelling it did not use.
    #[test]
    fn a_quoted_env_value_may_hold_a_space() {
        let parsed = parse("FROM alpine\nENV MESSAGE=\"hello world\"\n").unwrap();
        assert_eq!(
            parsed.steps,
            [Step::Env {
                key: "MESSAGE".into(),
                value: "hello world".into()
            }]
        );

        let several = parse("FROM alpine\nENV A=1 B=\"x y\" C='p q'\n").unwrap();
        assert_eq!(
            several.steps,
            [
                Step::Env {
                    key: "A".into(),
                    value: "1".into()
                },
                Step::Env {
                    key: "B".into(),
                    value: "x y".into()
                },
                Step::Env {
                    key: "C".into(),
                    value: "p q".into()
                },
            ]
        );
    }

    /// Which spelling an `ENV` is comes from its first word. An `=` in the *value* of the
    /// bare form does not make it the pair form.
    #[test]
    fn an_env_value_may_hold_an_equals_in_the_bare_form() {
        let parsed = parse("FROM alpine\nENV JAVA_OPTS -Dfoo=bar\n").unwrap();
        assert_eq!(
            parsed.steps,
            [Step::Env {
                key: "JAVA_OPTS".into(),
                value: "-Dfoo=bar".into()
            }]
        );
    }

    /// One pair of quotes comes off, not every quote there is.
    #[test]
    fn only_the_surrounding_quotes_are_taken_off() {
        assert_eq!(unquoted("\"x\""), "x");
        assert_eq!(unquoted("\"\"x\"\""), "\"x\"");
        assert_eq!(unquoted("\""), "\"", "a lone quote is a value, not a pair");
        assert_eq!(unquoted("a\"b"), "a\"b");
    }

    /// `RUN` is checked for a command, like `FROM` and `WORKDIR` are for their arguments.
    #[test]
    fn a_run_with_no_command_is_refused() {
        let refused = parse("FROM alpine\nRUN\n").unwrap_err().to_string();
        assert!(refused.contains("RUN names no command"), "{refused}");
    }
}
