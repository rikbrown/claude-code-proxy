//! Replays server-side context compaction blobs on later turns.
//!
//! When `context_management` is enabled, Codex may compact the rendered window
//! mid-stream and emit an opaque `compaction` output item. A later request can
//! send that item in place of the history it represents. Unlike server
//! compaction, no extra request is made: the blob arrives on an ordinary turn.
//!
//! Safety comes from the prefix check, not from turn bookkeeping. The stored
//! state records a digest of every conversation item the blob covers, and a
//! blob is only ever substituted for a request whose conversation starts with
//! exactly those items under the same model and prompt shape. Anything else
//! discards the state and sends the full history.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::request_identity::ConversationIdentity;

use super::compaction::split_input_envelope;
use super::continuation::{canonical_input_item, prompt_signature, stable_json};
use super::translate::request::{ResponsesInputItem, ResponsesRequest};

const STATE_TTL_MS: u64 = 30 * 60 * 1_000;
const MAX_STATES: usize = 1_000;
const MAX_BLOB_BYTES: usize = 1024 * 1024;
const MAX_COVERED_ITEMS: usize = 100_000;
const MAX_TOTAL_STATE_BYTES: usize = 20_000_000;

type ItemDigest = [u8; 32];

struct ContextState {
    model: String,
    prompt_signature: String,
    envelope: Vec<ItemDigest>,
    /// Digests of the client-visible conversation items the blob stands for,
    /// in order. Chained replays keep extending this list so the client's
    /// full history always matches even though the wire never carries it.
    covered: Vec<ItemDigest>,
    encrypted_content: String,
    updated_at: u64,
}

#[derive(Default)]
struct ContextRegistry {
    states: HashMap<ConversationIdentity, ContextState>,
    total_bytes: usize,
}

static REGISTRY: Mutex<Option<ContextRegistry>> = Mutex::new(None);

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
        covered_items: usize,
        blob_bytes: usize,
        chained: bool,
    },
    Retained,
    Skipped,
    Discarded(DiscardReason),
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
    let covered_items = state.covered.len();
    if conversation.len() < covered_items
        || conversation[..covered_items]
            .iter()
            .zip(&state.covered)
            .any(|(item, digest)| item_digest(item) != *digest)
    {
        remove_state(registry, owner);
        return ReplayOutcome::Discarded(DiscardReason::NotAppendOnly);
    }
    let tail = &conversation[covered_items..];
    if tail.is_empty() {
        return ReplayOutcome::Unchanged;
    }

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

/// Records the blob a completed turn produced, keyed by the conversation items
/// the request carried. `request` is the input actually sent upstream; when it
/// was itself a replay, the new blob covers the old blob's history plus the
/// tail, so the stored digests are extended rather than replaced.
pub fn capture_context_blob(
    owner: Option<&ConversationIdentity>,
    request: &ResponsesRequest,
    encrypted_content: Option<&str>,
) -> CaptureOutcome {
    let Some(owner) = owner else {
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

    let envelope_digests: Vec<ItemDigest> = envelope.iter().map(item_digest).collect();
    let mut covered = Vec::new();
    if let Some(replayed_blob) = replayed_blob {
        let Some(state) = registry.states.get_mut(owner) else {
            return CaptureOutcome::Discarded(DiscardReason::ReplayMismatch);
        };
        if state.encrypted_content != replayed_blob
            || check_shape(state, request, envelope, tail).is_err()
        {
            remove_state(registry, owner);
            return CaptureOutcome::Discarded(DiscardReason::ReplayMismatch);
        }
        if encrypted_content.is_none() {
            state.updated_at = now;
            return CaptureOutcome::Retained;
        }
        covered.extend_from_slice(&state.covered);
    }
    let Some(encrypted_content) = encrypted_content else {
        return CaptureOutcome::Skipped;
    };
    covered.extend(tail.iter().map(item_digest));

    if encrypted_content.len() > MAX_BLOB_BYTES || covered.len() > MAX_COVERED_ITEMS {
        remove_state(registry, owner);
        return CaptureOutcome::Discarded(DiscardReason::TooLarge);
    }
    let state = ContextState {
        model: request.model.clone(),
        prompt_signature: prompt_signature(request),
        envelope: envelope_digests,
        covered,
        encrypted_content: encrypted_content.to_string(),
        updated_at: now,
    };
    let outcome = CaptureOutcome::Captured {
        covered_items: state.covered.len(),
        blob_bytes: encrypted_content.len(),
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

/// Drops every owner belonging to a Claude Code session, including its agents.
pub fn clear_session_context(session_id: &str) {
    let mut guard = REGISTRY.lock().unwrap();
    if let Some(registry) = guard.as_mut() {
        registry.states.retain(|owner, _| match owner {
            ConversationIdentity::Main(session) | ConversationIdentity::Agent(session, _) => {
                session != session_id
            }
        });
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

fn state_size(owner: &ConversationIdentity, state: &ContextState) -> usize {
    let owner_len = match owner {
        ConversationIdentity::Main(session) => session.len(),
        ConversationIdentity::Agent(session, agent) => session.len() + agent.len(),
    };
    owner_len
        + state.model.len()
        + state.prompt_signature.len()
        + (state.envelope.len() + state.covered.len()) * std::mem::size_of::<ItemDigest>()
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

    fn captured(input: serde_json::Value, blob: &str) -> CaptureOutcome {
        capture_context_blob(Some(&owner()), &request(input), Some(blob))
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
                covered_items: 2,
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

        assert_eq!(
            capture_context_blob(Some(&owner()), &replay.request, Some("blob-2")),
            CaptureOutcome::Captured {
                covered_items: 3,
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
        assert_eq!(
            input,
            vec![compaction("blob-2"), assistant("four"), user("five")]
        );
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
        assert_eq!(
            capture_context_blob(Some(&owner()), &replay.request, None),
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
            capture_context_blob(None, &body, Some("blob")),
            CaptureOutcome::Skipped
        );
        assert_eq!(
            capture_context_blob(Some(&owner()), &body, None),
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
        assert_eq!(
            capture_context_blob(Some(&owner()), &trailing, Some("blob-2")),
            CaptureOutcome::Skipped
        );
        assert!(matches!(
            capture_context_blob(Some(&owner()), &server_compaction, Some("blob-2")),
            CaptureOutcome::Discarded(DiscardReason::ReplayMismatch)
        ));
    }

    #[test]
    fn stale_replay_completion_cannot_chain_onto_newer_state() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap();
        clear_all_context_management_for_tests();
        captured(json!([user("one")]), "blob-old");
        let older = request(json!([user("one"), user("two")]));
        let ReplayOutcome::Replayed(older_replay) = apply_context_replay(Some(&owner()), &older)
        else {
            panic!("expected a replay");
        };
        captured(json!([user("one"), user("two")]), "blob-new");

        assert!(matches!(
            capture_context_blob(Some(&owner()), &older_replay.request, Some("blob-late")),
            CaptureOutcome::Discarded(DiscardReason::ReplayMismatch)
        ));
        let next = request(json!([user("one"), user("two"), user("three")]));
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
            capture_context_blob(Some(owner), &body, Some("blob"));
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
