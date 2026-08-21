//! The language a store's memories are written in.
//!
//! # Why a tag and not a word
//!
//! A store is labelled with its language once and read back by everything that writes to it, so
//! the label has to mean the same thing to the next reader as it did to whoever typed it.
//! "Korean", "korean", "ko", "kor" and "KR" are five spellings a person might reasonably pick,
//! and a store that holds one of them tells the next command almost nothing — it would have to
//! guess, and a guess about a language is a guess about how every memory in the file was
//! written. So what is accepted here is a language *tag*: [BCP 47], the same identifier HTML's
//! `lang`, HTTP's `Accept-Language` and every locale API already speak, which means a caller
//! almost never has to learn it and a later reader can hand it to anything that takes one.
//!
//! # Which part of BCP 47
//!
//! A language, optionally the script it is written in, optionally the region it belongs to:
//! `en`, `ko-KR`, `zh-Hant-TW`. That is the whole of what a memory store can act on. The rest of
//! the standard — extensions, private use, variants like `de-DE-1901` — describes distinctions
//! that no extraction here makes, and accepting a tag whose meaning is then ignored is worse
//! than refusing it: the file would claim a precision it does not have. A caller who needs one
//! is told plainly that this is not the tag for it.
//!
//! Case and separator are the caller's convenience and not part of the identity: `ko_kr`, `KO-KR`
//! and `ko-KR` are one language written three ways, and all three are kept as `ko-KR` — the
//! casing BCP 47 recommends, so that the tag leaves here in the form everything else expects.
//! What is *not* smoothed over is a region nobody asked for: `en` stays `en` rather than becoming
//! `en-US`, because a store written in unmarked English is not a store written in American
//! English, and inventing the region would be answering a question the caller declined to.
//!
//! [BCP 47]: https://www.rfc-editor.org/info/bcp47

use std::{fmt, str::FromStr};

/// A language tag, in the one spelling this crate keeps.
///
/// Held as the normalized string rather than as its parts, because the parts are never asked
/// about separately: what the store writes, what a prompt says and what a caller reads back is
/// the whole tag. Constructed only through [`FromStr`], so a `Lang` that exists is one that
/// parsed — nothing downstream has to re-check it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lang(String);

impl Lang {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Lang {
    /// A sentence, because this is read by whoever typed the tag: clap puts it after
    /// `invalid value 'xx' for '--lang <LANG>'`, and what is useful there is which part was not
    /// understood and what the accepted shape is.
    type Err = String;

    fn from_str(tag: &str) -> Result<Self, String> {
        let tag = tag.trim();
        if tag.is_empty() {
            return Err(
                "a store is written in some language; name it, as in `en` or `ko-KR`".into(),
            );
        }

        // `_` alongside `-`: the POSIX spelling of a locale is `ko_KR`, and it is what a caller
        // coming from `$LANG` has in front of them. It names the same language, so it is read as
        // the same tag rather than refused over a character.
        let mut parts = tag.split(['-', '_']);

        // Two or three letters, and not the four-to-eight BCP 47 also allows: those lengths are
        // reserved and registered subtags that no language actually uses, and admitting them
        // would make `--lang Korean` a tag rather than the mistake it is. Every language a
        // caller can name has an ISO 639 code of two or three letters.
        let language = parts.next().expect("a split yields at least one piece");
        if !(2..=3).contains(&language.len()) || !language.chars().all(|c| c.is_ascii_alphabetic())
        {
            return Err(format!(
                "`{language}` is not a language: a language tag begins with the two- or \
                 three-letter code for one, as in `en` or `ko`"
            ));
        }
        let mut out = language.to_ascii_lowercase();

        let mut next = parts.next();

        // A script, if one is written: four letters, and the only subtag of that shape.
        if let Some(script) =
            next.filter(|s| s.len() == 4 && s.chars().all(|c| c.is_ascii_alphabetic()))
        {
            out.push('-');
            out.push_str(&script[..1].to_ascii_uppercase());
            out.push_str(&script[1..].to_ascii_lowercase());
            next = parts.next();
        }

        // A region, if one is named: two letters, or the three digits UN M.49 uses for the
        // areas that have no letters of their own.
        if let Some(region) = next.filter(|r| {
            (r.len() == 2 && r.chars().all(|c| c.is_ascii_alphabetic()))
                || (r.len() == 3 && r.chars().all(|c| c.is_ascii_digit()))
        }) {
            out.push('-');
            out.push_str(&region.to_ascii_uppercase());
            next = parts.next();
        }

        // Whatever is left is either a subtag of the wrong shape where a script or region was
        // expected, or a part of BCP 47 this store cannot act on. Both are refused, and the
        // refusal names the piece rather than the whole tag: a caller who wrote `ko-KRR` needs
        // to be told which of the two words was wrong.
        if let Some(rest) = next {
            return Err(format!(
                "`{rest}` is not a script or a region: a language tag here is a language, \
                 optionally a four-letter script and a two-letter region, as in `ko-KR` or \
                 `zh-Hant-TW`"
            ));
        }

        Ok(Lang(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lang(tag: &str) -> String {
        tag.parse::<Lang>()
            .unwrap_or_else(|e| panic!("`{tag}` is a language tag: {e}"))
            .as_str()
            .to_string()
    }

    #[test]
    fn a_language_on_its_own_is_a_tag() {
        assert_eq!(lang("en"), "en");
        assert_eq!(lang("ko"), "ko");
        // Three letters is the tag for a language with no two-letter code of its own.
        assert_eq!(lang("haw"), "haw");
    }

    /// One language, however it was written: the casing and the separator are the caller's
    /// habit, and what is kept is the spelling BCP 47 recommends.
    #[test]
    fn the_same_language_written_five_ways_is_one_tag() {
        for written in ["ko-KR", "ko_KR", "KO-kr", "ko-kr", "  ko_kr  "] {
            assert_eq!(lang(written), "ko-KR", "written as `{written}`");
        }
    }

    #[test]
    fn a_script_is_titlecased_between_the_language_and_the_region() {
        assert_eq!(lang("zh-hant-tw"), "zh-Hant-TW");
        assert_eq!(lang("sr-Cyrl"), "sr-Cyrl");
    }

    /// The digits UN M.49 uses where a region has no letters of its own — `es-419`, Latin
    /// American Spanish, is the one a caller is most likely to have met.
    #[test]
    fn a_region_can_be_a_number() {
        assert_eq!(lang("es-419"), "es-419");
    }

    /// An unmarked language is left unmarked: a store written in English is not a store written
    /// in American English, and the difference is the caller's to state.
    #[test]
    fn no_region_is_invented_for_a_language_that_named_none() {
        assert_eq!(lang("en"), "en");
    }

    /// The name of a language is not the code for it: a store labelled `Korean` says nothing a
    /// later reader can hand to anything, and it is the mistake a caller is most likely to make.
    #[test]
    fn what_is_not_a_language_is_said_to_be_not_a_language() {
        for written in ["", "   ", "k", "1", "한국어", "e2", "Korean", "english"] {
            assert!(
                written.parse::<Lang>().is_err(),
                "`{written}` is not a language tag"
            );
        }
    }

    /// The refusal names the subtag that was wrong, not the tag it was in: `ko-KRR` is a
    /// language that parsed and a region that did not.
    #[test]
    fn a_subtag_of_the_wrong_shape_is_named() {
        let e = "ko-KRR".parse::<Lang>().expect_err("`KRR` is no region");
        assert!(e.contains("KRR"), "{e}");

        let e = "en-US-x-private".parse::<Lang>().expect_err("not this tag");
        assert!(e.contains("x"), "{e}");
    }
}
