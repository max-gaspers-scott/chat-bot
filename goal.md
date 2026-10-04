# Project: Rust agent harness for development work

I'm building a coding agent harness as a single distributable Rust binary. Users don't supply API keys. Instead, they authenticate against my server, which issues short-lived JWTs, and my proxy forwards requests to an OpenAI-compatible endpoint. I already have a separate TUI and web frontend, so this project is the harness core only, designed as a library that frontends consume.

Before writing any code, read the current docs and source for the `rig` crate version we pin (check docs.rs and the source in ~/.cargo/registry after adding it). Don't rely on memory for Rig's API, because it changes between versions. Then show me a short plan and wait for my approval.

## Environment details
- Proxy base URL: https://cloud-wolf.team-stingray.com/
- JWT endpoint: [how to obtain a token, request/response shape, refresh mechanism, expiry]
- Model name(s) exposed by the proxy: gemini-2.5-flash

## Architecture requirements

Cargo workspace with:
- `harness` (library crate): agent loop, tools, memory trait, events, client wrapper
- `harness-cli` (binary): minimal streaming CLI for testing only. The real TUI/web UIs come later.

The harness exposes an **event stream out** (text deltas, tool call started/finished, approval requested, errors, turn finished) and **commands in** (user message, approve/deny, cancel). Frontends are just clients of this interface.

## Phase 1 scope (do these in order, committing after each)

**1. Client with token refresh** ✅ COMPLETE
- Rig's OpenAI client with the Mira dialect, custom base URL, and a `HarnessHttpClient` transport that implements `HttpClientExt` — no custom `ProviderClient` needed.
- The current JWT lives in a shared `Arc<RwLock<TokenState>>` and is injected per request (overriding rig's static header).
- Proactive refresh background task (`spawn_proactive_refresh`) wakes every 10s and refreshes when expiry is within 60s.
- On 401: refreshes once and retries.
- Exponential back-off (jittered, capped at 30s) on 429 and 5xx, up to 4 retries.
- `harness-cli` provides a hello-world `chat` command using `AgentBuilder`.
- Integration test in `harness/tests/token_refresh.rs`: stands up a mock HTTP server, mints a 2s JWT, confirms 401→refresh→retry succeeds and the store holds the new token. All 10 tests pass, clippy clean.

**2. Agent loop**
- Write our own loop (don't rely on Rig's built-in multi-turn loop unless it supports all of the following). Signature roughly: `run_task(task, memory, events, cancel_token, config) -> Result<TaskOutcome>`. It runs until the model stops calling tools or `max_turns` is hit, and can be called repeatedly as the user sends new requests against the same memory.
- Define our own `Memory` trait (load, append, replace) and our own `Message` type. Convert to Rig's types only at call time. Provide an in-memory implementation (`Vec<Message>`, or wrapping `rig::memory::InMemoryConversationMemory` if that works cleanly). Later we'll add DB persistence and compaction behind this trait.
- Cancellation via `tokio_util::sync::CancellationToken`. Cancelling must stop mid-stream and mid-tool-call.
- An approval hook before executing tools that need it.
- Never split a tool call from its result in memory.
- Append-only JSONL transcript log of every message and event per session.
- System prompt plus loading of `AGENTS.md` from the workspace root, if present.
- A **mock provider** that returns scripted responses, with unit tests for: plain reply, tool call round trip, approval granted/denied, cancellation, max turns, token expiry mid-task.

**3. Tools** (implemented in-process, no external MCP servers)
All paths are restricted to the workspace root. Tool outputs are capped, and truncated output tells the model what was cut and how to read more.
- `read_file`: offset/limit, line numbers, default size cap
- `apply_patch`: use the OpenAI `apply_patch` format. Check whether the Apache-2.0 `apply-patch` crate from openai/codex can be vendored (keep the license notice), otherwise implement it. Failure messages must be clear enough for the model to recover.
- `write_file`
- `bash`: `tokio::process` in its own process group so timeout/cancel kills the whole tree. Default timeout, head+tail output truncation. Requires approval every time, with an "always allow for this session" option. No allowlist parsing yet.
- `grep` (via `grep-searcher` + `grep-regex`, or shell out to `rg` if present), `glob` (`globset`), file walking via `ignore`
- Use `similar` to generate diffs for approval previews.

Each tool is a typed Rig `Tool` impl (or whatever fits our own loop) with a JSON schema and a clear, concise description.

## Explicitly out of scope for now
Context compaction/pruning beyond output truncation, MCP (`rmcp`), PTY support, subagents, database persistence, bash allowlist parsing, the custom codegen-macro tools (they'll come later as typed tools), and any TUI/web work.

## Working style
- Plan first, then implement step by step, running `cargo check`, `cargo clippy`, and `cargo test` at each step.
- Keep dependencies lean and pin versions.
- Prefer small modules with clear boundaries, such as `client`, `loop`, `memory`, `events`, `tools/*`, `transcript`.
- If something in this spec conflicts with what you find in Rig's actual API, tell me and propose an alternative rather than working around it silently.
- When Phase 1 is done, summarize what exists, what's untested, and what you'd tackle next.
