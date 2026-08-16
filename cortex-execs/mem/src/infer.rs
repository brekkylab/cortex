//! What to remember from what was said, and what that means for what is already remembered.

use std::io;

use futures_core::future::BoxFuture;

/// A memory the store already holds, offered to [`Inference::reconcile`] as context.
///
/// The `id` is the store's own — whatever comes back naming it is applied to that row — so a
/// decision cannot invent one. An implementation that answers about an id it was not given is
/// answering about a memory it did not see, and the caller drops it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub memory: String,
}

/// One thing to do to the store.
///
/// There is no "leave it alone" here. A decision list is what to change, and a memory nobody
/// decided about is a memory that stays as it is — which needs no entry to say so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Remember this, as a memory that did not exist before.
    Add(String),
    /// Replace the text of an existing memory. Its id survives, so its history is continuous:
    /// this is the same fact, better stated or since changed.
    Update { id: String, memory: String },
    /// Forget one. The row goes; the history entry recording that it went stays.
    Delete { id: String },
}

/// How raw input becomes memories, and how those meet the memories already there.
///
/// # The two halves, and why they are separate calls
///
/// Remembering is not storing. What arrives at `mem insert` is a piece of text somebody wrote
/// for another purpose, and what a store should hold is the standing facts inside it — so the
/// first half, [`facts`](Self::facts), turns one input into zero or more statements each worth
/// keeping on its own.
///
/// The second half is the one that makes a store more than an append log. A new fact about
/// something already remembered should replace it rather than sit beside it, a fact that
/// contradicts one should retire it, and a fact already held should do nothing at all.
/// [`reconcile`](Self::reconcile) is where that judgement happens, and it is separate because
/// between the two the store does something neither half can: it searches, so the second half
/// is shown the memories that are actually near the new facts rather than all of them.
///
/// Splitting them also means an implementation can be interesting in one half and trivial in
/// the other — see [`Verbatim`], which does no extraction and still declines to store the
/// same sentence twice.
///
/// Both wait, and both are boxed, for the reason everything else on this path is: the
/// implementation that matters asks a model.
pub trait Inference: Send + Sync {
    /// The standing facts in `input`, each phrased to stand alone.
    ///
    /// Empty is a real answer — text can carry nothing worth remembering — and a caller that
    /// gets it writes nothing rather than writing the input for want of anything better.
    fn facts<'a>(&'a self, input: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>>;

    /// What `facts` mean for `candidates`: the memories the store found nearest to them.
    ///
    /// `candidates` is a neighbourhood, not the store — an implementation cannot conclude
    /// that a fact is new because it is not here, only that it is not near anything here,
    /// which is the same conclusion for a store that keeps related things together.
    fn reconcile<'a>(
        &'a self,
        facts: &'a [String],
        candidates: &'a [Candidate],
    ) -> BoxFuture<'a, io::Result<Vec<Decision>>>;
}

/// Store what was said, as it was said.
///
/// The whole input is one fact, and a fact already held word for word is dropped. Nothing is
/// extracted, nothing is rephrased, nothing is retired.
///
/// # This is a mode, not a placeholder
///
/// It is the right implementation whenever the caller has already decided what is worth
/// keeping — a script recording an event, a person writing a note — and a model asked to
/// improve on that would only paraphrase a sentence somebody meant literally. It is also what
/// makes the store usable with no service reachable at all.
///
/// What it cannot do is the thing that keeps a store from growing sideways: the same fact
/// arriving in different words is a second memory here, and a fact that contradicts an older
/// one leaves both. That is [`Inferred`]'s work.
pub struct Verbatim;

impl Inference for Verbatim {
    fn facts<'a>(&'a self, input: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>> {
        Box::pin(async move {
            let trimmed = input.trim();
            Ok(if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_string()]
            })
        })
    }

    fn reconcile<'a>(
        &'a self,
        facts: &'a [String],
        candidates: &'a [Candidate],
    ) -> BoxFuture<'a, io::Result<Vec<Decision>>> {
        Box::pin(async move {
            Ok(facts
                .iter()
                .filter(|fact| !candidates.iter().any(|c| c.memory == **fact))
                .map(|fact| Decision::Add(fact.clone()))
                .collect())
        })
    }
}

/// Extraction and reconciliation by a model.
///
/// [`facts`](Inference::facts) should read the input the way a person taking notes would:
/// standing facts, each rewritten to make sense without the sentence it came from, and
/// nothing for text that carries none.
///
/// [`reconcile`](Inference::reconcile) is shown those facts beside the nearest existing
/// memories and answers with [`Decision`]s over them — the same fact restated is an
/// [`Update`](Decision::Update) on the id it restates, a fact that contradicts one is a
/// [`Delete`](Decision::Delete) and an [`Add`](Decision::Add), and a fact already held is
/// no decision at all.
pub struct Inferred;

impl Inference for Inferred {
    fn facts<'a>(&'a self, _input: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>> {
        Box::pin(async move { todo!("extract standing facts from the input with a model") })
    }

    fn reconcile<'a>(
        &'a self,
        _facts: &'a [String],
        _candidates: &'a [Candidate],
    ) -> BoxFuture<'a, io::Result<Vec<Decision>>> {
        Box::pin(async move { todo!("decide each fact against its neighbours with a model") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, memory: &str) -> Candidate {
        Candidate {
            id: id.into(),
            memory: memory.into(),
        }
    }

    #[tokio::test]
    async fn the_input_is_the_fact() {
        let facts = Verbatim.facts("  the user drinks tea  ").await.unwrap();
        assert_eq!(facts, ["the user drinks tea"]);
    }

    /// Nothing said is nothing to remember — the caller writes no memory rather than an
    /// empty one.
    #[tokio::test]
    async fn blank_input_yields_no_facts() {
        assert!(Verbatim.facts("   \n ").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_fact_with_no_neighbours_is_added() {
        let facts = vec!["the user drinks tea".to_string()];
        let decisions = Verbatim.reconcile(&facts, &[]).await.unwrap();
        assert_eq!(decisions, [Decision::Add("the user drinks tea".into())]);
    }

    /// The one judgement this makes: the same sentence twice is one memory. Anything short of
    /// word-for-word is a second one, which is the limit being asserted, not an oversight.
    #[tokio::test]
    async fn a_fact_already_held_word_for_word_decides_nothing() {
        let facts = vec!["the user drinks tea".to_string()];
        let held = [candidate("a", "the user drinks tea")];
        assert!(Verbatim.reconcile(&facts, &held).await.unwrap().is_empty());

        let restated = vec!["the user drinks tea daily".to_string()];
        assert_eq!(
            Verbatim.reconcile(&restated, &held).await.unwrap(),
            [Decision::Add("the user drinks tea daily".into())]
        );
    }
}
