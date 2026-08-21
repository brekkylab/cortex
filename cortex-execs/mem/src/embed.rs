//! Turning text into the vector the store searches by.

use std::io;

use futures_core::future::BoxFuture;

/// Something that can place a piece of text in a vector space.
///
/// # Why this is a trait, and why it names itself
///
/// The vectors are what a store is *made of*: the table that holds them fixes their width
/// when it is created, and a query only means anything against vectors the same model
/// produced. Two embedders that agree on width still disagree on direction, so a file
/// written by one and searched by another answers confidently and wrongly — there is no
/// error to observe, only bad neighbours.
///
/// So an embedder is not a swappable detail behind one interface; it is part of the store's
/// identity. [`name`](Self::name) and [`dims`](Self::dims) are recorded when a store is
/// created and checked every time one is opened, which turns that silent wrongness into a
/// refusal at the door.
///
/// # Why it waits, and why it batches
///
/// The implementation worth having reaches a service. That is the same reason
/// [`Executable`](cortex::exec::Executable) is async, and the future is boxed here for
/// the same reason too: a store holds one behind a `dyn`, so the type is not in anyone's
/// signature.
///
/// [`embed`](Self::embed) takes a slice and not a string because a single `mem insert` has
/// several texts to place at once — one per fact the input was broken into — and every
/// provider charges a round trip per call rather than per text. A caller with one text passes
/// a slice of one.
pub trait Embedder: Send + Sync {
    /// What to record this embedder as in a store it writes.
    ///
    /// Whatever distinguishes vectors that can be compared from vectors that cannot: a
    /// provider and a model, usually. Two embedders sharing a name promise their vectors are
    /// interchangeable, and a store opened under a name it was not written with is refused.
    fn name(&self) -> &str;

    /// The width of every vector this produces.
    ///
    /// Fixed for the life of the embedder — a store's vector table is declared with this
    /// number and cannot be widened afterwards.
    fn dims(&self) -> usize;

    /// Place each of `texts`, in order.
    ///
    /// The answer has one vector per input, each [`dims`](Self::dims) long. A provider that
    /// returns anything else has broken the contract the store's table was built on, and the
    /// store checks rather than trusting it.
    fn embed<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, io::Result<Vec<Vec<f32>>>>;
}

/// An embedder with no model behind it: words hashed into buckets, and nothing learned.
///
/// # What it is for
///
/// Everything around the model — the schema, the nearest-neighbour query, the decision about
/// what to write, the executable's own argument handling — is testable only if there is
/// *some* embedder, and a test that reaches a network is not a test of any of it. This one is
/// deterministic, offline, and instant, so the whole pipeline can be exercised end to end
/// with no key and no service.
///
/// It is not a stand-in that pretends: texts sharing words land near each other because they
/// hit the same buckets, and texts sharing none are orthogonal. That is enough to tell a
/// working search from a broken one, and it is emphatically not enough to tell a *good*
/// search from a bad one — "car" and "automobile" are as unrelated here as any two words.
///
/// # The hashing trick
///
/// Each word is hashed; the hash picks a bucket and a sign, and the bucket is incremented by
/// that sign. Signs are what keep unrelated texts near-orthogonal rather than merely
/// positively correlated: without them every collision pushes two vectors together and only
/// ever together, so with a few hundred buckets everything ends up vaguely similar to
/// everything.
///
/// The vector is then scaled to unit length, because the store compares by cosine distance
/// and a long text would otherwise be a large vector rather than a differently-pointed one.
/// A text with no words at all is left as zeroes, which is at cosine distance 1 — as far as
/// the metric goes — from everything.
pub struct HashEmbedder {
    dims: usize,
}

impl HashEmbedder {
    /// An embedder producing `dims`-wide vectors.
    ///
    /// Wider is fewer collisions, and nothing else — there is no model whose width this has
    /// to match. [`default`](Self::default)'s 256 is enough that the words in a handful of
    /// sentences rarely land on top of each other.
    pub fn new(dims: usize) -> Self {
        assert!(dims > 0, "an embedding needs at least one dimension");
        HashEmbedder { dims }
    }

