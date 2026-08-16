//! Remembering and recalling: the [`Store`], an [`Embedder`] and an [`Inference`] doing one
//! job between them.

use std::{io, sync::Arc};

use crate::{
    Candidate, Decision, Embedder, Inference,
    store::{Applied, Entry, Hit, Record, Scope, Store, Write},
};

/// How many existing memories a fact is judged against.
///
/// The neighbourhood [`Inference::reconcile`] is shown, per fact. Small on purpose: these are
/// the memories a new fact could plausibly be *about*, and past the nearest few they are
/// unrelated text that only makes the decision harder. mem0 settled on the same number.
const NEIGHBOURS: usize = 5;

/// A memory store with the two providers it needs to be used.
///
/// # What this adds over [`Store`]
///
/// The store speaks in vectors and rows: it can find neighbours and it can write what it is
/// told to write, and it has no opinion about what should be written. This is where the
/// opinion lives — one `add` is an extraction, a search per fact, a judgement, and a
/// transaction, in that order — and it is also where waiting is allowed. The two providers
/// reach services; the store blocks; keeping them apart is what lets each be replaced without
/// the other noticing.
///
/// # Blocking, and where it happens
///
/// SQLite and the vector scan run in the calling thread and can take as long as the file is
/// big. Every call into the store therefore goes through [`spawn_blocking`], because the
/// thread this is awaited on is the one answering a console's channel — see
/// [`Executable`](cortex::executable::Executable), which is the same argument for the same
/// reason.
///
/// [`spawn_blocking`]: tokio::task::spawn_blocking
pub struct Memory {
    store: Arc<Store>,
    embedder: Arc<dyn Embedder>,
    inference: Arc<dyn Inference>,
}

/// The store, and which embedder is holding it open — the pair that decides what a search
/// means. Neither provider can say more about itself than its name.
impl std::fmt::Debug for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memory")
            .field("store", &self.store)
            .field("embedder", &self.embedder.name())
            .finish_non_exhaustive()
    }
}

impl Memory {
    /// Open the store at `path`, creating it if it is not there.
    pub async fn create(
        path: &std::path::Path,
        embedder: Arc<dyn Embedder>,
        inference: Arc<dyn Inference>,
    ) -> io::Result<Memory> {
        Memory::opened(path, embedder, inference, Store::create).await
    }

    /// Open an existing store at `path` for writing.
    ///
    /// [`NotFound`](io::ErrorKind::NotFound) if there is no such file — see [`Store::open`].
    pub async fn open(
        path: &std::path::Path,
        embedder: Arc<dyn Embedder>,
        inference: Arc<dyn Inference>,
    ) -> io::Result<Memory> {
        Memory::opened(path, embedder, inference, Store::open).await
    }

    /// Open an existing store at `path`, read-only.
    ///
    /// [`NotFound`](io::ErrorKind::NotFound) if there is no such file — see [`Store::read`].
    pub async fn read(
        path: &std::path::Path,
        embedder: Arc<dyn Embedder>,
        inference: Arc<dyn Inference>,
    ) -> io::Result<Memory> {
        Memory::opened(path, embedder, inference, Store::read).await
    }

    /// The three constructors above, which differ only in how the file is opened.
    ///
    /// The embedder's identity is read here and handed to `open` as plain data, because the
    /// store is opened on another thread and an `Arc<dyn Embedder>` is not the store's to hold.
    async fn opened(
        path: &std::path::Path,
        embedder: Arc<dyn Embedder>,
        inference: Arc<dyn Inference>,
        open: fn(&std::path::Path, &str, usize) -> io::Result<Store>,
    ) -> io::Result<Memory> {
        let (name, dims) = (embedder.name().to_string(), embedder.dims());
        let path = path.to_path_buf();
        let store = blocking(move || open(&path, &name, dims)).await?;
        Ok(Memory {
            store: Arc::new(store),
            embedder,
            inference,
        })
    }

