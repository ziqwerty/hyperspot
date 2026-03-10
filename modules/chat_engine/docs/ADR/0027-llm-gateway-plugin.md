Created:  2026-03-06 by Constructor Tech
Updated:  2026-03-09 by Constructor Tech
---
status: accepted
date: 2026-03-06
---

# ADR-0027: LLM Gateway Plugin

**ID**: `cpt-cf-chat-engine-adr-llm-gateway-plugin`

## Context and Problem Statement

Chat Engine defines a generic plugin interface (`ChatEngineBackendPlugin` trait, ADR-0026) for backend integrations. The first concrete plugin is the **LLM gateway plugin** — it connects Chat Engine to an LLM gateway service and a Model Registry service. The plugin must solve three concerns without modifying Chat Engine core:

1. **Capability resolution** — determine which LLM parameters (model, temperature, max_tokens, web_search) are available for a given session type and expose them through the capabilities system (ADR-0002)
2. **Schema extension** — store LLM-specific data (response facts, token usage, plugin configuration) in Chat Engine's `metadata` JSONB fields with typed validation
3. **Message processing** — forward user messages to the LLM gateway service and stream responses back

How should the LLM gateway plugin implement these concerns while keeping Chat Engine agnostic to LLM specifics?

## Decision Drivers

* Capabilities must come from a reliable external source — hardcoding them in the plugin creates drift when models change
* User-selectable LLM params (model, temperature, max_tokens, web_search) must go through the capabilities system (ADR-0002) — user configures `Session.enabled_capabilities` from `SessionType.available_capabilities`
* Plugin configuration (default_model) belongs in `SessionType.metadata` — opaque to Chat Engine
* LLM response facts (model_used, finish_reason, temperature_used) belong in `Message.metadata`
* Base `Usage` schema must remain abstract and unchanged — `LlmUsage` is a standalone schema nested inside `LlmMessageMetadata.usage` as a plain dict within the JSONB field, not a derived type of `Usage`
* Schema validation must work without modifying Chat Engine core
* LLM plugin schema namespace must be isolated from other plugins

## Considered Options

* **Option 1: Model Registry + GTS derived schemas** — capabilities fetched from Model Registry at configuration time; LLM-specific metadata via registered GTS derived types; message processing via LLM gateway HTTP calls
* **Option 2: Hardcoded capabilities + GTS derived schemas** — capabilities defined as constants in plugin code; same schema extension approach
* **Option 3: All config in SessionType.metadata** — no capabilities for LLM params; everything in developer config; user cannot override per-session; flat untyped metadata

## Decision Outcome

Chosen option: "Model Registry + GTS derived schemas", because it keeps capabilities in sync with actual model support, separates user-selectable concerns (capabilities) from developer configuration (SessionType.metadata), provides typed validation for LLM-specific fields, and keeps the LLM plugin namespace isolated.

### Plugin Lifecycle

1. **Startup** — plugin registers GTS derived schemas (`LlmSessionTypeMetadata`, `LlmMessageMetadata`, `LlmUsage`) and entity schemas in the GTS schema registry
2. **Session type configuration** (`on_session_type_configured`) — plugin reads `default_model` from `SessionType.metadata`, queries **Model Registry** to retrieve model-supported capabilities (parameters, allowed values, defaults), and returns `Vec<Capability>`
3. **Session creation** (`on_session_created`) — plugin establishes per-session state if needed
4. **Message processing** (`on_message`, `on_message_recreate`) — plugin builds an LLM gateway request from the message context and user-selected `CapabilityValue[]`, calls the LLM gateway service via HTTP, and streams the response back as `ResponseStream`
5. **Summarization** (`on_session_summary`) — plugin routes summary requests to the LLM gateway service

### External Service Dependencies

| Service | Used In | Purpose |
|---------|---------|---------|
| **Model Registry** | `on_session_type_configured` | Retrieve model capabilities (supported parameters, allowed values, defaults) |
| **LLM Gateway** | `on_message`, `on_message_recreate`, `on_session_summary` | Forward messages and receive streamed LLM responses |

### Consequences

* Good, because capabilities reflect actual model support — Model Registry is the single source of truth
* Good, because adding a new model or changing model parameters requires no plugin code changes
* Good, because users can select model, temperature, max_tokens, and web_search per session via the capabilities UI
* Good, because `LlmUsage` provides typed token counts (prompt/completion/cached) without breaking the abstract base `Usage` schema
* Good, because Chat Engine validates LLM metadata blobs against registered GTS schemas (FR-021)
* Good, because plugin schema namespace is isolated (`gts.x.chat_engine.llm_gateway.*`) — no conflicts with other plugins
* Good, because base schemas remain unchanged — non-LLM plugins are unaffected
* Bad, because plugin depends on Model Registry availability during `on_session_type_configured`
* Bad, because plugin must register GTS schemas at startup before any session type can be created
* Bad, because Chat Engine must implement schema registry lookup for metadata validation (FR-021 is `p2` — not yet implemented)

### Confirmation

Confirmed when:

