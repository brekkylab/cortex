//! Unit tests for the tree answering its own index.
//!
//! Nothing here reaches a network. A fake source hands back fixtures, because what these assert
//! is what the *tree* decides: which file a hit's path names, whose name goes on the record, and
//! that the record is spelled the way that file spells it.

use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::super::error::{SourceError, SourceResult};
use super::super::source::{
    Author, Capabilities, ConvId, ConvKind, Conversation, FileRef, MessengerSearch, MessengerSource,
    Message, MsgId, SearchHit, Thread, User, Window,
};
use super::super::messenger::MessengerFs;
use crate::BoxFuture;
use crate::fs::{FileSystem, Hit, Searchable};

/// `2026-08-03T06:17:55Z`, the instant a real workspace's thread was started on.
const ROOT_TS: u64 = 1_785_737_875;

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn conv(name: &str, id: &str, kind: ConvKind) -> Conversation {
    Conversation {
        // The date axis is generated from this, so a fixture has to stand somewhere. A day
        // before the messages, which is the only relation that matters here.
        created: at(ROOT_TS - 86_400),
        id: ConvId(id.into()),
        name: name.into(),
        kind,
    }
}

fn msg(ts: u64, text: &str, thread: Option<Thread>) -> Message {
    Message {
        id: MsgId(format!("{ts}.000000")),
        ts: at(ts),
        from: Author {
            id: "U1".into(),
            name: None,
            claimed: None,
        },
        text: text.into(),
        thread,
        files: Vec::new(),
        raw: serde_json::Value::Null,
    }
}

/// What a fake source does when its index is asked.
#[derive(Default, PartialEq)]
enum Index {
    /// It has one and it answers.
    #[default]
    Answers,
    /// It has one and this credential is refused it — a request is spent to find out.
    Refuses,
    /// It has none at all, so nothing may ask.
    Absent,
}

#[derive(Default)]
struct Fake {
    hits: Vec<SearchHit>,
    convs: Vec<Conversation>,
    users: Vec<User>,
    index: Index,
    /// What the tree actually asked the source for, so a claim about cost is asserted rather
    /// than asserted-to.
    asked: StdArc<Asked>,
}

#[derive(Default)]
struct Asked {
    users: AtomicUsize,
    convs: AtomicUsize,
}

impl MessengerSource for Fake {
    fn conversations<'a>(&'a self) -> BoxFuture<'a, SourceResult<Vec<Conversation>>> {
        Box::pin(async move {
            self.asked.convs.fetch_add(1, Ordering::SeqCst);
            Ok(self.convs.clone())
        })
    }

    fn history<'a>(
        &'a self,
        _conv: &'a ConvId,
        _window: Window,
    ) -> BoxFuture<'a, SourceResult<Vec<Message>>> {
        Box::pin(async move { Ok(Vec::new()) })
    }

    fn thread<'a>(
        &'a self,
        _conv: &'a ConvId,
        _root: &'a MsgId,
    ) -> BoxFuture<'a, SourceResult<Vec<Message>>> {
        Box::pin(async move { Ok(Vec::new()) })
    }

    fn users<'a>(&'a self) -> BoxFuture<'a, SourceResult<Vec<User>>> {
        Box::pin(async move {
            self.asked.users.fetch_add(1, Ordering::SeqCst);
            Ok(self.users.clone())
        })
    }

    fn fetch_file<'a>(
        &'a self,
        _file: &'a FileRef,
        _range: Option<std::ops::Range<u64>>,
    ) -> BoxFuture<'a, SourceResult<(Vec<u8>, bool)>> {
        Box::pin(async move { Ok((Vec::new(), false)) })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            channels: true,
            dms: true,
        }
    }

    fn searchable(&self) -> Option<&dyn MessengerSearch> {
        (self.index != Index::Absent).then_some(self as &dyn MessengerSearch)
    }
}

impl MessengerSearch for Fake {
    fn search<'a>(
        &'a self,
        _query: &'a str,
        _count: usize,
    ) -> BoxFuture<'a, SourceResult<Vec<SearchHit>>> {
        Box::pin(async move {
            match self.index {
                Index::Refuses => Err(SourceError::io("slack search needs a user token (xoxp-)")),
                _ => Ok(self.hits.clone()),
            }
        })
    }
}

async fn hits(fake: Fake) -> Result<Vec<Hit>, String> {
    MessengerFs::new(fake).search("가격", 20).await
}

fn one(hits: &[Hit]) -> (&str, String) {
    (
        hits[0].path.as_str(),
        String::from_utf8(hits[0].record.clone()).unwrap(),
    )
}

#[tokio::test]
async fn a_hit_names_the_day_file_it_can_be_read_in() {
    let found = hits(Fake {
        hits: vec![SearchHit {
            conv: conv("pricing", "C1", ConvKind::Channel),
            msg: msg(ROOT_TS, "발표영상", None),
            thread_started: None,
        }],
        convs: vec![conv("pricing", "C1", ConvKind::Channel)],
        ..Default::default()
    })
    .await
    .expect("searched");

    let (path, record) = one(&found);
    assert_eq!(path, "channels/pricing__C1/2026/08/2026-08-03.jsonl");
    assert!(record.contains("\"text\":\"발표영상\""), "{record}");
    assert!(record.ends_with('\n'), "a record is a line of that file");
}

