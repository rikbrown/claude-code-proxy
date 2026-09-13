---
title: Codex
description: Configure ChatGPT Codex authentication, models, reasoning, tools, images, transports, continuation, compaction, and OpenAI-compatible APIs.
---

Codex uses the ChatGPT subscription Responses endpoint at `https://chatgpt.com/backend-api/codex/responses`.

OpenAI's Thibault Sottiaux has publicly welcomed using Codex through other coding
harnesses:

> [Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't
> discriminate on the harness.](https://x.com/thsottiaux/status/2075830097488249060)

## Account and authentication

Sign in with a **ChatGPT Plus or Pro account**, not OpenAI API credentials.

```sh
claude-code-proxy codex auth login
# Headless device-code flow
claude-code-proxy codex auth device
claude-code-proxy codex auth status
```

The proxy owns its tokens and does not read native Codex CLI credentials. It refreshes expiring access tokens with a single-flight guard. See [Files and storage](/reference/files-and-storage/) for credential locations.

## Models and fast mode

Use `claude-code-proxy models` as the current catalog. Model access depends on your ChatGPT account. A model rejected by the subscription produces the upstream error verbatim.

Append `-fast` to any registered Codex model to request `service_tier: "priority"`. For example, `gpt-5.6-sol-fast` selects `gpt-5.6-sol` with fast service. `CCP_CODEX_SERVICE_TIER` or `codex.serviceTier` takes precedence.

## Reasoning

Claude Code's `/effort` value maps to Codex `reasoning.effort`: `low`, `medium`, `high`, `xhigh`, or `max`. A proxy override can also force `none`.

When reasoning is enabled, the proxy requests an automatic reasoning summary and translates summary deltas into Claude Code thinking blocks. Codex may omit a summary for a simple prompt. `CCP_CODEX_REASONING_SUMMARY=off` suppresses summaries while preserving effort and encrypted continuation content.

Claude Code summary compaction requests are capped at low effort by default because they perform extraction over a large transcript. `CCP_COMPACT_EFFORT=off` disables the cap, `none` removes reasoning, and another valid effort sets a different maximum. A compaction request that names a lower effort keeps it. A compaction request that names no effort uses the cap, so compaction does not run at the upstream default effort. Each compaction request writes a `compact_effort_resolved` entry to the proxy log with the incoming effort, the resolved effort, and the cap.

## Tools and multimodal input

- Claude function tools and tool results map to Responses API function calls and outputs.
- Claude Code's forced `web_search_20250305` subrequest uses Codex's standalone
  `/alpha/search` endpoint. It keeps the resolved model, omits search reasoning,
  and preserves non-empty domain filters, so Luna searches do not require a Sol
  Responses turn. Automatic hosted-search requests remain on the full Responses
  API because the standalone endpoint cannot decide whether to invoke a tool.
  Structured result DTOs map back to Anthropic `server_tool_use` and
  `web_search_tool_result` blocks, while standalone text output remains text.
  The proxy locally estimates input and output tokens and reports search usage.
- Top-level base64 user images map to `input_image`.
- Supported base64 images nested in tool results also map to `input_image`.
- Remote image URLs, malformed images, and unsupported tool-result image forms remain textual placeholders.
- Strict JSON schema output maps to Responses `text.format`.

## Transport and continuation

WebSocket is the default transport. Set `CCP_CODEX_TRANSPORT=http` for HTTP SSE, or `auto` to use WebSocket with HTTP fallback only when setup fails before a request is sent.

WebSocket setup honors `HTTP_PROXY` for `ws://`, `HTTPS_PROXY` for the default `wss://` endpoint, `ALL_PROXY` as a fallback, and `NO_PROXY` exclusions. A normal HTTP proxy can therefore carry the default WebSocket connection with CONNECT; TUN mode is not required. Set proxy variables before starting the process and restart after changing them. For example, setting `HTTPS_PROXY` to `http://127.0.0.1:7890` sends HTTPS/WSS destinations through the HTTP proxy at port 7890; it does not require an `https://` proxy URL.

`CCP_CODEX_PREVIOUS_RESPONSE_ID=1` enables append-only WebSocket continuation. A valid identity containing only a Claude Code session ID owns the Main continuation for that session. Each valid direct Agent ID owns an independent continuation and reusable WebSocket within the same session. Nested Agents are keyed by their direct child ID; the parent ID is validated but does not become part of the owner key. The proxy sends `previous_response_id` only when the translated request shape and transcript extension are safe, and only on the exact live WebSocket that produced that response.

An absent, malformed, or ambiguous identity does not reject the HTTP request; that request proceeds without continuation or WebSocket reuse. If the originating socket is missing, dead, or has been replaced, the proxy retries once with the full translated input and without the stale response ID. Continuation and connection state is held only in memory and is lost when the proxy restarts.

Detected auto-review classifier subrequests are intentionally stateless even when valid session and Agent headers are present. They neither consume nor publish continuation or WebSocket ownership.

## Server compaction

Claude Code normally compacts a long conversation by asking the active model to write a portable text summary. Later turns contain that summary instead of the original transcript. This works across providers, but a prose summary can flatten details from a long, tool-heavy Codex session.

Codex server compaction preserves the same boundary in a model-native form. Codex returns an opaque encrypted `compaction` item representing the earlier Responses history. On later turns, the proxy gives that item back to Codex together with selected recent messages and everything added after the boundary. Claude Code still receives its normal portable summary, so the session has a safe fallback.

This is most useful for long coding sessions where continuity after `/compact` or automatic compaction matters. It does not increase the model's context window or prevent Claude Code from compacting. The boundary also takes longer because it adds one Codex request.

### How it works

1. Claude Code reaches a manual or automatic compaction boundary.
2. The proxy sends the translated conversation to Codex with a trailing `compaction_trigger`.
3. Codex returns an encrypted `compaction` item, which the proxy keeps in memory for that Claude Code session and model.
4. Claude Code completes its normal summary request. The proxy uses the resulting summary as an exact anchor.
5. On subsequent matching turns, the proxy replaces the portable summary with the encrypted item, retained recent context, and post-compaction messages.

The encrypted item remains opaque to the proxy. It is stored only in memory and sent back to Codex as native Responses input.

### Enable server compaction

Server compaction is disabled by default. Enable it in `config.json`:

```json
{
  "codex": {
    "serverCompaction": true
  }
}
```

Or enable it for one proxy process:

```sh
CCP_CODEX_SERVER_COMPACTION=1 claude-code-proxy serve
```

### Fallbacks and visibility

Replay requires the same Claude Code session and Codex model with append-only history. A branch, proxy restart, provider or model change, malformed response, upstream failure, memory limit, or 30 minutes without matching activity discards the native state and uses Claude Code's portable summary instead.

While the native request is active, the monitor shows `compacting`. Structured log events named `server_compaction_triggered`, `server_compaction_completed`, and `server_compaction_failed` report each attempt and outcome.

## Context management

Codex can also compact a conversation on the server while it answers. With context management enabled, a Codex request on the full Responses lane carries `context_management` with a `compaction` entry and a token threshold. When the rendered window crosses that threshold, Codex compacts mid-stream and returns an opaque encrypted `compaction` item alongside the normal answer. Unlike server compaction, this needs no extra request and does not wait for a Claude Code compaction boundary.

:::caution
The Responses Lite lane rejects server-side compaction (`X-OpenAI-Internal-Codex-Responses-Lite does not support server-side compaction`). The proxy serves `gpt-5.6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-6-astra` on that lane, so context management is unavailable on the default `gpt-5.6-*` and `gpt-6-*` routes. Requests for those models are sent without `context_management`, and no compaction item is captured or replayed for them. Only full-lane models such as `gpt-5.5` use the feature. A `context_management_unsupported_lane` log event is written once per process when the feature is enabled but the request runs on the Lite lane.
:::

The proxy keeps the last `compaction` item from a completed turn in memory for that conversation owner and model, together with a record of exactly what it covers. A `compaction` item emitted after some of the turn's output items contains those items as well as the request input, so the covered history is the request's conversation items followed by the output items (assistant text, tool calls, and reasoning) that were emitted before the item. Output emitted after the item is not covered and stays explicit. On the next turn, if the translated input starts with exactly that covered history and continues from there, the proxy sends the item in place of the covered history and keeps only what follows in full. Claude Code still sends its complete history; only the upstream request is shortened.

If the proxy cannot establish what a `compaction` item covers, for example because the item cannot be positioned against the turn's output, it stores nothing and the next turn sends the full history.

### Enable context management

Context management is disabled by default. Enable it in `config.json`:

```json
{
  "codex": {
    "contextManagement": true,
    "contextManagementThreshold": 200000
  }
}
```

Or enable it for one proxy process:

```sh
CCP_CODEX_CONTEXT_MANAGEMENT=1 claude-code-proxy serve
```

`codex.contextManagementThreshold` or `CCP_CODEX_CONTEXT_MANAGEMENT_THRESHOLD` sets the `compact_threshold` in tokens. The default is `200000`. Codex rejects values below `1000`, so the proxy ignores them and uses the default.

### Fallbacks and visibility

Replay requires the same conversation owner, Codex model, system prompt, tools, and request shape, and the conversation must begin with exactly the covered history. A branch, an edited or regenerated assistant reply, a provider or model change, a proxy restart, a memory limit, or 30 minutes without matching activity discards the stored item and sends the full history. When two turns for the same conversation overlap, only the newest may store an item, so a slower older turn cannot install an item that a newer history then matches. The item is never combined with a server compaction replay in one request. State is held only in memory and is lost when the proxy restarts. With `CCP_TRAFFIC_LOG=1`, traffic captures record the encrypted `compaction` item together with the rest of the upstream request and response, as they do for prompt text and tool output.

Structured log events named `context_management_blob_captured`, `context_management_replayed`, `context_management_discarded`, `context_management_capture_refused`, and `context_management_capture_superseded` report each capture, replay, discard, and refusal with counts and reasons only.

## OpenAI-compatible APIs

`CCP_CODEX_RESPONSES_API=1` enables both `POST /v1/responses` and `POST /v1/chat/completions`. The setting is under Codex configuration, but the routes also accept Kimi, Grok, OpenCode Go, and Cursor models.

The Responses route preserves native JSON or SSE response bodies for registered Codex models. The Chat Completions route translates standard text messages, reasoning effort, JSON object or JSON Schema output, and buffered or streaming responses. Its omitted reasoning effort defaults to `medium`; the proxy-wide Codex effort override still takes precedence.

The proxy replaces incoming credentials with stored Codex auth for both routes. Response retrieval or deletion, function calling through Chat Completions, and WebSocket ingress are outside their scope. See [HTTP API](/reference/http-api/) for supported Chat Completions fields and error behavior.

## Images API

`CCP_CODEX_IMAGES_API=1` separately enables `POST /v1/images/generations` and `POST /v1/images/edits`. The routes reuse the proxy's stored ChatGPT OAuth session and target the ChatGPT Codex image backend; no OpenAI Platform API key is required.

```sh
CCP_CODEX_IMAGES_API=1 claude-code-proxy serve
```

The model defaults to and is restricted to `gpt-image-2`. Generation accepts JSON. Editing accepts either Codex JSON data URLs or OpenAI-style multipart uploads, which the proxy validates and converts into the Codex JSON contract. Results are returned as `data[].b64_json`. Masks, remote URLs, URL-formatted output, and image variations are not supported.

This is an internal ChatGPT Codex interface rather than the public Platform Images API. It consumes the signed-in account's image quota and can change without public API compatibility guarantees. Image prompts, uploads, generated base64, and upstream error bodies are excluded from traffic captures and persistent error diagnostics.

Because callers are not authenticated, binding to a LAN address lets every firewall-admitted host consume the signed-in account's quota. Restrict the listener to a trusted interface/subnet and never expose it through router forwarding, UPnP, a public tunnel, or permissive IPv6 rules.

See [Configuration](/reference/configuration/) for every Codex setting and [Troubleshooting](/using/troubleshooting/) for auth, model, and transport failures.
