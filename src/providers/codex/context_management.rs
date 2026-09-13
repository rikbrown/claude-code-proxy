//! Replays server-side context compaction blobs on later turns.
//!
//! When `context_management` is enabled, Codex may compact the rendered window
//! mid-stream and emit an opaque `compaction` output item. A later request can
//! send that item in place of the history it represents. Unlike server
//! compaction, no extra request is made: the blob arrives on an ordinary turn.
//!
//! Safety comes from an explicit boundary, not from turn bookkeeping, and the
//! boundary has two parts because a blob's real coverage cannot be read off
//! the stream. Live probes show a blob contains the request input plus the
//! output items emitted before it, but a blob emitted at output_index 0 has
//! also been observed to contain the message emitted after it. Position is
//! therefore a lower bound on coverage, not an upper bound.
//!
//! - The *checked* prefix is the request's conversation items followed by
//!   every output item of the producing turn that the client echoes back. A
//!   blob is only ever replayed for a request whose conversation starts with
//!   exactly that prefix under the same model and prompt shape, so a reply
//!   that differs from the one inside the blob always fails the check.
//! - The *stripped* count is the part of that prefix the blob is known to
//!   hold: the request input plus the output items positionally before the
//!   blob. Only that much is replaced on the wire; the rest of the checked
//!   prefix stays explicit. A reply the blob happens to hold may therefore be
//!   sent twice, which is wasteful but never wrong.
//!
//! Anything else discards the state and sends the full history. A turn
//! generation guard additionally stops a late-completing older turn from
//! installing state over a newer one.
//!
//! The feature is unavailable on the Responses Lite lane, which rejects
//! server-side compaction; requests translated for that lane never carry
//! `context_management`, and capture and replay both key off that field.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::request_identity::ConversationIdentity;

use super::compaction::split_input_envelope;
use super::continuation::{canonical_input_item, prompt_signature, stable_json};
use super::translate::reducer::CompactionOutput;
use super::translate::request::{ResponsesInputItem, ResponsesRequest};

const STATE_TTL_MS: u64 = 30 * 60 * 1_000;
const MAX_STATES: usize = 1_000;
const MAX_TURNS: usize = 10_000;
const MAX_BLOB_BYTES: usize = 1024 * 1024;
const MAX_COVERED_ITEMS: usize = 100_000;
const MAX_TOTAL_STATE_BYTES: usize = 20_000_000;

type ItemDigest = [u8; 32];

struct ContextState {
    model: String,
    prompt_signature: String,
    envelope: Vec<ItemDigest>,
    /// Digests of the client-visible items a replaying request must begin
    /// with, in order: the producing request's conversation items followed by
    /// every echoable output item of that turn. Chained replays keep extending
    /// this list so the client's full history always matches even though the
    /// wire never carries it.
    checked: Vec<ItemDigest>,
    /// How many leading `checked` items the blob replaces on the wire: the
    /// request input plus the output items emitted before the blob. Never
    /// more than what position proves the blob holds.
    stripped: usize,
    encrypted_content: String,
    updated_at: u64,
}

struct TurnEntry {
    id: u64,
    updated_at: u64,
}

#[derive(Default)]
struct ContextRegistry {
    states: HashMap<ConversationIdentity, ContextState>,
    /// The newest turn begun per owner. Only that turn may install state.
    turns: HashMap<ConversationIdentity, TurnEntry>,
    total_bytes: usize,
}

static REGISTRY: Mutex<Option<ContextRegistry>> = Mutex::new(None);
static NEXT_TURN_ID: AtomicU64 = AtomicU64::new(1);

/// A generation token for one request under one owner. Capture requires the
/// token to still be the owner's newest, so an older turn that completes after
/// a newer one began cannot install state the newer history then matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextTurn(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardReason {
    ModelChanged,
    PromptChanged,
    EnvelopeChanged,
    NotAppendOnly,
    ForeignCompaction,
    ReplayMismatch,
    TooLarge,
}

impl DiscardReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelChanged => "model_changed",
            Self::PromptChanged => "prompt_changed",
            Self::EnvelopeChanged => "envelope_changed",
            Self::NotAppendOnly => "not_append_only",
            Self::ForeignCompaction => "foreign_compaction",
            Self::ReplayMismatch => "replay_mismatch",
            Self::TooLarge => "too_large",
        }
    }
}

pub struct ContextReplay {
    pub request: ResponsesRequest,
    pub covered_items: usize,
    pub tail_items: usize,
}

