//! What a conversation leaves behind: the memories worth keeping out of it.
//!
//! # Why the input is a conversation and not a sentence
//!
//! A memory store could take a sentence per call and be simpler for it. It would also be
//! wrong about where memories come from. What a person tells an assistant is spread across
//! turns — the fact is in one message, the date it happened is in the one before, and the
//! pronoun that names it is in the one after — so a single sentence pulled out of that is a
//! sentence somebody already decided about, and deciding is the whole job here. So the input
//! is the conversation, as a list of [`Message`](ailoy::message::Message)s — turns of a role
//! and what was said, which
//! is the shape a chat has everywhere — and this module reads it the way a person taking notes
//! afterwards would.
//!
//! Only `user` and `assistant` turns are read. A `system` turn is the caller's instruction to
//! a model, not something anybody said, and a `tool` turn is machine output that the
//! assistant will have restated in its own words if it mattered — remembering either would
//! mean the store held the scaffolding of a conversation instead of its content.
//!
//! # Why one call and not two
//!
//! The extraction here is additive: it answers with memories to add and nothing else. It is
//! shown the memories the store already holds, but only so it can decline to restate one —
//! never to rewrite or retire them, and never to say anything *about* them. That makes this
//! half of `insert` a single model call whose every answer is a row to write, which is why
//! nothing in it can go wrong halfway.
//!
//! What that gives up is retirement: a fact that contradicts a held one leaves both, and
//! reconciling them is separate work that reads what this wrote. The trade is deliberate —
//! a store that grows and is occasionally redundant is recoverable, where a store that let a
//! model delete rows on the strength of one judgement is not.

use std::{borrow::Cow, collections::BTreeMap, str::FromStr};

use ailoy::{
    agent::{AgentBuilder, AgentProvider, get_agent_providers_mut},
    lang_model::{LangModelProvider, LangModelProviderElem, ResponseFormat, get_lm_providers_mut},
    message::{Message, Part, Role},
};
use anyhow::{Context as _, anyhow, bail};
use futures::StreamExt as _;
use serde::Deserialize;

use crate::memory::Memory;