/// The one a caller could not work out for itself: a reply is in its thread's file, under the
/// day the thread was *started*, not the day the reply was written.
#[tokio::test]
async fn a_reply_names_its_threads_file_under_the_roots_day() {
    let found = hits(Fake {
        hits: vec![SearchHit {
            conv: conv("pricing", "C1", ConvKind::Channel),
            msg: msg(
                // A month later than its root, deliberately: `chat_path` spends `day` on the
                // month directory alone, so a reply written in the same month as its root
                // produces the same path whether the day came from the thread or from the
                // reply. Same-month, this test passes against `day = hit.msg.ts`.
                ROOT_TS + 3_000_000,
                "좋은데?",
                Some(Thread {
                    root: Some(MsgId("1785737875.341929".into())),
                    replies: 0,
                }),
            ),
            thread_started: Some(at(ROOT_TS)),
        }],
        convs: vec![conv("pricing", "C1", ConvKind::Channel)],
        ..Default::default()
    })
    .await
    .expect("searched");

    assert_eq!(
        one(&found).0,
        "channels/pricing__C1/2026/08/threads/1785737875.341929.jsonl",
        "the thread's day, not the reply's"
    );
}

/// An index may know a conversation by less than the listing does — a DM with no display name.
/// The listing is the authority on the human half; the id half is the address either way.
#[tokio::test]
async fn the_listings_name_wins_over_the_indexs() {
    let found = hits(Fake {
        hits: vec![SearchHit {
            conv: conv("", "D1", ConvKind::Dm),
            msg: msg(ROOT_TS, "안녕", None),
            thread_started: None,
        }],
        convs: vec![conv("김철수", "D1", ConvKind::Dm)],
        ..Default::default()
    })
    .await
    .expect("searched");

    assert!(
        one(&found).0.starts_with("dms/김철수__D1/"),
        "{}",
        one(&found).0
    );
}

#[tokio::test]
async fn an_unlisted_conversation_still_gets_a_path() {
    let found = hits(Fake {
        hits: vec![SearchHit {
            conv: conv("", "D9", ConvKind::Dm),
            msg: msg(ROOT_TS, "안녕", None),
            thread_started: None,
        }],
        ..Default::default()
    })
    .await
    .expect("searched");

    assert!(
        one(&found).0.starts_with("dms/unnamed__D9/"),
        "{}",
        one(&found).0
    );
}

#[tokio::test]
async fn an_author_is_named_from_the_roster() {
    let found = hits(Fake {
        hits: vec![SearchHit {
            conv: conv("pricing", "C1", ConvKind::Channel),
            msg: msg(ROOT_TS, "발표영상", None),
            thread_started: None,
        }],
        users: vec![User {
            id: "U1".into(),
            name: "로리우셀".into(),
            record: serde_json::Value::Null,
        }],
        ..Default::default()
    })
    .await
    .expect("searched");

    assert!(
        one(&found).1.contains("\"name\":\"로리우셀\""),
        "{}",
        one(&found).1
    );
}

/// A refusal is the service's own sentence, carried up for a fan-out to report.
#[tokio::test]
async fn an_index_that_refuses_is_an_error_and_says_why() {
    let err = hits(Fake {
        index: Index::Refuses,
        ..Default::default()
    })
    .await
    .expect_err("this credential cannot search");
    assert!(err.contains("user token"), "{err}");
}

/// Having no index and being refused one are different answers, and the difference is a
/// request: a mount whose source has none is never asked at all.
#[tokio::test]
async fn a_source_with_no_index_offers_the_tree_none() {
    let has = MessengerFs::new(Fake::default());
    assert!(has.searchable().is_some(), "this source answers its index");

    let has_not = MessengerFs::new(Fake {
        index: Index::Absent,
        ..Default::default()
    });
    assert!(
        has_not.searchable().is_none(),
        "a source with no index must not look like one that has an empty one"
    );
}

/// Two searches cost one roster, not two — and a listing before them costs none of it again.
///
/// The point of the impl being on the tree rather than beside it. A source keeps nothing between
/// calls, so without the store's own cache every query pays for the roster, and on a workspace
/// of any size the roster is most of what a query costs.
#[tokio::test]
async fn searches_share_the_stores_cache() {
    let asked = StdArc::new(Asked::default());
    let store = MessengerFs::new(Fake {
        hits: vec![SearchHit {
            conv: conv("pricing", "C1", ConvKind::Channel),
            msg: msg(ROOT_TS, "발표영상", None),
            thread_started: None,
        }],
        convs: vec![conv("pricing", "C1", ConvKind::Channel)],
        users: vec![User {
            id: "U1".into(),
            name: "kim".into(),
            record: serde_json::Value::Null,
        }],
        asked: asked.clone(),
        ..Default::default()
    });

    // A directory listing first, because that is the order a reader arrives in: they were
    // reading the tree and then asked it a question.
    store.list(std::path::Path::new("channels")).await.expect("listed");
    for _ in 0..3 {
        store.search("가격", 20).await.expect("searched");
    }

    let seen = |c: &AtomicUsize| c.load(Ordering::SeqCst);
    assert_eq!(seen(&asked.convs), 1, "a listing and three searches, one listing fetch");
    assert_eq!(seen(&asked.users), 1, "three searches, one roster");
}
