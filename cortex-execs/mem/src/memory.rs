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

use std::sync::OnceLock;

use charabia::{Tokenizer, TokenizerBuilder};
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
    /// The terms this memory is found by, in the order they were written.
    ///
    /// # What a term is
    ///
    /// Not a word of the text, and not a substring of it: a term is what the language's own
    /// tokenizer made of the text, normalized. Korean comes back as morphemes in decomposed
    /// jamo, Chinese as words in traditional characters, whatever was written. So nothing
    /// outside this compares a term to a string somebody typed — a query is cut by this same
    /// method and compared to what it produced, which is the only comparison that holds
    /// whichever way charabia normalizes.
    pub fn tokenize(&self) -> Vec<String> {
        // Built once. What a `Tokenizer` holds is the normalizer and segmenter configuration,
        // and the dictionaries behind those are statics in charabia.
        static TOKENIZER: OnceLock<Tokenizer<'static>> = OnceLock::new();
        let tokenizer =
            TOKENIZER.get_or_init(|| TokenizerBuilder::<Vec<u8>>::default().into_tokenizer());

        tokenizer
            .tokenize(&self.text)
            // Separators and whatever charabia could not classify are not terms. Only what it
            // calls a word is a thing a search can ask for.
            .filter(|token| token.is_word())
            .map(|token| token.lemma().to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str) -> Vec<String> {
        Memory { text: text.into() }.tokenize()
    }

    /// What a search has to be able to rely on: the terms a query is cut into are terms the
    /// text was indexed under. Every assertion below is one of these in disguise.
    fn finds(text: &str, query: &str) -> bool {
        let indexed = terms(text);
        let asked = terms(query);
        !asked.is_empty() && asked.iter().all(|term| indexed.contains(term))
    }

    #[test]
    fn a_latin_sentence_is_its_words_lowercased() {
        assert_eq!(
            terms("The quick brown fox!"),
            ["the", "quick", "brown", "fox"]
        );
    }

    /// The whole reason a dictionary is here rather than a rule about spaces: `서울에서` is one
    /// space-delimited word and two terms, and a caller asking for `서울` has to find it.
    ///
    /// Asserted against `terms` of the word rather than against `"서울"` written here, because a
    /// term is not the text it came from — charabia answers in decomposed jamo, which is the
    /// same word and not the same string. What has to hold is that both sides agree, and
    /// comparing a term to a literal would be asserting which normalization it picked.
    #[test]
    fn korean_particles_come_off_the_noun() {
        let indexed = terms("서울에서 친구를 만났다");
        assert_eq!(
            indexed.len(),
            6,
            "morphemes, not two spaced words: {indexed:?}"
        );
        assert!(finds("서울에서 친구를 만났다", "서울"), "{indexed:?}");
        assert!(finds("서울에서 친구를 만났다", "친구"), "{indexed:?}");
    }

    /// No spaces at all, so nothing but a dictionary could have found the word boundaries.
    #[test]
    fn chinese_is_cut_into_words_without_spaces() {
        let indexed = terms("我在北京大学学习");
        assert!(indexed.len() > 1, "a sentence is not one term: {indexed:?}");
        assert!(finds("我在北京大学学习", "北京"), "{indexed:?}");
        // Written simplified and asked simplified. That charabia holds both as traditional is
        // its business, and stays its business precisely because both sides come through here.
        assert!(finds("我在北京大学学习", "北京大学"), "{indexed:?}");
    }

    /// The kana is what says the dictionary is Japanese, so it is in the query too — see
    /// `han_alone_is_read_as_chinese` for what a query of bare Han is instead.
    #[test]
    fn japanese_is_cut_into_words_without_spaces() {
        let indexed = terms("東京で寿司を食べた");
        assert!(indexed.len() > 1, "a sentence is not one term: {indexed:?}");
        assert!(indexed.contains(&"東京".to_string()), "{indexed:?}");
        assert!(finds("東京で寿司を食べた", "寿司を"), "{indexed:?}");
        assert!(finds("東京で寿司を食べた", "東京で"), "{indexed:?}");
    }

    /// The one thing no text settles about itself: whether Han is Chinese or Japanese. Nothing
    /// here settles it either, so `東京` on its own is read as Chinese — two characters, and
    /// normalized as Chinese — where the same word inside a Japanese sentence is one term.
    /// A search over a Japanese phrase kept as it was written has to ask in that phrase's own
    /// script, and cannot ask in Han alone.
    #[test]
    fn han_alone_is_read_as_chinese() {
        assert_eq!(terms("東京"), ["東", "京"]);
        assert!(!finds("東京で寿司を食べた", "東京"));
    }

    /// A memory is English and the names in it are not: what a name is cut by is the script it
    /// was written in, in the middle of an English sentence like anywhere else.
    #[test]
    fn a_name_kept_as_written_is_cut_by_its_own_script() {
        let text = "User met a friend in 서울에서 and bought an iPhone 15 Pro";
        let indexed = terms(text);
        assert!(indexed.contains(&"user".to_string()), "{indexed:?}");
        assert!(indexed.contains(&"iphone".to_string()), "{indexed:?}");
        assert!(indexed.contains(&"15".to_string()), "{indexed:?}");
        assert!(finds(text, "서울"), "{indexed:?}");
    }

    /// One term per word, whichever way the word arrived. A Korean syllable is composed on one
    /// machine and decomposed on another, and a search must not be able to tell which keyboard
    /// wrote the memory.
    #[test]
    fn one_word_written_two_ways_is_one_term() {
        assert_eq!(
            terms("\u{1109}\u{1165}\u{110B}\u{116E}\u{11AF}"),
            terms("서울"),
            "the same syllables, decomposed"
        );
        assert_eq!(terms("CAFE"), terms("cafe"));
    }

    /// Nothing worth a term in it is an empty answer and not a term of nothing.
    #[test]
    fn what_is_not_a_word_is_not_a_term() {
        assert!(terms("").is_empty());
        assert!(terms("   ").is_empty());
        assert!(terms("!!! ...").is_empty());
    }
}