/// The extraction instructions, as they are actually sent.
///
/// mem0's `ADDITIVE_EXTRACTION_PROMPT` — `mem0/configs/prompts.py` in `mem0ai/mem0`, taken
/// from `main` on 2026-08-20 — with the edits below and no others.
///
/// Two fields are gone from the answer, and with them the sections that described them.
///
/// `attributed_to` — the side of the conversation a memory is credited to — went because a
/// memory here is text and nothing else; [`memory`](crate::memory) argues that at length.
/// Upstream does read the field, unlike the next one: it becomes payload beside the memory's
/// text and its read APIs surface it. Beside the text is the point, though. It describes where a
/// row came from rather than what is remembered, so a crate whose [`Memory`] is deliberately not
/// a row has nowhere to put it, and asking for it anyway would be a required key per memory that
/// arrives with nothing to receive it.
///
/// `linked_memory_ids` went for the plainer reason that upstream asks for it and upstream's own
/// code never reads it — every `linked_memory_ids` in mem0's store is on an *entity* record,
/// built by its entity extractor rather than answered by the model. So the field was a section
/// of instructions and a required key spent on an answer nobody looked at.
///
/// Neither removal was optional once the field left [`output_schema`]: strict mode forbids
/// properties the schema does not name, so a model still obeying the section that asked for one
/// would have had its whole answer rejected.
///
/// The language a memory is written in is not an input either. Upstream is handed a `Language`
/// tag per call and writes its memories in it; here every memory is English, so the section that
/// named the tag, the guideline that pointed at it and the example that demonstrated writing in
/// another language are one guideline instead — write English, and keep names, titles, quoted text
/// and identifiers in the script they were written in. A tag would be a knob two conversations set
/// two ways, and a store whose memories are half in one language answers a search well in neither.
///
/// # The inputs it does not have
///
/// `# INPUTS` describes four sections, and [`user_prompt`] sends four. Upstream describes eight,
/// and the four that are gone are the ones nothing here could fill: `Summary`,
/// `Recently Extracted Memories`, `Last k Messages`, and the optional
/// `includes`/`excludes`/`custom_instructions`/`feedback_str`. A prompt that describes an input
/// it is never given is not a section that happens to be empty — it is an instruction the model
/// cannot follow, and it pays for the words twice, once to describe the input and once to
/// wonder where it went.
///
/// `Last k Messages` is the one that could not be filled even in principle: it is the window of
/// turns *preceding* the ones being read, and `insert` is handed a conversation rather than a
/// position in one. What it was for — resolving a pronoun against what came before — the
/// conversation does itself, because all of it is sent. With it gone there is one set of
/// messages and nothing to tell them apart from, which is why `## Messages` is not
/// `## New Messages`.
///
/// `Recently Extracted Memories` is the one worth arguing. Upstream keeps a session's own
/// deduplication list beside the store's, for a reading split across several calls. Here the two
/// lists would be one: a conversation is read in a single call, and what the store holds near it
/// is already `## Existing Memories`. There is no earlier chunk of this same reading whose
/// memories have not landed yet.
///
/// `Summary` would be a profile written by something that does not exist. Nothing in this crate
/// summarises a user, and an empty section under instructions that say to enrich extractions
/// with it is worse than no section at all.
///
/// # Edits to prose
///
/// `# ROLE` no longer opens with a persona and "your sole operation is ADD", because there is
/// one operation here — naming it the sole one describes a set of alternatives this prompt was
/// never given. And `## Messages` now says which two fields of a turn are the input, `role` and
/// `content` and nothing else, because a conversation in this workspace carries more than that:
/// a message has `thinking` and `tool_calls` beside its contents, and a `Tool` turn is a whole
/// role of machine output. [`turns`] and [`flatten`] drop all of it before anything is sent, so
/// the sentence is not what makes the input clean — it is what keeps a model from reading such
/// material as something somebody said in the case where it arrives inside a turn's text anyway.
///
/// `## Existing Memories` shows its ids as `"0"` because that is what [`offer`] sends, where
/// upstream shows a UUID. The examples lost the two input lines they carried for sections that
/// are gone, and are numbered without the gap upstream has.
///
/// Nothing else is changed, and the prompt is kept as a file rather than a string literal, so
/// that it stays diffable against the upstream one: a prompt quietly improved in a dozen places
/// is a prompt nobody can tell apart from the version it was tested as.
const INSTRUCTIONS: &str = include_str!("../prompts/EXTRACTION.md");

/// The provider used when `$MEM_LLM_PROVIDER` says nothing.
const DEFAULT_PROVIDER: &str = "openai";

