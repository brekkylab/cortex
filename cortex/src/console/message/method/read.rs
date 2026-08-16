use serde::{Deserialize, Serialize};

use super::bytes;

/// Part of a file to hand back. The `params` of `read`.
///
/// A path is one in the executor's own filesystem, under the
/// [`path`](super::WorkFsMount::path) `init` answered with — so the file this names is the
/// one a command would open by the same name, and reading it is how a requester sees what
/// an execution left behind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Read {
    /// UTF-8, and the executor's own: a requester builds it by joining onto the path the
    /// session was answered with, which is the one both ends have a name for.
    pub path: String,

    /// Where in the file to start. `None` is the beginning.
    ///
    /// Past the end is not an error: the answer is empty [`data`](ReadResult::data)
    /// and the [`size`](ReadResult::size) that says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,

    /// How many bytes to hand back at most, `None` for as many as there are.
    ///
    /// Either way the answer travels in one message under
    /// [`MAX_PAYLOAD`](super::MAX_PAYLOAD), so the executor hands back less than this
    /// asks for when the rest would not fit. Comparing what arrives against
    /// [`size`](ReadResult::size) is how a requester knows, and asking again from
    /// further along is how it gets the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
}

/// The bytes a `read` asked for, and how big the file is. The `result` of `read`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResult {
    /// What was there, starting at the [`offset`](Read::offset) that was asked for.
    #[serde(with = "bytes")]
    pub data: Vec<u8>,

    /// The whole file's size, and not `data`'s length.
    ///
    /// The two differ whenever a read was bounded — by a [`len`](Read::len), by an
    /// [`offset`](Read::offset) past the beginning, or by what one message holds — and
    /// the difference is the only thing that says there is more to ask for. A reader
    /// that ignores it has no way to tell a whole small file from the front of a large
    /// one.
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read with no bounds carries neither member, so the whole-file case is the
    /// smallest thing the method can say.
    #[test]
    fn an_unbounded_read_carries_no_bounds() {
        let read = Read {
            path: "log.txt".into(),
            ..Read::default()
        };
        let doc = bson::serialize_to_document(&read).unwrap();
        assert_eq!(doc, bson::doc! {"path": "log.txt"});
        assert_eq!(bson::deserialize_from_document::<Read>(doc).unwrap(), read);
    }

    /// A read that hands back less than the file holds is the ordinary case, and
    /// `size` against `data` is the only thing that says so.
    #[test]
    fn a_read_result_says_how_much_it_left() {
        let value = bson::serialize_to_bson(&ReadResult {
            data: vec![0xff, 0x00],
            size: 4096,
        })
        .unwrap();

        let doc = value.as_document().unwrap();
        assert_eq!(
            doc.get("data"),
            Some(&bson::Bson::Binary(bson::Binary {
                subtype: bson::spec::BinarySubtype::Generic,
                bytes: vec![0xff, 0x00],
            })),
        );

        let read: ReadResult = bson::deserialize_from_bson(value).unwrap();
        assert!(read.size > read.data.len() as u64);
    }
}