    /// Remember what is worth remembering in `input`.
    ///
    /// # The pipeline, and why it is this shape
    ///
    /// The input becomes facts; each fact is placed in the vector space; each fact's
    /// neighbourhood is looked up; and the facts, together with everything near them, are
    /// judged as a whole. Only then is anything written.
    ///
    /// The judgement is one call rather than one per fact because facts arriving together can
    /// bear on each other and on the same memory — two facts that both restate one old memory
    /// should not each decide to replace it, unaware of the other.
    ///
    /// # What it will not do
    ///
    /// A decision naming a memory that was not among the neighbours is dropped. The ids exist
    /// so a decision can point at something it was shown; a decision pointing anywhere else is
    /// about a memory the judgement never saw, and an implementation that reaches a model is
    /// exactly where an id it invented would come from.
    ///
    /// The answer is what was actually written, which can be empty: nothing worth remembering,
    /// nothing to change, or a fact already held.
    pub async fn add(
        &self,
        input: &str,
        scope: &Scope,
        metadata: &str,
    ) -> io::Result<Vec<Applied>> {
        let facts = self.inference.facts(input).await?;
        if facts.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self.embed(&facts).await?;

        let mut candidates: Vec<Candidate> = Vec::new();
        for vector in &vectors {
            for hit in self.nearest(vector.clone(), scope, NEIGHBOURS).await? {
                if !candidates.iter().any(|c| c.id == hit.record.id) {
                    candidates.push(Candidate {
                        id: hit.record.id,
                        memory: hit.record.memory,
                    });
                }
            }
        }

        let decisions = self.inference.reconcile(&facts, &candidates).await?;
        let decisions: Vec<Decision> = decisions
            .into_iter()
            .filter(|d| match d {
                Decision::Add(_) => true,
                Decision::Update { id, .. } | Decision::Delete { id } => {
                    candidates.iter().any(|c| c.id == *id)
                }
            })
            .collect();

        // Texts that need placing and were not among the facts: an update is usually a
        // rephrasing, so its vector is one nobody has computed. Gathered first and embedded
        // together, because a provider charges by the round trip.
        let mut extra: Vec<String> = Vec::new();
        for decision in &decisions {
            let memory = match decision {
                Decision::Add(memory) | Decision::Update { memory, .. } => memory,
                Decision::Delete { .. } => continue,
            };
            if !facts.contains(memory) && !extra.contains(memory) {
                extra.push(memory.clone());
            }
        }
        let extra_vectors = self.embed(&extra).await?;

        let placed = |memory: &String| -> Vec<f32> {
            let found = facts
                .iter()
                .position(|f| f == memory)
                .map(|i| &vectors[i])
                .or_else(|| {
                    extra
                        .iter()
                        .position(|t| t == memory)
                        .map(|i| &extra_vectors[i])
                });
            found
                .expect("every written text was embedded above")
                .clone()
        };

        let writes: Vec<Write> = decisions
            .iter()
            .map(|decision| match decision {
                Decision::Add(memory) => Write::Add {
                    memory: memory.clone(),
                    vector: placed(memory),
                },
                Decision::Update { id, memory } => Write::Update {
                    id: id.clone(),
                    memory: memory.clone(),
                    vector: placed(memory),
                },
                Decision::Delete { id } => Write::Delete { id: id.clone() },
            })
            .collect();

        let (scope, metadata, now) = (scope.clone(), metadata.to_string(), now());
        self.blocking(move |store| store.apply(&writes, &scope, &metadata, &now))
            .await
    }