/// The memories in `conversation` that the store does not already hold.
///
/// `existing` is the memories the store already holds near this conversation, as text — the
/// neighbourhood, not the store. It is shown so that the extraction can decline to restate one,
/// and for nothing else: no id is sent and nothing in the answer refers back to it. An
/// extraction cannot conclude from it that a fact is new, only that it is not near anything
/// held — which is the same conclusion for a store that keeps related things together. Pass an
/// empty slice and every memory found is offered as new, which is the correct behaviour for the
/// first write to a store and a wasteful one for the thousandth.
///
/// An empty answer is a real answer: a conversation can carry nothing worth keeping, and a
/// caller that gets one writes nothing rather than writing the transcript for want of
/// anything better. A conversation with no readable turns at all is answered without asking a
/// model, since there is nothing for it to read.
///
/// `env` is the environment the call was made in —
/// [`ExecCall::env`](cortex::exec::ExecCall::env) — and not this process's, because the two are
/// not the same environment. Run as a program they hold the same variables and the distinction
/// costs nothing; delegated in a console they are, in cortex's own words, "different machines'
/// worth of facts", and the provider and the key that pays for a call belong to the session that
/// asked for it rather than to whoever started the server that answers.
pub async fn extract_memories(
    conversation: &[Message],
    existing: &[String],
    env: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<Memory>> {
    let turns = turns(conversation);
    if turns.is_empty() {
        return Ok(Vec::new());
    }

    // Held to the end of the call: dropping the registration takes the provider back out of
    // ailoy's registries, and the agent resolves its model through them on every run.
    let (model, registered) = asked(env)?;
    let offered = offer(existing);

    let mut builder = AgentBuilder::new(&model)
        .instruction(INSTRUCTIONS)
        // Structured output rather than a parse of prose. The instructions end in a JSON
        // shape and would mostly be obeyed without this, but "mostly" here means an
        // occasional turn whose memories are unrecoverable, and the schema is the same
        // sentence said in a way the provider enforces.
        .response_format(ResponseFormat::json_schema(output_schema().into())?);
    if let Some(registered) = &registered {
        builder = builder.agent_provider(registered.name());
    }
    let mut agent = builder
        .build()
        .with_context(|| format!("building an extraction agent for `{model}`"))?;

    let query = Message::new(Role::User).with_contents([Part::text(user_prompt(&turns, &offered))]);

    let mut answer: Option<String> = None;
    {
        let mut turn = agent.run(query);
        while let Some(output) = turn.next().await {
            let output = output.with_context(|| format!("asking `{model}` for memories"))?;
            if output.message.role != Role::Assistant {
                continue;
            }
            let text: String = output
                .message
                .contents
                .iter()
                .filter_map(|part| part.as_text())
                .collect();
            if !text.is_empty() {
                answer = Some(text);
            }
        }
    }
    let answer = answer.ok_or_else(|| anyhow!("`{model}` answered with no text"))?;

    Ok(answer.parse::<Answer>()?.memory)
}

/// A turn as the instructions read one: who spoke, and what they said.
///
/// A role and what was said, which is what the instructions show themselves reading — so a turn
/// is that and nothing else, and its `Serialize` is how it reaches them. There is only one shape
/// a turn has here, and a type that existed to be converted into this one on the next line would
/// be a second spelling of it.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct Turn {
    role: &'static str,
    content: String,
}

/// The conversation's readable turns, in order and in the form they are sent.
///
/// A turn contributes nothing when it has no text after flattening — an assistant message that
/// is only a tool call, say — and is dropped rather than sent as an empty string, which the
/// model would have to guess the meaning of.
fn turns(conversation: &[Message]) -> Vec<Turn> {
    conversation
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                // See the module docs: neither is something anybody said.
                Role::System | Role::Tool => return None,
            };
            let content = flatten(message);
            (!content.is_empty()).then_some(Turn { role, content })
        })
        .collect()
}

/// The conversation as one piece of text, for asking a store what it already holds near it.
///
/// Built out of [`turns`] rather than out of the messages, so that what the store is asked about
/// is the same text the extraction will be shown. The two are one question in two halves — "what
/// do you already hold near this?" and "what is new in this?" — and asking them about different
/// text is how a neighbourhood comes back that has nothing to do with what the model then reads.
/// A system turn is dropped from both for the same reason: it is the caller's instruction to a
/// model, and searching a store for it would answer with whatever memories happen to share its
/// wording.
///
/// The roles are left out. What is wanted from this is terms, and `user` and `assistant` are two
/// words that every conversation contains and no memory is about.
pub fn as_query(conversation: &[Message]) -> String {
    turns(conversation)
        .into_iter()
        .map(|turn| turn.content)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A message's parts as one string.
///
/// An image becomes a marker rather than being dropped, because the instructions treat a
/// shared photo as something to extract from and the surrounding text often only makes sense
/// as a caption. What the marker cannot do is say what the image showed — that would need the
/// image on the wire, which is a decision about cost that belongs to whoever configures the
/// model, not to a flattening function.
fn flatten(message: &Message) -> String {
    let mut out = String::new();
    for part in &message.contents {
        let piece: Cow<'_, str> = match part {
            Part::Text { text } => text.trim().into(),
            Part::Image { .. } => "[Shared image]".into(),
            Part::Value { value } => serde_json::Value::from(value.clone()).to_string().into(),
            // A function part is a call, not content: the instructions have nothing to say
            // about one and its arguments are an implementation detail of some tool.
            Part::Function { .. } => continue,
        };
        if piece.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&piece);
    }
    out
}

