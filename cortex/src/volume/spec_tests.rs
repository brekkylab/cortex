use super::*;

fn s3() -> S3Config {
    S3Config {
        bucket: "b".into(),
        region: "r".into(),
        access_key_id: "k".into(),
        secret_access_key: "s".into(),
        endpoint: None,
        key_prefix: None,
    }
}

/// **The claim this module exists to keep**: what a spec looks like on the wire does not
/// depend on how the binary reading it was compiled.
///
/// Run under every feature combination. A build without `s3` still has the tag, still
/// parses a document carrying it, and refuses it only when asked to *realize* it — which is
/// how a client gets `unsupported volume kind: s3` instead of a parse error it cannot tell
/// from its own bug.
#[test]
fn every_tag_is_on_the_wire() {
    let specs = [
        VolumeSpec::Local {
            host: "/tmp/p".into(),
        },
        VolumeSpec::S3(s3()),
        VolumeSpec::Notion(NotionConfig {
            api_key: "a".into(),
        }),
    ];
    let tags: Vec<String> = specs
        .iter()
        .map(|s| {
            let doc = bson::serialize_to_document(s).expect("serializes");
            doc.get_str("type").expect("has a type tag").to_string()
        })
        .collect();
    assert_eq!(tags, vec!["local", "s3", "notion"]);
}

/// A kind this build cannot realize is refused as such, not as a broken document.
#[test]
fn a_kind_without_a_provider_is_refused_by_name() {
    #[cfg(not(feature = "s3"))]
    assert!(matches!(
        VolumeSpec::S3(s3()).build_mountable(),
        Err(crate::CortexError::UnsupportedVolume("s3"))
    ));

    #[cfg(not(feature = "notion"))]
    assert!(matches!(
        VolumeSpec::Notion(NotionConfig {
            api_key: "a".into()
        })
        .build_mountable(),
        Err(crate::CortexError::UnsupportedVolume("notion"))
    ));

    // Under `--features s3,notion` both realize, and there is nothing here to assert that
    // the tests above do not already cover.
}

/// A mount is a named pair on the wire, not a positional one.
///
/// `{"0": …, "1": …}` is what a tuple gives, and it is unreadable, unextendable, and
/// impossible to write a second implementation against from the document alone.
#[test]
fn a_mount_names_its_members() {
    let spec = WorkspaceSpec::default().mount(
        "work",
        VolumeSpec::Local {
            host: "/tmp/p".into(),
        },
    );
    let doc = bson::serialize_to_document(&spec).expect("serializes");
    let mounts = doc.get_array("mounts").expect("has mounts");
    let first = mounts[0].as_document().expect("a document, not an array");

    assert_eq!(first.get_str("path").expect("named path"), "work");
    assert!(first.get_document("volume").is_ok(), "named volume");
}

#[test]
fn a_spec_round_trips_through_bson() {
    let spec = WorkspaceSpec::default()
        .mount(
            "work",
            VolumeSpec::Local {
                host: "/tmp/p".into(),
            },
        )
        .mount("archive", VolumeSpec::S3(s3()));

    let bytes = bson::serialize_to_vec(&spec).expect("serializes");
    let back: WorkspaceSpec = bson::deserialize_from_slice(&bytes).expect("parses");
    assert_eq!(back, spec);
}

/// **What a spec answers must not depend on how it was written down.**
///
/// The two mounts are independently bad — one names a kind this build may have no provider
/// for, one names a path no workspace can hold — and realizing them one at a time lets
/// whichever comes first pick the error. So the same set, written both ways round, has to
/// answer the same thing.
///
/// Not asserting *which* error, because that is the next test's job and it is the one thing
/// here that legitimately differs by build.
#[test]
fn the_order_a_spec_lists_its_mounts_in_does_not_change_the_answer() {
    let bad_path = (
        "../escape",
        VolumeSpec::Local {
            host: "/tmp/p".into(),
        },
    );
    let unrealizable = ("data", VolumeSpec::S3(s3()));

    let forwards = WorkspaceSpec::default()
        .mount(unrealizable.0, unrealizable.1.clone())
        .mount(bad_path.0, bad_path.1.clone());
    let backwards = WorkspaceSpec::default()
        .mount(bad_path.0, bad_path.1)
        .mount(unrealizable.0, unrealizable.1);

    // Compared as their `Display`, because `CortexError` has no `PartialEq` — and because
    // the message is half of what a client reads, so two answers that differ only there
    // are still two answers.
    let answer = |spec: &WorkspaceSpec| {
        crate::volume::Workspace::from_spec(spec)
            .err()
            .expect("neither spelling is realizable")
            .to_string()
    };
    assert_eq!(answer(&forwards), answer(&backwards));
}

/// And the answer is the *path*, in every build — because a path a workspace cannot hold is
/// wrong whoever reads it, where an unrealizable kind is only wrong here.
///
/// This is the test that would have caught it: under `--features s3` the interleaved
/// version answered `InvalidName` and without it `UnsupportedVolume`, for one document.
#[test]
fn a_spec_that_is_wrong_two_ways_is_answered_the_same_in_every_build() {
    let spec = WorkspaceSpec::default()
        .mount("data", VolumeSpec::S3(s3()))
        .mount(
            "../escape",
            VolumeSpec::Local {
                host: "/tmp/p".into(),
            },
        );
    assert!(matches!(
        crate::volume::Workspace::from_spec(&spec),
        Err(crate::CortexError::InvalidName)
    ));
}

/// A repeated path is answered before anything is opened, so the same spelling twice is
/// `AlreadyExists` rather than whatever realizing the first one happened to do.
#[test]
fn a_repeated_mount_path_is_refused_before_any_volume_is_realized() {
    let spec = WorkspaceSpec::default()
        .mount("data", VolumeSpec::S3(s3()))
        .mount("./data", VolumeSpec::S3(s3()));
    assert!(matches!(
        crate::volume::Workspace::from_spec(&spec),
        Err(crate::CortexError::AlreadyExists)
    ));
}

/// A host path with no UTF-8 form is refused where it was chosen, not inside a serializer.
///
/// Without the check it reaches BSON, which answers "path contains invalid UTF-8
/// characters" — true, and no help in finding which of a session's mounts said it.
#[test]
fn a_host_path_bson_cannot_carry_is_refused_before_it_is_written() {
    use std::os::unix::ffi::OsStrExt as _;

    let host = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff\xfe"));
    let spec = WorkspaceSpec::default().mount("work", VolumeSpec::Local { host });

    assert!(matches!(
        spec.check(),
        Err(crate::CortexError::InvalidName)
    ));
    // And this is what it is standing in for.
    assert!(bson::serialize_to_vec(&spec).is_err());
}

/// The check says nothing about a spec it has no complaint with, including an empty one.
#[test]
fn a_writable_spec_passes_the_check() {
    assert!(WorkspaceSpec::default().check().is_ok());
    assert!(
        WorkspaceSpec::default()
            .mount(
                "work",
                VolumeSpec::Local {
                    host: "/tmp/p".into()
                }
            )
            .mount("archive", VolumeSpec::S3(s3()))
            .check()
            .is_ok()
    );
}

#[test]
fn an_empty_spec_is_empty_and_a_mounted_one_is_not() {
    assert!(WorkspaceSpec::default().is_empty());
    assert!(
        !WorkspaceSpec::default()
            .mount(
                "",
                VolumeSpec::Local {
                    host: "/tmp/p".into()
                }
            )
            .is_empty()
    );
}