pub enum ReplayOutcome {
    Replayed(Box<ContextReplay>),
    Unchanged,
    Discarded(DiscardReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOutcome {
    Captured {
        /// Items a replaying request must begin with.
        checked_items: usize,
        /// Leading checked items the blob replaces on the wire.
        stripped_items: usize,
        blob_bytes: usize,
        chained: bool,
    },
    /// A replayed turn finished without a new blob; the old state stands.
    Retained,
    /// Nothing to record: no owner, no turn, or no blob.
    Skipped,
    /// A newer turn began for this owner before this one completed.
    Superseded,
    /// The blob's coverage could not be established, so nothing was stored.
    /// Any existing state is left untouched.
    Refused(&'static str),
    Discarded(DiscardReason),
}

/// Marks the start of a request for `owner`. The returned token must be
/// passed to [`capture_context_blob`] when the turn completes.
pub fn begin_context_turn(owner: Option<&ConversationIdentity>) -> Option<ContextTurn> {
    let owner = owner?;
    let id = NEXT_TURN_ID.fetch_add(1, Ordering::Relaxed);
    let now = now_ms();
    let mut guard = REGISTRY.lock().unwrap();
    let registry = guard.get_or_insert_with(ContextRegistry::default);
    evict_states(registry, now);
    registry.turns.insert(
        owner.clone(),
        TurnEntry {
            id,
            updated_at: now,
        },
    );
    Some(ContextTurn(id))
}

/// Substitutes the stored blob for the covered prefix of `request.input` when
/// the request is a verified append-only extension of what the blob covers.
pub fn apply_context_replay(
    owner: Option<&ConversationIdentity>,
    request: &ResponsesRequest,
) -> ReplayOutcome {
    let Some(owner) = owner else {
        return ReplayOutcome::Unchanged;
    };
    let now = now_ms();
    let mut guard = REGISTRY.lock().unwrap();
    let Some(registry) = guard.as_mut() else {
        return ReplayOutcome::Unchanged;
    };
    evict_states(registry, now);
    let Some(state) = registry.states.get_mut(owner) else {
        return ReplayOutcome::Unchanged;
    };

    let (envelope, conversation) = split_input_envelope(&request.input);
    if let Err(reason) = check_shape(state, request, envelope, conversation) {
        remove_state(registry, owner);
        return ReplayOutcome::Discarded(reason);
    }
    // The conversation must begin with the whole checked prefix: the
    // producing request's items and every echoable output item of that turn.
    // A reply that differs from the one inside the blob fails here and is
    // never replayed. Only the stripped part is then replaced on the wire.
    if !starts_with_digests(conversation, &state.checked) {
        remove_state(registry, owner);
        return ReplayOutcome::Discarded(DiscardReason::NotAppendOnly);
    }
    if conversation.len() == state.checked.len() {
        return ReplayOutcome::Unchanged;
    }
    let covered_items = state.stripped;
    let tail = &conversation[covered_items..];

    let mut replay = request.clone();
    replay.input = envelope
        .iter()
        .cloned()
        .chain(std::iter::once(ResponsesInputItem::Compaction {
            encrypted_content: state.encrypted_content.clone(),
        }))
        .chain(tail.iter().cloned())
        .collect();
    state.updated_at = now;
    ReplayOutcome::Replayed(Box::new(ContextReplay {
        request: replay,
        covered_items,
        tail_items: tail.len(),
    }))
}

/// Records the blob a completed turn produced. `request` is the input actually
/// sent upstream and `output_items` the turn's captured output in order;
/// `compaction` says how many leading output items the blob was emitted
/// after. The checked prefix becomes the request's conversation items
/// followed by all of `output_items`; the stripped count covers the
/// conversation items plus only the positionally covered output. When the
/// request was itself a replay, the new blob covers the old blob's history
/// plus the tail, so the stored digests are extended rather than replaced.
///
/// Every output item must be of a kind whose client round-trip form is
/// reproduced exactly by request translation (assistant messages, function
/// calls, and reasoning items); otherwise nothing is stored.
pub fn capture_context_blob(
    owner: Option<&ConversationIdentity>,
    turn: Option<ContextTurn>,
    request: &ResponsesRequest,
    output_items: &[ResponsesInputItem],
    compaction: Option<&CompactionOutput>,
) -> CaptureOutcome {
    let (Some(owner), Some(turn)) = (owner, turn) else {
        return CaptureOutcome::Skipped;
    };
    let (envelope, conversation) = split_input_envelope(&request.input);
    let (replayed_blob, tail) = match conversation.split_first() {
        Some((ResponsesInputItem::Compaction { encrypted_content }, tail)) => {
            (Some(encrypted_content.as_str()), tail)
        }
        _ => (None, conversation),
    };
    if tail
        .iter()
        .any(|item| matches!(item, ResponsesInputItem::Compaction { .. }))
    {
        return CaptureOutcome::Skipped;
    }

    let now = now_ms();
    let mut guard = REGISTRY.lock().unwrap();
    let registry = guard.get_or_insert_with(ContextRegistry::default);
    evict_states(registry, now);
    if registry.turns.get(owner).map(|entry| entry.id) != Some(turn.0) {
        return CaptureOutcome::Superseded;
    }

    let envelope_digests: Vec<ItemDigest> = envelope.iter().map(item_digest).collect();
    let mut checked = Vec::new();
    let mut stripped = 0;
    if let Some(replayed_blob) = replayed_blob {
        let Some(state) = registry.states.get_mut(owner) else {
            return CaptureOutcome::Discarded(DiscardReason::ReplayMismatch);
        };
        // The replayed request was `blob + checked[stripped..] + new items`;
        // the explicit remainder of the old checked prefix must still lead
        // the tail, or this completion belongs to some other history.
        if state.encrypted_content != replayed_blob
            || check_shape(state, request, envelope, tail).is_err()
            || !starts_with_digests(tail, &state.checked[state.stripped..])
        {
            remove_state(registry, owner);
            return CaptureOutcome::Discarded(DiscardReason::ReplayMismatch);
        }
        if compaction.is_none() {
            state.updated_at = now;
            return CaptureOutcome::Retained;
        }
        checked.extend_from_slice(&state.checked[..state.stripped]);
        stripped = state.stripped;
    }
    let Some(compaction) = compaction else {
        return CaptureOutcome::Skipped;
    };
    if compaction.covered_output_items > output_items.len() {
        return CaptureOutcome::Refused("covered_output_beyond_items");
    }
    if !output_items.iter().all(round_trips_exactly) {
        return CaptureOutcome::Refused("unsupported_output_item");
    }
    // The new blob holds everything the request carried (the old blob's
    // content and the explicit tail) plus the output emitted before it.
    checked.extend(tail.iter().map(item_digest));
    checked.extend(output_items.iter().map(item_digest));
    stripped += tail.len() + compaction.covered_output_items;

    if compaction.encrypted_content.len() > MAX_BLOB_BYTES || checked.len() > MAX_COVERED_ITEMS {
        remove_state(registry, owner);
        return CaptureOutcome::Discarded(DiscardReason::TooLarge);
    }
    let state = ContextState {
        model: request.model.clone(),
        prompt_signature: prompt_signature(request),
        envelope: envelope_digests,
        checked,
        stripped,
        encrypted_content: compaction.encrypted_content.clone(),
        updated_at: now,
    };
    let outcome = CaptureOutcome::Captured {
        checked_items: state.checked.len(),
        stripped_items: state.stripped,
        blob_bytes: compaction.encrypted_content.len(),
        chained: replayed_blob.is_some(),
    };
    registry.states.insert(owner.clone(), state);
    evict_states(registry, now);
    if registry.states.contains_key(owner) {
        outcome
    } else {
        CaptureOutcome::Discarded(DiscardReason::TooLarge)
    }
}

/// Whether request translation reproduces this reducer output item exactly
/// when Claude Code echoes it back on the next turn, so its digest can be
/// part of the coverage boundary.
fn round_trips_exactly(item: &ResponsesInputItem) -> bool {
    match item {
        ResponsesInputItem::Message { role, .. } => role == "assistant",
        ResponsesInputItem::FunctionCall { .. } | ResponsesInputItem::Reasoning { .. } => true,
        ResponsesInputItem::AdditionalTools { .. }
        | ResponsesInputItem::FunctionCallOutput { .. }
        | ResponsesInputItem::Compaction { .. }
        | ResponsesInputItem::CompactionTrigger => false,
    }
}

/// Drops every owner belonging to a Claude Code session, including its agents.
pub fn clear_session_context(session_id: &str) {
    let mut guard = REGISTRY.lock().unwrap();
    if let Some(registry) = guard.as_mut() {
        let belongs_to_session = |owner: &ConversationIdentity| match owner {
            ConversationIdentity::Main(session) | ConversationIdentity::Agent(session, _) => {
                session == session_id
            }
        };
        registry
            .states
            .retain(|owner, _| !belongs_to_session(owner));
        registry.turns.retain(|owner, _| !belongs_to_session(owner));
        update_total_bytes(registry);
    }
}

pub fn clear_all_context_management_for_tests() {
    *REGISTRY.lock().unwrap() = None;
}

fn check_shape(
    state: &ContextState,
    request: &ResponsesRequest,
    envelope: &[ResponsesInputItem],
    conversation: &[ResponsesInputItem],
) -> Result<(), DiscardReason> {
    if state.model != request.model {
        return Err(DiscardReason::ModelChanged);
    }
    if state.prompt_signature != prompt_signature(request) {
        return Err(DiscardReason::PromptChanged);
    }
    if conversation
        .iter()
        .any(|item| matches!(item, ResponsesInputItem::Compaction { .. }))
    {
        return Err(DiscardReason::ForeignCompaction);
    }
    if envelope.len() != state.envelope.len()
        || envelope
            .iter()
            .zip(&state.envelope)
            .any(|(item, digest)| item_digest(item) != *digest)
    {
        return Err(DiscardReason::EnvelopeChanged);
    }
    Ok(())
}

fn item_digest(item: &ResponsesInputItem) -> ItemDigest {
    Sha256::digest(stable_json(&canonical_input_item(item))).into()
}

fn starts_with_digests(items: &[ResponsesInputItem], digests: &[ItemDigest]) -> bool {
    items.len() >= digests.len()
        && items
            .iter()
            .zip(digests)
            .all(|(item, digest)| item_digest(item) == *digest)
}

fn state_size(owner: &ConversationIdentity, state: &ContextState) -> usize {
    let owner_len = match owner {
        ConversationIdentity::Main(session) => session.len(),
        ConversationIdentity::Agent(session, agent) => session.len() + agent.len(),
    };
    owner_len
        + state.model.len()
        + state.prompt_signature.len()
        + (state.envelope.len() + state.checked.len()) * std::mem::size_of::<ItemDigest>()
        + state.encrypted_content.len()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn remove_state(registry: &mut ContextRegistry, owner: &ConversationIdentity) {
    registry.states.remove(owner);
    update_total_bytes(registry);
}

fn update_total_bytes(registry: &mut ContextRegistry) {
    registry.total_bytes = registry
        .states
        .iter()
        .map(|(owner, state)| state_size(owner, state))
        .sum();
}

fn evict_states(registry: &mut ContextRegistry, now: u64) {
    registry
        .states
        .retain(|_, state| now.saturating_sub(state.updated_at) <= STATE_TTL_MS);
    registry
        .turns
        .retain(|_, turn| now.saturating_sub(turn.updated_at) <= STATE_TTL_MS);
    while registry.turns.len() > MAX_TURNS {
        let oldest = registry
            .turns
            .iter()
            .min_by_key(|(_, turn)| turn.updated_at)
            .map(|(owner, _)| owner.clone());
        let Some(oldest) = oldest else {
            break;
        };
        registry.turns.remove(&oldest);
    }
    update_total_bytes(registry);
    while registry.states.len() > MAX_STATES || registry.total_bytes > MAX_TOTAL_STATE_BYTES {
        let oldest = registry
            .states
            .iter()
            .min_by_key(|(_, state)| state.updated_at)
            .map(|(owner, _)| owner.clone());
        let Some(oldest) = oldest else {
            break;
        };
        registry.states.remove(&oldest);
        update_total_bytes(registry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    static TEST_REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    fn owner() -> ConversationIdentity {
        ConversationIdentity::Main("session".to_string())
    }

    fn request(input: serde_json::Value) -> ResponsesRequest {
        serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "instructions": "system prompt",
            "input": input,
            "store": false,
            "stream": true,
            "parallel_tool_calls": false,
            "text": {"verbosity":"low"},
            "context_management": [{"type":"compaction","compact_threshold":200000}]
        }))
        .unwrap()
    }

    fn user(text: &str) -> serde_json::Value {
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]})
    }