/// The held memories as the model sees them.
///
/// The ids are positions — `"0"`, `"1"`, … — and not the store's own, because the store's own
/// would be a dozen tokens of UUID each for something nothing reads back. The instructions
/// describe this section as a list of `{"id", "text"}` and are sent unedited, so the shape is
/// kept and the id is filled with the only number that costs nothing.
fn offer(existing: &[String]) -> String {
    let listed: Vec<_> = existing
        .iter()
        .enumerate()
        .map(|(i, text)| serde_json::json!({ "id": i.to_string(), "text": text }))
        .collect();
    serde_json::to_string(&listed).unwrap_or_else(|_| "[]".to_string())
}

/// The sections the instructions name, all of them, in the order they are named.
///
/// All of them and no others is the property worth having, and it is asserted rather than
/// maintained by hand: see [`INSTRUCTIONS`] on why a section described and not sent is worse
/// than one that was never described.
///
/// `Observation Date` and `Current Date` are both today. The distinction the instructions
/// draw between them is real and worth keeping the sections for — a conversation imported
/// from six months ago must have its "last week" resolved against when it happened, not
/// against now — but `insert` is told a conversation and not when it was held, so today is
/// the only honest answer to both. Threading a real observation date through is a change to
/// what `insert` accepts, and until then saying "today" twice is at least not a lie.
fn user_prompt(turns: &[Turn], offered: &str) -> String {
    let today = chrono::Utc::now().date_naive();
    let messages = serde_json::to_string(turns).unwrap_or_else(|_| "[]".to_string());

    [
        format!("## Existing Memories\n{offered}"),
        format!("## Messages\n{messages}"),
        format!("## Observation Date\n{today}"),
        format!("## Current Date\n{today}"),
        "# Output:".to_string(),
    ]
    .join("\n\n")
}

/// The shape an answer must take.
///
/// Every property is required, because OpenAI's strict mode admits no optional ones at all —
/// which is also why the instructions must not describe a field this omits: `additionalProperties`
/// is `false` under strict mode, so a model obeying a prompt that asked for one more key would
/// have its whole answer rejected. `additionalProperties` itself is left off on purpose: ailoy's
/// `ResponseSchemaMarshal` adds it per provider, and setting it here would mean deciding for
/// providers whose rules this crate does not track.
fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "memory": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "text": { "type": "string" }
                    },
                    "required": ["id", "text"]
                }
            }
        },
        "required": ["memory"]
    })
}

/// The envelope the instructions wrap an answer's memories in.
///
/// An `Answer` only ever comes from a model's text, so it is built by parsing one rather than by
/// a constructor of its own, and `Deserialize` is the inner step of that parse and not a second
/// way in: what `serde_json` produces is a shape the answer had, where an `Answer` is one whose
/// memories are all memories.
#[derive(Debug, Deserialize)]
struct Answer {
    memory: Vec<Memory>,
}

impl FromStr for Answer {
    type Err = anyhow::Error;

    fn from_str(answer: &str) -> anyhow::Result<Self> {
        let answer = answer.trim();
        if answer.is_empty() {
            bail!("the answer was empty");
        }
        let mut parsed: Answer = serde_json::from_str(answer)
            .with_context(|| format!("reading the answer as memories: {answer:.400}"))?;

        // Trimmed before the emptiness is judged, so that a memory of nothing but whitespace is
        // the same as no memory at all.
        for memory in &mut parsed.memory {
            memory.text = memory.text.trim().to_string();
        }
        parsed.memory.retain(|memory| !memory.text.is_empty());
        Ok(parsed)
    }
}