    /// The memories nearest `query` within `scope`, nearest first.
    ///
    /// No inference: a query is a question, not something to remember, and turning it into
    /// facts would search for a rephrasing of what was asked rather than for what was asked.
    pub async fn search(&self, query: &str, scope: &Scope, limit: usize) -> io::Result<Vec<Hit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut vectors = self.embed(&[query.to_string()]).await?;
        self.nearest(vectors.remove(0), scope, limit).await
    }

    /// Every memory in `scope`, oldest first, up to `limit`.
    pub async fn all(&self, scope: &Scope, limit: usize) -> io::Result<Vec<Record>> {
        let scope = scope.clone();
        self.blocking(move |store| store.all(&scope, limit)).await
    }

    /// One memory by id, or `None`.
    pub async fn get(&self, id: &str) -> io::Result<Option<Record>> {
        let id = id.to_string();
        self.blocking(move |store| store.get(&id)).await
    }

    /// Forget the memory `id`. The answer is what was removed, or empty if there was nothing
    /// by that id to remove.
    pub async fn delete(&self, id: &str) -> io::Result<Vec<Applied>> {
        let writes = vec![Write::Delete { id: id.to_string() }];
        let now = now();
        self.blocking(move |store| store.apply(&writes, &Scope::default(), "{}", &now))
            .await
    }

    /// Everything that has happened to the memory `id`, oldest first.
    pub async fn history(&self, id: &str) -> io::Result<Vec<Entry>> {
        let id = id.to_string();
        self.blocking(move |store| store.history(&id)).await
    }

    /// Place `texts`, checking that the embedder answered about all of them at the width it
    /// promised.
    ///
    /// Checked rather than trusted because the store's table was built on that promise, and a
    /// provider that returns a short list would otherwise misalign every vector after the gap
    /// — each fact stored under its neighbour's meaning, with nothing to observe afterwards.
    async fn embed(&self, texts: &[String]) -> io::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self.embedder.embed(texts).await?;
        if vectors.len() != texts.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the {} embedder answered about {} of {} texts",
                    self.embedder.name(),
                    vectors.len(),
                    texts.len()
                ),
            ));
        }
        if let Some(wrong) = vectors.iter().find(|v| v.len() != self.embedder.dims()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the {} embedder declares {} dimensions and produced {}",
                    self.embedder.name(),
                    self.embedder.dims(),
                    wrong.len()
                ),
            ));
        }
        Ok(vectors)
    }

    async fn nearest(&self, vector: Vec<f32>, scope: &Scope, k: usize) -> io::Result<Vec<Hit>> {
        let scope = scope.clone();
        self.blocking(move |store| store.nearest(&vector, &scope, k))
            .await
    }

    /// Run `f` against the store on a thread where blocking is allowed.
    async fn blocking<T, F>(&self, f: F) -> io::Result<T>
    where
        F: FnOnce(&Store) -> io::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let store = Arc::clone(&self.store);
        blocking(move || f(&store)).await
    }
}

