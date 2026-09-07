//! A memory: the thing this program is about.
//!
//! Everything else here is in service of this type. A caller decides memories, a store holds
//! them, a search answers with them — so the type is defined on its own rather than beside
//! whichever half of the program happens to produce it, and none of the fields are shaped by
//! how one of them works.
//!
//! What a memory is *not* is a row. There is no id here, and no timestamp: those belong to a
//! store, which is where identity comes from, and a memory that carried its own would be
//! claiming to be somewhere before anything put it there.
//!
//! # Why a memory is text and nothing else
//!
//! Where a memory came from is the same kind of fact as when it was written: it is about the
//! memory's arrival rather than about what is remembered, so by the paragraph above it belongs
//! to a row and not here. It would also have to be *decided*, and this crate is the one that
//! does not decide anything about what it is given — a field for provenance would be a field
//! `insert` had to invent a value for on behalf of a caller who did not offer one.
//!
//! What such a field is for survives without it, because the text is required to carry it: a
//! caller who writes "User was recommended X" has already said who provided it, where a `role`
//! beside "X is recommended" says it in a second place that no search over text can see. So a
//! store that later wants to answer "only what I said" wants that answered in the sentence it
//! is searching, and a column would be a copy of it — which is why there is one field here,
//! and why `insert` takes one string per memory and nothing else.
//!
//! # Nothing here cuts the text
//!
//! There is no tokenizer in this module, and that is the arrangement rather than an omission:
//! **the store hands the text to FTS5 as written, and FTS5 cuts it.** See
//! [`store`](crate::store) for what that buys and what it postpones.

/// One thing worth remembering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory {
    /// The memory itself: a statement that stands on its own, with the pronouns resolved and
    /// the relative dates made absolute.
    ///
    /// Standing on its own is the whole property. A memory is read back months later beside
    /// others it has nothing to do with, so anything it leans on — where it came from, what
    /// "she" referred to, when "last week" was — has to have been folded in before it was
    /// written, because none of it will be there to lean on.
    ///
    /// **Nothing here enforces it, and nothing can.** `mem` writes what it is handed; a
    /// caller that hands it "she moved there last week" gets a row saying exactly that, which
    /// is the honest behaviour for a command whose whole point is not to second-guess its
    /// input.
    pub text: String,
}