/// The model to ask, and a provider registered to ask it with — both out of `env`.
///
/// `$MEM_LLM_PROVIDER` names a provider — `openai` unless it says otherwise — and picks that
/// provider's default model. `$MEM_LLM_MODEL` overrides the model outright, either fully
/// qualified (`anthropic/claude-sonnet-4-6`) or bare (`gpt-4o`), in which case the provider
/// qualifies it. The provider is a separate variable from the model because the common case is
/// a caller who has one API key and no opinion about which model spends it.
///
/// `None` for the registration where this crate has no key variable for the provider: the model
/// is then resolved against ailoy's own `"default"` registry, which is the component that knows
/// about providers this one does not.
fn asked(env: &BTreeMap<String, String>) -> anyhow::Result<(String, Option<Registered>)> {
    let model = qualify(
        env.get("MEM_LLM_PROVIDER").map(String::as_str),
        env.get("MEM_LLM_MODEL").map(String::as_str),
    )?;

    let provider = model.split_once('/').map_or(model.as_str(), |(p, _)| p);

    // Where the key is read from and which endpoint it is handed to, decided together. One
    // match and not two, because a build that read `$ANTHROPIC_API_KEY` and sent it to OpenAI
    // would be a credential leak that compiles. A provider not listed here is left to ailoy,
    // which is the component that knows about it — nothing is refused on its behalf.
    let (var, endpoint): (&str, fn(String) -> LangModelProviderElem) = match provider {
        "openai" => ("OPENAI_API_KEY", LangModelProvider::openai),
        "anthropic" => ("ANTHROPIC_API_KEY", LangModelProvider::anthropic),
        "google" => ("GEMINI_API_KEY", LangModelProvider::gemini),
        "x-ai" => ("XAI_API_KEY", LangModelProvider::grok),
        "deepseek" => ("DEEPSEEK_API_KEY", LangModelProvider::deepseek),
        "moonshotai" => ("KIMI_API_KEY", LangModelProvider::kimi),
        _ => return Ok((model, None)),
    };

    // Refused here rather than by ailoy, which registers a provider only for keys that are
    // present and so reports a missing one as "no provider found for model X" — true, and no
    // help at all to somebody who has not exported anything.
    let Some(key) = env.get(var).map(|k| k.trim()).filter(|k| !k.is_empty()) else {
        bail!("${var} is not set, so `{model}` cannot be asked anything")
    };

    let registered = Registered::new(provider, endpoint(key.to_string()));
    Ok((model, Some(registered)))
}

/// A provider in ailoy's registries for the length of one call, and gone after it.
///
/// # Why anything is registered at all
///
/// ailoy resolves a model through process-wide registries, by name — there is no constructor
/// that takes a provider by value. Its `"default"` entry is built from *this process's*
/// environment, which is exactly the environment an [`Executable`](cortex::exec::Executable)
/// must not use: delegated in a console, the key that should pay for the call is the one the
/// calling session had, and the one in this process belongs to whoever started the server. So
/// the key from [`ExecCall::env`](cortex::exec::ExecCall::env) is registered under a name of
/// this call's own, and the agent is pointed at that name instead of `"default"`.
///
/// # Why the name is unique and the entry is removed
///
/// A registry is process-wide and a console server answers more than one session. A fixed name
/// would mean two calls with two different keys writing over each other, and the loser paying
/// for the winner's model — so each call gets a name nobody else will pick. Removing it on drop
/// is what keeps that from being a map that grows for the life of the server, and it happens on
/// every path out of the call because it is a destructor and not a step.
/// `Debug` is safe to derive because what this holds is a registry name and never the key: the
/// key went into ailoy's registry and nothing here keeps a copy to print by accident.
#[derive(Debug)]
struct Registered(String);

impl Registered {
    fn new(provider: &str, elem: LangModelProviderElem) -> Self {
        let name = format!("mem-{}", uuid::Uuid::new_v4());

        let mut models = LangModelProvider::new();
        // The glob the provider prefix implies: the model was qualified with this prefix
        // above, so it is the pattern that matches it and nothing else is registered here.
        models.insert(format!("{provider}/*"), elem);

        // Two registries, two locks, taken one at a time: a write guard held while reaching
        // for the second is a deadlock waiting for the call that takes them the other way.
        get_lm_providers_mut().insert(name.clone(), models);
        get_agent_providers_mut().insert(name.clone(), AgentProvider::new(&name, "default"));

        Self(name)
    }

    fn name(&self) -> &str {
        &self.0
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        // Reverse order of the two inserts, for the same reason they were taken one at a time.
        get_agent_providers_mut().remove(&self.0);
        get_lm_providers_mut().remove(&self.0);
    }
}