    fn assistant(text: &str) -> serde_json::Value {
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})
    }

    fn compaction(blob: &str) -> serde_json::Value {
        json!({"type":"compaction","encrypted_content":blob})
    }

    fn items(values: &[serde_json::Value]) -> Vec<ResponsesInputItem> {
        values
            .iter()
            .map(|value| serde_json::from_value(value.clone()).unwrap())
            .collect()
    }

    fn blob(encrypted_content: &str, covered_output_items: usize) -> CompactionOutput {
        CompactionOutput {
            encrypted_content: encrypted_content.to_string(),
            covered_output_items,
        }
    }

    fn turn() -> Option<ContextTurn> {
        begin_context_turn(Some(&owner()))
    }

    /// Captures a blob emitted before any output item, so the boundary is the
    /// request input alone.
    fn captured(input: serde_json::Value, encrypted_content: &str) -> CaptureOutcome {
        let turn = turn();
        capture_context_blob(
            Some(&owner()),
            turn,
            &request(input),
            &[],
            Some(&blob(encrypted_content, 0)),
        )
    }

    /// Captures a blob emitted after `covered` of the given output items.
    fn captured_with_output(
        input: serde_json::Value,
        output: &[serde_json::Value],
        covered: usize,
        encrypted_content: &str,
    ) -> CaptureOutcome {
        let turn = turn();
        capture_context_blob(
            Some(&owner()),
            turn,
            &request(input),
            &items(output),
            Some(&blob(encrypted_content, covered)),
        )
    }

    fn replay_input(outcome: ReplayOutcome) -> Vec<serde_json::Value> {
        let ReplayOutcome::Replayed(replay) = outcome else {
            panic!("expected a replay");
        };
        serde_json::to_value(replay.request.input)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    fn set_updated_at(age_ms: u64) {
        let mut guard = REGISTRY.lock().unwrap();
        let state = guard.as_mut().unwrap().states.get_mut(&owner()).unwrap();
        state.updated_at = now_ms() - age_ms;
    }

    #[test]
    fn replays_blob_for_append_only_extension() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        assert_eq!(
            captured(json!([user("one"), assistant("two")]), "blob-1"),
            CaptureOutcome::Captured {
                checked_items: 2,
                stripped_items: 2,
                blob_bytes: 6,
                chained: false
            }
        );

        let next = request(json!([user("one"), assistant("two"), user("three")]));
        let input = replay_input(apply_context_replay(Some(&owner()), &next));
        assert_eq!(input, vec![compaction("blob-1"), user("three")]);
    }

    #[test]
    fn replay_keeps_lite_envelope_outside_the_blob() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        let envelope = json!({"type":"additional_tools","role":"developer","tools":[]});
        let developer = json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"instructions"}]});
        captured(
            json!([envelope, developer, user("one"), assistant("two")]),
            "blob-1",
        );

        let next = request(json!([
            envelope,
            developer,
            user("one"),
            assistant("two"),
            user("three")
        ]));
        let ReplayOutcome::Replayed(replay) = apply_context_replay(Some(&owner()), &next) else {
            panic!("expected a replay");
        };
        assert_eq!(replay.covered_items, 2);
        assert_eq!(replay.tail_items, 1);
        let input = serde_json::to_value(replay.request.input).unwrap();
        assert_eq!(
            input,
            json!([envelope, developer, compaction("blob-1"), user("three")])
        );
    }

    #[test]
    fn chained_capture_extends_covered_history() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        captured(json!([user("one"), assistant("two")]), "blob-1");
        let next = request(json!([user("one"), assistant("two"), user("three")]));
        let ReplayOutcome::Replayed(replay) = apply_context_replay(Some(&owner()), &next) else {
            panic!("expected a replay");
        };

        let turn = turn();
        assert_eq!(
            capture_context_blob(
                Some(&owner()),
                turn,
                &replay.request,
                &items(&[assistant("four")]),
                Some(&blob("blob-2", 1)),
            ),
            CaptureOutcome::Captured {
                checked_items: 4,
                stripped_items: 4,
                blob_bytes: 6,
                chained: true
            }
        );
        let later = request(json!([
            user("one"),
            assistant("two"),
            user("three"),
            assistant("four"),
            user("five")
        ]));
        let input = replay_input(apply_context_replay(Some(&owner()), &later));
        assert_eq!(input, vec![compaction("blob-2"), user("five")]);
    }

    #[test]
    fn chained_capture_keeps_unstripped_output_explicit_and_checked() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        captured(json!([user("one")]), "blob-1");
        let next = request(json!([user("one"), user("two")]));
        let ReplayOutcome::Replayed(replay) = apply_context_replay(Some(&owner()), &next) else {
            panic!("expected a replay");
        };
        // blob-2 was emitted before the reply: the reply is checked, not
        // stripped.
        let turn = turn();
        assert_eq!(
            capture_context_blob(
                Some(&owner()),
                turn,
                &replay.request,
                &items(&[assistant("three")]),
                Some(&blob("blob-2", 0)),
            ),
            CaptureOutcome::Captured {
                checked_items: 3,
                stripped_items: 2,
                blob_bytes: 6,
                chained: true
            }
        );
        let later = request(json!([
            user("one"),
            user("two"),
            assistant("three"),
            user("four")
        ]));
        let input = replay_input(apply_context_replay(Some(&owner()), &later));
        assert_eq!(
            input,
            vec![compaction("blob-2"), assistant("three"), user("four")]
        );

        // A third capture chained on that replay extends the stripped part
        // over the explicit reply, and the prefix stays consistent.
        let ReplayOutcome::Replayed(replay) = apply_context_replay(Some(&owner()), &later) else {
            panic!("expected a replay");
        };
        let third_turn = begin_context_turn(Some(&owner()));
        assert_eq!(
            capture_context_blob(
                Some(&owner()),
                third_turn,
                &replay.request,
                &items(&[assistant("five")]),
                Some(&blob("blob-3", 1)),
            ),
            CaptureOutcome::Captured {
                checked_items: 5,
                stripped_items: 5,
                blob_bytes: 6,
                chained: true
            }
        );
        let edited = request(json!([
            user("one"),
            user("two"),
            assistant("edited"),
            user("four"),
            assistant("five"),
            user("six")
        ]));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &edited),
            ReplayOutcome::Discarded(DiscardReason::NotAppendOnly)
        ));
    }

    #[test]
    fn blob_after_output_covers_that_output() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        assert_eq!(
            captured_with_output(json!([user("one")]), &[assistant("two")], 1, "blob-1"),
            CaptureOutcome::Captured {
                checked_items: 2,
                stripped_items: 2,
                blob_bytes: 6,
                chained: false
            }
        );
        let next = request(json!([user("one"), assistant("two"), user("three")]));
        let input = replay_input(apply_context_replay(Some(&owner()), &next));
        assert_eq!(input, vec![compaction("blob-1"), user("three")]);
    }

    #[test]
    fn differing_reply_never_replays_a_blob_that_holds_another_reply() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        // Whether the blob was emitted after the reply (covered 1) or before
        // it (covered 0), the reply is part of the checked prefix: a blob at
        // index 0 has been observed to hold the message emitted after it.
        for covered in [1, 0] {
            captured_with_output(json!([user("one")]), &[assistant("two")], covered, "blob-1");
            // The client kept a different reply (edit, regeneration, or a
            // competing turn); the blob may hold "two", so it must not be sent.
            let next = request(json!([user("one"), assistant("other"), user("three")]));
            assert!(matches!(
                apply_context_replay(Some(&owner()), &next),
                ReplayOutcome::Discarded(DiscardReason::NotAppendOnly)
            ));
            assert!(matches!(
                apply_context_replay(Some(&owner()), &next),
                ReplayOutcome::Unchanged
            ));
        }
    }

    #[test]
    fn blob_before_output_leaves_output_explicit() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        let call = json!({"type":"function_call","call_id":"call-1","name":"Read","arguments":"{\"path\":\"a\"}"});
        assert_eq!(
            captured_with_output(
                json!([user("one")]),
                &[call.clone(), assistant("two")],
                1,
                "blob-1"
            ),
            CaptureOutcome::Captured {
                checked_items: 3,
                stripped_items: 2,
                blob_bytes: 6,
                chained: false
            }
        );
        let next = request(json!([
            user("one"),
            call.clone(),
            assistant("two"),
            user("three")
        ]));
        let input = replay_input(apply_context_replay(Some(&owner()), &next));
        assert_eq!(
            input,
            vec![compaction("blob-1"), assistant("two"), user("three")]
        );

        // The explicit reply is still checked: a different one discards.
        let edited = request(json!([
            user("one"),
            call,
            assistant("edited"),
            user("three")
        ]));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &edited),
            ReplayOutcome::Discarded(DiscardReason::NotAppendOnly)
        ));
    }

    #[test]
    fn late_older_turn_cannot_install_state_over_a_newer_turn() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        let history = request(json!([user("one")]));
        let older = turn();
        let newer = turn();
        assert_eq!(
            capture_context_blob(
                Some(&owner()),
                newer,
                &history,
                &items(&[assistant("new")]),
                Some(&blob("blob-new", 1)),
            ),
            CaptureOutcome::Captured {
                checked_items: 2,
                stripped_items: 2,
                blob_bytes: 8,
                chained: false
            }
        );
        assert_eq!(
            capture_context_blob(
                Some(&owner()),
                older,
                &history,
                &items(&[assistant("old")]),
                Some(&blob("blob-old", 1)),
            ),
            CaptureOutcome::Superseded
        );

        let next = request(json!([user("one"), assistant("new"), user("two")]));
        let input = replay_input(apply_context_replay(Some(&owner()), &next));
        assert_eq!(input, vec![compaction("blob-new"), user("two")]);
    }

    #[test]
    fn unpositionable_or_unsupported_coverage_stores_nothing() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        let output = json!({"type":"function_call_output","call_id":"call-1","output":"ok"});
        assert_eq!(
            captured_with_output(
                json!([user("one")]),
                std::slice::from_ref(&output),
                1,
                "blob-1"
            ),
            CaptureOutcome::Refused("unsupported_output_item")
        );
        // Every output item is part of the checked prefix, so an unsupported
        // one refuses even when it sits after the blob.
        assert_eq!(
            captured_with_output(json!([user("one")]), &[output], 0, "blob-1"),
            CaptureOutcome::Refused("unsupported_output_item")
        );
        assert_eq!(
            captured_with_output(json!([user("one")]), &[assistant("two")], 2, "blob-1"),
            CaptureOutcome::Refused("covered_output_beyond_items")
        );
        let next = request(json!([user("one"), assistant("two"), user("three")]));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &next),
            ReplayOutcome::Unchanged
        ));
    }

    #[test]
    fn replayed_turn_without_new_blob_keeps_state() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        captured(json!([user("one")]), "blob-1");
        let next = request(json!([user("one"), user("two")]));
        let ReplayOutcome::Replayed(replay) = apply_context_replay(Some(&owner()), &next) else {
            panic!("expected a replay");
        };
        let turn = turn();
        assert_eq!(
            capture_context_blob(Some(&owner()), turn, &replay.request, &[], None),
            CaptureOutcome::Retained
        );

        let later = request(json!([user("one"), user("two"), user("three")]));
        let input = replay_input(apply_context_replay(Some(&owner()), &later));
        assert_eq!(
            input,
            vec![compaction("blob-1"), user("two"), user("three")]
        );
    }

    #[test]
    fn identical_request_is_sent_unchanged_without_losing_state() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        captured(json!([user("one")]), "blob-1");
        let same = request(json!([user("one")]));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &same),
            ReplayOutcome::Unchanged
        ));
        let next = request(json!([user("one"), user("two")]));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &next),
            ReplayOutcome::Replayed(_)
        ));
    }

    #[test]
    fn missing_owner_or_blob_is_stateless() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        let body = request(json!([user("one")]));
        assert_eq!(
            capture_context_blob(None, None, &body, &[], Some(&blob("blob", 0))),
            CaptureOutcome::Skipped
        );
        assert_eq!(
            capture_context_blob(Some(&owner()), None, &body, &[], Some(&blob("blob", 0))),
            CaptureOutcome::Skipped
        );
        let turn = turn();
        assert_eq!(
            capture_context_blob(Some(&owner()), turn, &body, &[], None),
            CaptureOutcome::Skipped
        );
        assert!(matches!(
            apply_context_replay(None, &body),
            ReplayOutcome::Unchanged
        ));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &body),
            ReplayOutcome::Unchanged
        ));
    }

    #[test]
    fn branch_discards_state_and_sends_full_history() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        for branched in [
            json!([user("different"), assistant("two"), user("three")]),
            json!([user("one")]),
            json!([]),
        ] {
            captured(json!([user("one"), assistant("two")]), "blob-1");
            let next = request(branched);
            assert!(matches!(
                apply_context_replay(Some(&owner()), &next),
                ReplayOutcome::Discarded(DiscardReason::NotAppendOnly)
            ));
            assert!(matches!(
                apply_context_replay(Some(&owner()), &next),
                ReplayOutcome::Unchanged
            ));
        }
    }

    #[test]
    fn model_prompt_and_envelope_changes_discard_state() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        let envelope = json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"instructions"}]});
        let history = json!([envelope, user("one")]);

        captured(history.clone(), "blob-1");
        let mut changed_model = request(json!([envelope, user("one"), user("two")]));
        changed_model.model = "gpt-5.6-terra".to_string();
        assert!(matches!(
            apply_context_replay(Some(&owner()), &changed_model),
            ReplayOutcome::Discarded(DiscardReason::ModelChanged)
        ));

        captured(history.clone(), "blob-1");
        let mut changed_prompt = request(json!([envelope, user("one"), user("two")]));
        changed_prompt.instructions = Some("other system prompt".to_string());
        assert!(matches!(
            apply_context_replay(Some(&owner()), &changed_prompt),
            ReplayOutcome::Discarded(DiscardReason::PromptChanged)
        ));

        captured(history, "blob-1");
        let other_envelope = json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"other"}]});
        let changed_envelope = request(json!([other_envelope, user("one"), user("two")]));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &changed_envelope),
            ReplayOutcome::Discarded(DiscardReason::EnvelopeChanged)
        ));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &changed_envelope),
            ReplayOutcome::Unchanged
        ));
    }

    #[test]
    fn expired_state_is_discarded() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        captured(json!([user("one")]), "blob-1");
        set_updated_at(STATE_TTL_MS + 1);
        let next = request(json!([user("one"), user("two")]));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &next),
            ReplayOutcome::Unchanged
        ));
        assert!(REGISTRY.lock().unwrap().as_ref().unwrap().states.is_empty());
    }

    #[test]
    fn foreign_compaction_items_are_never_replayed_or_captured() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        captured(json!([user("one")]), "blob-1");
        let server_compaction = request(json!([compaction("other-feature"), user("two")]));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &server_compaction),
            ReplayOutcome::Discarded(DiscardReason::ForeignCompaction)
        ));

        let trailing = request(json!([
            user("one"),
            compaction("other-feature"),
            user("two")
        ]));
        let turn = turn();
        assert_eq!(
            capture_context_blob(
                Some(&owner()),
                turn,
                &trailing,
                &[],
                Some(&blob("blob-2", 0))
            ),
            CaptureOutcome::Skipped
        );
        assert!(matches!(
            capture_context_blob(
                Some(&owner()),
                turn,
                &server_compaction,
                &[],
                Some(&blob("blob-2", 0))
            ),
            CaptureOutcome::Discarded(DiscardReason::ReplayMismatch)
        ));
    }

    #[test]
    fn stale_replay_completion_cannot_chain_onto_newer_state() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        captured(json!([user("one")]), "blob-old");
        let older_turn = turn();
        let older = request(json!([user("one"), user("two")]));
        let ReplayOutcome::Replayed(older_replay) = apply_context_replay(Some(&owner()), &older)
        else {
            panic!("expected a replay");
        };
        captured(json!([user("one"), user("two")]), "blob-new");

        // The newer turn's state stands; the stale completion is ignored.
        assert_eq!(
            capture_context_blob(
                Some(&owner()),
                older_turn,
                &older_replay.request,
                &[],
                Some(&blob("blob-late", 0))
            ),
            CaptureOutcome::Superseded
        );
        let next = request(json!([user("one"), user("two"), user("three")]));
        let input = replay_input(apply_context_replay(Some(&owner()), &next));
        assert_eq!(input, vec![compaction("blob-new"), user("three")]);
    }

    #[test]
    fn replayed_blob_mismatch_discards_state() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        captured(json!([user("one")]), "blob-1");
        let turn = turn();
        let foreign = request(json!([compaction("blob-other"), user("two")]));
        assert!(matches!(
            capture_context_blob(
                Some(&owner()),
                turn,
                &foreign,
                &[],
                Some(&blob("blob-2", 0))
            ),
            CaptureOutcome::Discarded(DiscardReason::ReplayMismatch)
        ));
        let next = request(json!([user("one"), user("two")]));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &next),
            ReplayOutcome::Unchanged
        ));
    }

    #[test]
    fn function_call_arguments_compare_as_json() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        let streamed = json!({"type":"function_call","call_id":"call-1","name":"Read","arguments":"{\"path\": \"a\", \"limit\": 1}"});
        let echoed = json!({"type":"function_call","call_id":"call-1","name":"Read","arguments":"{\"limit\":1,\"path\":\"a\"}"});
        let output = json!({"type":"function_call_output","call_id":"call-1","output":"ok"});
        captured(json!([user("one"), streamed]), "blob-1");

        let next = request(json!([user("one"), echoed, output]));
        let input = replay_input(apply_context_replay(Some(&owner()), &next));
        assert_eq!(input, vec![compaction("blob-1"), output]);
    }

    #[test]
    fn oversized_blob_is_discarded() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        let blob = "x".repeat(MAX_BLOB_BYTES + 1);
        assert!(matches!(
            captured(json!([user("one")]), &blob),
            CaptureOutcome::Discarded(DiscardReason::TooLarge)
        ));
        let next = request(json!([user("one"), user("two")]));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &next),
            ReplayOutcome::Unchanged
        ));
    }

    #[test]
    fn clearing_a_session_drops_its_agents_but_not_other_sessions() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        let agent = ConversationIdentity::Agent("session".to_string(), "agent".to_string());
        let other = ConversationIdentity::Main("other".to_string());
        let body = request(json!([user("one")]));
        for owner in [&owner(), &agent, &other] {
            let turn = begin_context_turn(Some(owner));
            capture_context_blob(Some(owner), turn, &body, &[], Some(&blob("blob", 0)));
        }

        clear_session_context("session");

        let next = request(json!([user("one"), user("two")]));
        assert!(matches!(
            apply_context_replay(Some(&owner()), &next),
            ReplayOutcome::Unchanged
        ));
        assert!(matches!(
            apply_context_replay(Some(&agent), &next),
            ReplayOutcome::Unchanged
        ));
        assert!(matches!(
            apply_context_replay(Some(&other), &next),
            ReplayOutcome::Replayed(_)
        ));
    }
}