    /// `text` as a unit vector, or zeroes if it has no words.
    fn vector(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dims];
        for word in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let h = fnv1a(word.to_lowercase().as_bytes());
            let bucket = (h % self.dims as u64) as usize;
            // The top bit, which no bucket index consumed: a second, independent draw from
            // the same hash rather than a rederivation of the first.
            let sign = if h >> 63 == 0 { 1.0 } else { -1.0 };
            v[bucket] += sign;
        }

        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        HashEmbedder::new(256)
    }
}

impl Embedder for HashEmbedder {
    fn name(&self) -> &str {
        "hash"
    }

    fn dims(&self) -> usize {
        self.dims
    }

    fn embed<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, io::Result<Vec<Vec<f32>>>> {
        Box::pin(async move { Ok(texts.iter().map(|t| self.vector(t)).collect()) })
    }
}

/// FNV-1a, 64-bit.
///
/// Written out rather than taken from [`DefaultHasher`](std::collections::hash_map::DefaultHasher),
/// whose output the standard library explicitly declines to promise across releases. These
/// vectors are written to a file that outlives the build that wrote it, so a hash that may
/// change is a store that silently stops matching itself.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Cosine similarity, for callers that hold two vectors rather than a store.
///
/// One where two vectors point the same way, zero where they are unrelated. The store
/// computes this itself, in SQL; this is here for tests and for a caller comparing vectors it
/// has not written yet.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn embed(e: &HashEmbedder, texts: &[&str]) -> Vec<Vec<f32>> {
        let owned: Vec<String> = texts.iter().map(|t| t.to_string()).collect();
        e.embed(&owned).await.unwrap()
    }

    #[tokio::test]
    async fn every_text_gets_one_vector_of_the_declared_width() {
        let e = HashEmbedder::new(64);
        let v = embed(&e, &["one", "two", "three"]).await;
        assert_eq!(v.len(), 3);
        assert!(v.iter().all(|v| v.len() == 64));
    }

    /// The property the whole store rests on: a file written on one run is searchable on the
    /// next, which is only true if the same text keeps producing the same vector.
    #[tokio::test]
    async fn the_same_text_always_lands_in_the_same_place() {
        let e = HashEmbedder::default();
        let a = embed(&e, &["the user drinks tea"]).await;
        let b = embed(&e, &["the user drinks tea"]).await;
        assert_eq!(a, b);
    }

    /// Enough of a model to tell a working search from a broken one — shared words pull
    /// texts together, and nothing else does.
    #[tokio::test]
    async fn shared_words_are_nearer_than_none() {
        let e = HashEmbedder::default();
        let v = embed(
            &e,
            &[
                "the user drinks tea",
                "the user drinks coffee",
                "deploys run on friday",
            ],
        )
        .await;

        let same_topic = cosine(&v[0], &v[1]);
        let unrelated = cosine(&v[0], &v[2]);
        assert!(
            same_topic > unrelated,
            "sharing three words scored {same_topic}, sharing none scored {unrelated}"
        );
    }

    /// Casing and punctuation are not content — a memory written in a sentence has to match a
    /// query typed as a fragment.
    #[tokio::test]
    async fn case_and_punctuation_do_not_move_a_text() {
        let e = HashEmbedder::default();
        let v = embed(&e, &["Tea, please!", "tea please"]).await;
        assert!((cosine(&v[0], &v[1]) - 1.0).abs() < 1e-6);
    }

    /// Unit length, so cosine distance is about direction and a long memory is not a big one.
    #[tokio::test]
    async fn a_vector_is_scaled_to_unit_length() {
        let e = HashEmbedder::default();
        for v in embed(
            &e,
            &["one word", "a rather longer sentence with several words"],
        )
        .await
        {
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-5, "norm was {norm}");
        }
    }

    /// A text with nothing in it is not an error and not near anything: zeroes, which cosine
    /// puts at distance 1 from every other vector.
    #[tokio::test]
    async fn a_text_with_no_words_is_all_zeroes() {
        let e = HashEmbedder::new(8);
        let v = embed(&e, &["   ...  "]).await;
        assert!(v[0].iter().all(|x| *x == 0.0));
        assert_eq!(cosine(&v[0], &[1.0; 8]), 0.0);
    }
}