/// A fully qualified model name, or why one could not be had.
fn qualify(provider: Option<&str>, model: Option<&str>) -> anyhow::Result<String> {
    let provider = provider
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(DEFAULT_PROVIDER);

    if let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) {
        return Ok(if model.contains('/') {
            model.to_string()
        } else {
            format!("{provider}/{model}")
        });
    }

    // A default model is a claim that a particular name exists at a particular provider, and
    // that claim goes stale. Rather than guess for a provider whose catalogue this crate does
    // not follow, refuse and say which variable settles it.
    match provider {
        "openai" => Ok("openai/gpt-4o-mini".to_string()),
        "anthropic" => Ok("anthropic/claude-sonnet-4-6".to_string()),
        "google" => Ok("google/gemini-2.5-flash".to_string()),
        other => bail!(
            "no default model for provider `{other}`: name one in $MEM_LLM_MODEL, or set \
             $MEM_LLM_PROVIDER to one of openai, anthropic, google"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, text: &str) -> Message {
        Message::new(role).with_contents([Part::text(text)])
    }

    #[test]
    fn only_the_two_sides_that_said_something_are_read() {
        let conversation = [
            msg(Role::System, "You are a helpful assistant."),
            msg(Role::User, "  I switched to oat milk.  "),
            msg(Role::Tool, "{\"ok\":true}"),
            msg(Role::Assistant, "Noted."),
        ];
        assert_eq!(
            turns(&conversation),
            [
                Turn {
                    role: "user",
                    content: "I switched to oat milk.".into()
                },
                Turn {
                    role: "assistant",
                    content: "Noted.".into()
                },
            ]
        );
    }

    /// The same turns, as the text a store is asked about — and without the roles, which every
    /// conversation contains and no memory is about.
    #[test]
    fn a_conversation_asks_a_store_about_what_was_said_in_it() {
        let conversation = [
            msg(Role::System, "You are a helpful assistant."),
            msg(Role::User, "I switched to oat milk."),
            msg(Role::Assistant, "Noted."),
        ];
        assert_eq!(
            as_query(&conversation),
            "I switched to oat milk.\nNoted.",
            "the instruction to the model is not something anybody said"
        );
        assert_eq!(as_query(&[]), "");
        assert_eq!(
            as_query(&[msg(Role::System, "Be helpful.")]),
            "",
            "nothing was said, so there is nothing to ask a store about"
        );
    }

    /// A message whose parts flatten to nothing is dropped rather than sent as an empty
    /// turn — a tool call carries no content for an extraction to read.
    #[test]
    fn a_turn_with_nothing_in_it_is_not_a_turn() {
        let calling = Message::new(Role::Assistant).with_contents([Part::function(
            "call-1",
            "search",
            ailoy::datatype::Value::from(serde_json::json!({})),
        )]);
        assert!(turns(&[calling]).is_empty());
        assert!(turns(&[msg(Role::User, "   ")]).is_empty());
    }

    #[test]
    fn an_image_is_a_marker_beside_its_caption() {
        let shared = Message::new(Role::User).with_contents([
            Part::image_url("https://example.com/a.png".to_string()).unwrap(),
            Part::text("this is my dog Poppy"),
        ]);
        assert_eq!(
            flatten(&shared),
            "[Shared image]\nthis is my dog Poppy",
            "the caption alone would not say there was a photo"
        );
    }

    #[test]
    fn held_memories_are_offered_by_position() {
        let existing = [
            "User has a dog named Poppy".to_string(),
            "User lives in Seoul".to_string(),
        ];
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&offer(&existing)).unwrap(),
            serde_json::json!([
                { "id": "0", "text": "User has a dog named Poppy" },
                { "id": "1", "text": "User lives in Seoul" },
            ])
        );
        assert_eq!(offer(&[]), "[]");
    }

    #[test]
    fn an_answer_becomes_memories() {
        let answer = r#"{"memory": [
            {"id": "0", "text": "  User switched to oat milk  "},
            {"id": "1", "text": "User was recommended a barista blend"}
        ]}"#;
        assert_eq!(
            answer.parse::<Answer>().unwrap().memory,
            [
                Memory {
                    text: "User switched to oat milk".into(),
                },
                Memory {
                    text: "User was recommended a barista blend".into(),
                },
            ]
        );
    }

    /// Nothing worth remembering is a real answer, and the one the instructions ask for by
    /// name — not an error and not the transcript.
    #[test]
    fn an_empty_answer_is_no_memories() {
        assert!(r#"{"memory": []}"#.parse::<Answer>().unwrap().memory.is_empty());
    }

    #[test]
    fn a_memory_with_no_text_is_not_a_memory() {
        let answer = r#"{"memory": [{"id": "0", "text": "   "}]}"#;
        assert!(answer.parse::<Answer>().unwrap().memory.is_empty());
    }

    /// A key the schema does not name is the model having ignored the schema, which is no
    /// reason to lose the memory that came with it — `id` is already such a key, asked for by
    /// the instructions and read by nothing here.
    #[test]
    fn a_key_that_is_not_a_memorys_is_ignored() {
        let answer = r#"{"memory": [{"id": "0", "text": "User drinks tea", "role": "user"}]}"#;
        assert_eq!(
            answer.parse::<Answer>().unwrap().memory,
            [Memory {
                text: "User drinks tea".into()
            }]
        );
    }

    #[test]
    fn prose_is_not_an_answer() {
        assert!("I could not find any memories.".parse::<Answer>().is_err());
        assert!("".parse::<Answer>().is_err());
    }

    #[test]
    fn the_schema_is_one_a_provider_will_take() {
        ResponseFormat::json_schema(output_schema().into())
            .expect("the output schema must be valid JSON Schema");
    }

    /// The inputs the instructions describe and the sections the prompt sends are the same set,
    /// asserted in both directions.
    ///
    /// Read off [`INSTRUCTIONS`] rather than listed here, because a list written in this file is
    /// a third copy of the same fact and drifts from both: a section renamed in the prompt file
    /// and not in [`user_prompt`] would leave a hand-written list passing, describing an input
    /// the model is never sent. What the two directions catch are the two ways that goes wrong —
    /// an input described and not supplied, and a section supplied that nothing explains.
    #[test]
    fn the_prompt_sends_every_input_the_instructions_describe_and_no_other() {
        // The `## ` headings under `# INPUTS`, up to wherever the next top-level heading starts.
        let described: Vec<&str> = INSTRUCTIONS
            .lines()
            .skip_while(|line| line.trim() != "# INPUTS")
            .take_while(|line| !line.starts_with("# GUIDELINES"))
            .filter(|line| line.starts_with("## "))
            .collect();
        assert!(
            !described.is_empty(),
            "the instructions describe their inputs under `# INPUTS`"
        );

        let turns = turns(&[msg(Role::User, "I switched to oat milk")]);
        let prompt = user_prompt(&turns, "[]");
        let sent: Vec<&str> = prompt
            .lines()
            .filter(|line| line.starts_with("## "))
            .collect();

        for section in &described {
            assert!(
                sent.contains(section),
                "the instructions describe `{section}` and the prompt does not send it: {sent:?}"
            );
        }
        for section in &sent {
            assert!(
                described.contains(section),
                "the prompt sends `{section}` and the instructions do not describe it: \
                 {described:?}"
            );
        }

        // Not an input, so it is not in the set above — and the one line the model is told to
        // answer after.
        assert!(prompt.contains("# Output:"));
        assert!(prompt.contains(r#"{"role":"user","content":"I switched to oat milk"}"#));
        assert!(
            !prompt.contains("Language"),
            "no section names a language: every memory is English, and an input that could say \
             otherwise is one the instructions no longer read"
        );
    }

    /// The inputs upstream fills from a service and this crate cannot: gone from the
    /// instructions, not sent empty. See [`INSTRUCTIONS`].
    #[test]
    fn the_instructions_describe_no_input_this_crate_has_nothing_to_put_in() {
        for absent in [
            "## Summary",
            "## Recently Extracted Memories",
            "## Last k Messages",
            "## Optional Inputs",
        ] {
            assert!(
                !INSTRUCTIONS.contains(absent),
                "`{absent}` is an input nothing here supplies"
            );
        }
    }

    #[test]
    fn the_provider_defaults_and_the_model_follows_from_it() {
        assert_eq!(qualify(None, None).unwrap(), "openai/gpt-4o-mini");
        assert_eq!(
            qualify(Some("anthropic"), None).unwrap(),
            "anthropic/claude-sonnet-4-6"
        );
        assert_eq!(qualify(Some("  "), None).unwrap(), "openai/gpt-4o-mini");
    }

    #[test]
    fn a_bare_model_is_qualified_by_the_provider() {
        assert_eq!(
            qualify(Some("openai"), Some("gpt-4o")).unwrap(),
            "openai/gpt-4o"
        );
        assert_eq!(
            qualify(Some("openai"), Some("anthropic/claude-sonnet-4-6")).unwrap(),
            "anthropic/claude-sonnet-4-6",
            "a qualified model names its own provider"
        );
    }

    #[test]
    fn a_provider_with_no_default_says_which_variable_settles_it() {
        let e = qualify(Some("deepseek"), None).unwrap_err().to_string();
        assert!(e.contains("MEM_LLM_MODEL"), "{e}");
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// The key is read from what was passed, and its absence says which variable to set.
    #[test]
    fn a_provider_with_no_key_in_this_environment_says_which_one_is_missing() {
        let e = asked(&env(&[("MEM_LLM_PROVIDER", "anthropic")]))
            .unwrap_err()
            .to_string();
        assert!(e.contains("$ANTHROPIC_API_KEY"), "{e}");
        assert!(e.contains("anthropic/claude-sonnet-4-6"), "{e}");

        // Present but blank is absent: an exported-and-empty variable is the shape a shell
        // leaves behind, and treating it as a key would send an unauthenticated request.
        let e = asked(&env(&[
            ("ANTHROPIC_API_KEY", "  "),
            ("MEM_LLM_PROVIDER", "anthropic"),
        ]))
        .unwrap_err()
        .to_string();
        assert!(e.contains("$ANTHROPIC_API_KEY"), "{e}");
    }

    /// A key here registers a provider of this call's own, and dropping it takes that provider
    /// back out — so nothing is left in ailoy's process-wide registries afterwards.
    #[test]
    fn a_key_is_registered_for_one_call_and_no_longer() {
        let (model, registered) = asked(&env(&[
            ("MEM_LLM_PROVIDER", "openai"),
            ("OPENAI_API_KEY", "not-a-real-key"),
        ]))
        .expect("a provider with a key resolves");
        assert_eq!(model, "openai/gpt-4o-mini");

        let name = registered
            .as_ref()
            .expect("openai is registered")
            .name()
            .to_string();
        assert!(ailoy::lang_model::get_lm_providers().contains_key(&name));
        assert!(ailoy::agent::get_agent_providers().contains_key(&name));

        drop(registered);
        assert!(
            !ailoy::lang_model::get_lm_providers().contains_key(&name),
            "a registry that keeps every call's provider grows for the life of the process"
        );
        assert!(!ailoy::agent::get_agent_providers().contains_key(&name));
    }

    /// A provider this crate has no key variable for is left to ailoy, which is the component
    /// that knows about it — nothing is registered and nothing is refused here.
    #[test]
    fn an_unknown_provider_is_left_to_ailoy() {
        let (model, registered) = asked(&env(&[("MEM_LLM_MODEL", "some-gateway/some-model")]))
            .expect("not this crate's call to refuse");
        assert_eq!(model, "some-gateway/some-model");
        assert!(registered.is_none());
    }

    /// No model is asked anything about a conversation with nothing in it — and in particular
    /// no key is needed to find that out.
    #[tokio::test]
    async fn a_conversation_with_no_turns_needs_no_model() {
        let conversation = [msg(Role::System, "You are a helpful assistant.")];
        assert!(
            extract_memories(&conversation, &[], &BTreeMap::new())
                .await
                .unwrap()
                .is_empty()
        );
    }
}