/// Now, as the store records it: UTC, milliseconds, `Z`.
///
/// One spelling for every timestamp in the file, sortable as text — which is how the history
/// is ordered when two entries land in the same transaction.
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Run `f` where blocking is allowed, and answer in the error type the rest of this speaks.
///
/// A join failure is a panic in `f` or a runtime shutting down under it. Neither is something
/// a caller can do anything about beyond reporting it, so it becomes an
/// [`io::Error`] like everything else here rather than a second failure mode to match on.
async fn blocking<T, F>(f: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use futures_core::future::BoxFuture;

    use super::*;
    use crate::{Event, HashEmbedder, Verbatim};

    fn embedder() -> Arc<dyn Embedder> {
        Arc::new(HashEmbedder::default())
    }

    async fn memory(dir: &tempfile::TempDir) -> Memory {
        Memory::create(&dir.path().join("m.sqlite"), embedder(), Arc::new(Verbatim))
            .await
            .expect("a store can be created")
    }

    async fn add(memory: &Memory, input: &str) -> Vec<Applied> {
        memory
            .add(input, &Scope::default(), "{}")
            .await
            .expect("the input can be remembered")
    }

    #[tokio::test]
    async fn what_was_remembered_is_found_by_asking_about_it() {
        let dir = tempfile::tempdir().unwrap();
        let mem = memory(&dir).await;
        add(&mem, "the user drinks tea in the morning").await;
        add(&mem, "deploys go out on friday afternoons").await;

        let hits = mem
            .search("what does the user drink", &Scope::default(), 1)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record.memory, "the user drinks tea in the morning");
    }

    #[tokio::test]
    async fn a_memory_carries_the_scope_and_metadata_it_was_written_with() {
        let dir = tempfile::tempdir().unwrap();
        let mem = memory(&dir).await;
        let scope = Scope {
            user: "ana".into(),
            ..Scope::default()
        };
        mem.add("drinks tea", &scope, r#"{"source":"chat"}"#)
            .await
            .unwrap();

        let all = mem.all(&scope, 10).await.unwrap();
        assert_eq!(all[0].scope, scope);
        assert_eq!(all[0].metadata, r#"{"source":"chat"}"#);
    }

    /// Nothing worth remembering is a real answer, and it writes nothing rather than storing
    /// the input for want of anything better.
    #[tokio::test]
    async fn input_with_no_facts_in_it_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mem = memory(&dir).await;
        assert!(add(&mem, "   ").await.is_empty());
        assert!(mem.all(&Scope::default(), 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_forgotten_memory_stops_being_found_and_keeps_its_history() {
        let dir = tempfile::tempdir().unwrap();
        let mem = memory(&dir).await;
        let id = add(&mem, "the user drinks tea").await[0].id.clone();

        let removed = mem.delete(&id).await.unwrap();
        assert_eq!(removed[0].event, Event::Delete);
        assert!(
            mem.search("tea", &Scope::default(), 5)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            mem.history(&id)
                .await
                .unwrap()
                .iter()
                .map(|e| e.event)
                .collect::<Vec<_>>(),
            [Event::Add, Event::Delete]
        );
    }

    /// An id is a handle on something the judgement was shown. One that names anything else
    /// came from somewhere that cannot be trusted with it — which is precisely a model.
    #[tokio::test]
    async fn a_decision_about_a_memory_it_never_saw_is_dropped() {
        /// Answers with one fact and a delete of whatever id it was constructed with.
        struct Meddler(String);

        impl Inference for Meddler {
            fn facts<'a>(&'a self, input: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>> {
                Box::pin(async move { Ok(vec![input.to_string()]) })
            }

            fn reconcile<'a>(
                &'a self,
                facts: &'a [String],
                _candidates: &'a [Candidate],
            ) -> BoxFuture<'a, io::Result<Vec<Decision>>> {
                Box::pin(async move {
                    Ok(vec![
                        Decision::Add(facts[0].clone()),
                        Decision::Delete { id: self.0.clone() },
                    ])
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite");
        let mem = Memory::create(&path, embedder(), Arc::new(Verbatim))
            .await
            .unwrap();

        // The memory to be meddled with, and then `NEIGHBOURS` memories that all share a word
        // with what is about to be inserted — so the target is the one thing a search for it
        // cannot reach, rather than merely an unrelated memory in an otherwise empty store.
        let held = add(&mem, "deploys go out on friday").await[0].id.clone();
        for text in [
            "the user drinks coffee",
            "the user drinks water",
            "the user drinks juice",
            "the user drinks milk",
            "the user drinks wine",
        ] {
            add(&mem, text).await;
        }
        drop(mem);

        let meddling = Memory::create(&path, embedder(), Arc::new(Meddler(held.clone())))
            .await
            .unwrap();
        let done = meddling
            .add("the user drinks tea", &Scope::default(), "{}")
            .await
            .unwrap();

        assert_eq!(
            done.iter().map(|a| a.event).collect::<Vec<_>>(),
            [Event::Add]
        );
        assert!(meddling.get(&held).await.unwrap().is_some());
    }

    /// The store's table was built on the width the embedder promised, so an embedder that
    /// breaks its own contract is refused rather than written.
    #[tokio::test]
    async fn an_embedder_that_answers_about_fewer_texts_is_refused() {
        struct Short;

        impl Embedder for Short {
            fn name(&self) -> &str {
                "hash"
            }

            fn dims(&self) -> usize {
                HashEmbedder::default().dims()
            }

            fn embed<'a>(
                &'a self,
                _texts: &'a [String],
            ) -> BoxFuture<'a, io::Result<Vec<Vec<f32>>>> {
                Box::pin(async move { Ok(Vec::new()) })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mem = Memory::create(
            &dir.path().join("m.sqlite"),
            Arc::new(Short),
            Arc::new(Verbatim),
        )
        .await
        .unwrap();
        let err = mem
            .add("the user drinks tea", &Scope::default(), "{}")
            .await
            .expect_err("an embedder that answers about nothing cannot be stored");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn searching_a_store_that_is_not_there_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = Memory::read(
            &dir.path().join("absent.sqlite"),
            embedder(),
            Arc::new(Verbatim),
        )
        .await
        .expect_err("there is no such store");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