- LLM plugin registers `LlmSessionTypeMetadata`, `LlmMessageMetadata`, and `LlmUsage` in GTS at startup
- LLM plugin queries Model Registry during `on_session_type_configured` and returns model-specific capabilities
- Creating a session type with LLM plugin validates `SessionType.metadata` against `LlmSessionTypeMetadata`
- Assistant message responses include `Message.metadata` with `model_used`, `finish_reason`, and `LlmUsage` token counts
- Non-LLM session types are unaffected by LLM schema registration
- `on_message` successfully calls LLM gateway and streams response back through Chat Engine

## Pros and Cons of the Options

### Option 1: Model Registry + GTS derived schemas (chosen)

Capabilities from Model Registry; typed metadata via GTS derived types; LLM gateway HTTP calls for message processing.

* Good, because capabilities stay in sync with model support automatically
* Good, because user control over LLM params per session via standard capabilities UI
* Good, because schema validation without Chat Engine core changes
* Good, because plugin namespace isolation prevents schema conflicts
* Bad, because Model Registry must be available during session type configuration
* Bad, because requires FR-021 (schema-extensibility) implementation before metadata validation is active

### Option 2: Hardcoded capabilities + GTS derived schemas

Capabilities defined as constants in plugin code; same schema extension approach.

* Good, because no external dependency for capability resolution
* Good, because schema validation same as Option 1
* Bad, because capability definitions drift when models are added or changed
* Bad, because plugin code changes required for every model update
* Bad, because different deployments cannot have different model catalogs without code forks

### Option 3: All config in SessionType.metadata

LLM params all in developer config; no capabilities; flat untyped metadata.

* Good, because simpler — no capability declarations, no schema registration
* Bad, because users cannot change model or temperature per session
* Bad, because no validation — typos and type mismatches silently accepted
* Bad, because no namespace isolation — metadata conflicts between plugins possible

## Capability Resolution via Model Registry

During `on_session_type_configured`, the LLM gateway plugin:

1. Reads `default_model` from `SessionType.metadata` (via `LlmSessionTypeMetadata`)
2. Queries the **Model Registry** service to retrieve model capabilities
3. Maps the Model Registry response to `Vec<Capability>` using Chat Engine's capability schema

The actual set of capabilities and their `enum_values` / defaults depend on the model's entry in the Model Registry — different models may expose different capabilities.

Example capabilities for a typical LLM model:

- `{ id: "model", name: "AI Model", type: "enum", default_value: "gpt-4o", enum_values: ["gpt-4o", "gpt-4o-mini", "o1"] }`
- `{ id: "temperature", name: "Temperature", type: "int", default_value: 70 }` — integer 0–100 maps to 0.0–1.0
- `{ id: "max_tokens", name: "Max Tokens", type: "int", default_value: 4096 }`
- `{ id: "web_search", name: "Web Search", type: "bool", default_value: false }`

## Schema Extensions

### Metadata Schemas

**GTS Schema IDs registered by LLM gateway plugin**:

| Schema | GTS ID | Extension Point |
|--------|--------|-----------------|
| `LlmSessionTypeMetadata` | `gts://gts.x.chat_engine.llm_gateway.session_type_metadata.v1` | `SessionType.metadata` |
| `LlmMessageMetadata` | `gts://gts.x.chat_engine.llm_gateway.message_metadata.v1` | `Message.metadata` |
| `LlmUsage` | `gts://gts.x.chat_engine.llm_gateway.usage.v1` | nested in `LlmMessageMetadata.usage` |

**`LlmSessionTypeMetadata` fields**: `default_model: string`

**`LlmMessageMetadata` fields**: `model_used: string`, `finish_reason: enum[stop|length|content_filter|tool_calls]`, `temperature_used?: number`, `usage?: LlmUsage`

**`LlmUsage` fields**: `prompt_tokens: int`, `completion_tokens: int`, `total_tokens: int`, `cached_tokens?: int`

### Entity Schemas

GTS entity schemas registered by LLM gateway plugin (extend base Chat Engine schemas via JSON Schema `allOf`, overriding the `metadata` property; `metadata` is stored as JSONB):

| Schema | GTS ID | Extends |
|--------|--------|---------|
| `LlmMessage` | `gts://gts.x.chat_engine.llm_gateway.message.v1` | `common/Message` |
| `LlmSessionType` | `gts://gts.x.chat_engine.llm_gateway.session_type.v1` | `common/SessionType` |
| `LlmMessageGetResponse` | `gts://gts.x.chat_engine.llm_gateway.message_get_response.v1` | `message/MessageGetResponse` |
| `LlmMessageNewResponse` | `gts://gts.x.chat_engine.llm_gateway.message_new_response.v1` | `webhook/MessageNewResponse` |
| `LlmMessageRecreateResponse` | `gts://gts.x.chat_engine.llm_gateway.message_recreate_response.v1` | `webhook/MessageRecreateResponse` |
| `LlmStreamingCompleteEvent` | `gts://gts.x.chat_engine.llm_gateway.streaming_complete_event.v1` | `streaming/StreamingCompleteEvent` |

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

* `cpt-cf-chat-engine-fr-schema-extensibility` — GTS derived schema registration is the mechanism used to extend metadata fields
* `cpt-cf-chat-engine-adr-plugin-backend-integration` — plugin system and trait interface (ADR-0026)
* `cpt-cf-chat-engine-adr-capability-model` — capabilities for user-selectable LLM params (ADR-0002)
* `cpt-cf-chat-engine-adr-session-metadata` — JSONB extension point and GTS validation strategy (ADR-0020)