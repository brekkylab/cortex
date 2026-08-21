//! A memory: the thing this program is about.
//!
//! Everything else here is in service of this type. An extraction decides memories, a store
//! holds them, a search answers with them — so the type is defined on its own rather than
//! beside whichever half of the program happens to produce it, and none of the fields are
//! shaped by how one of them works.
//!
//! What a memory is *not* is a row. There is no id here, and no timestamp: those belong to a
//! store, which is where identity comes from, and a memory that carried its own would be
//! claiming to be somewhere before anything put it there.
//!
//! # Why a memory is text and nothing else
//!
//! Which side of the conversation a memory came out of is the same kind of fact as when it was
//! written: it is about the memory's arrival rather than about what is remembered, so by the
//! paragraph above it belongs to a row and not here. Held here it also has to be *decided*, and
//! the two readings of it disagree exactly where it would be used — "User was recommended
//! Drive to Survive" is a fact about the user that came out of an assistant's turn, and a
//! single field cannot say both without the extraction picking one at random.
//!
//! What the distinction is for survives without a field, because the text is required to carry
//! it: an extraction that writes "User was recommended X" has already said who provided it,
//! where a `role` beside "X is recommended" says it in a second place that no search over text
//! can see. So a store that later wants to answer "only what I said" wants that answered in
//! the sentence it is searching, and a column would be a copy of it — which is why there is one
//! field here, and why the extraction is asked for one thing.

use serde::Deserialize;

/// One thing worth remembering.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Memory {
    /// The memory itself: a statement that stands on its own, with the pronouns resolved and
    /// the relative dates made absolute.
    ///
    /// Standing on its own is the whole property. A memory is read back months later beside
    /// others it has nothing to do with, so anything it leans on — the turn it came from, what
    /// "she" referred to, when "last week" was — has to have been folded in before it was
    /// written, because none of it will be there to lean on.
    pub text: String,
}

impl Memory {
    pub fn tokenize(&self) {
        todo!()
    }
}
