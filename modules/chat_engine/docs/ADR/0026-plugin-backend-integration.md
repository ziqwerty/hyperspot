Created:  2026-02-23 by Constructor Tech
Updated:  2026-03-09 by Constructor Tech
# ADR-0026: Internal Plugin Interface for Backend Integration

**Date**: 2026-02-23

**Status**: accepted — supersedes ADR-0006 (Synchronous HTTP Webhooks with Streaming)

**ID**: `cpt-cf-chat-engine-adr-plugin-backend-integration`

## Context and Problem Statement

ADR-0006 established HTTP webhooks as the integration mechanism between Chat Engine and message-processing backends: Chat Engine made outbound HTTP POST requests to a `webhook_url` configured per session type and handled all resilience concerns (auth, retry, circuit breaker, timeout) itself. This approach couples Chat Engine to transport-level details and forces every deployment to duplicate resilience infrastructure.

In practice, backend integrations are implemented as **code modules within Chat Engine** — not as independently deployed external services. A plugin is a Rust module inside the Chat Engine codebase that implements the `ChatEngineBackendPlugin` trait. The plugin decides how to communicate with external services (HTTP to LLM gateway, vector DB, etc.); Chat Engine is not involved in that transport. How should Chat Engine integrate with backend plugins while keeping its core free of transport, auth, and resilience logic?

## Decision Drivers

* Chat Engine core must not contain transport, auth, retry, or circuit breaker logic
* Plugins are internal code within Chat Engine — a plugin is a trait implementation, not an external service
* Each plugin independently decides how to communicate with external services it depends on
* Request/response format between a plugin and its external service must conform to Chat Engine schemas
* Session type configuration must reference a specific plugin by `plugin_instance_id`
* A compatibility path for legacy HTTP webhook-based services must exist without changes to Chat Engine core

## Considered Options

* **Option 1: HTTP webhooks (ADR-0006)** — Chat Engine makes outbound HTTP POST to `webhook_url` per session type; manages auth, retry, circuit breaker itself
* **Option 2: Internal plugin trait** — plugins are code inside Chat Engine implementing `ChatEngineBackendPlugin`; Chat Engine calls trait methods directly; each plugin manages its own outbound communication
* **Option 3: Hybrid** — internal plugin trait as primary; a first-party `webhook-compat` plugin wraps legacy HTTP webhook-based external services

## Decision Outcome

Chosen option: **Option 3 (Hybrid)**, with the internal plugin interface as the primary integration mechanism.

Chat Engine defines a `ChatEngineBackendPlugin` trait. Plugins are Rust modules inside the Chat Engine codebase that implement this trait and are registered in Chat Engine's internal plugin registry at startup. A session type references its plugin via `plugin_instance_id` (a GTS ID string). On each operation, Chat Engine looks up the plugin by `plugin_instance_id` and calls the appropriate trait method.

Plugins own all outbound communication. For example, the LLM gateway plugin makes HTTP requests to the LLM gateway service using request/response schemas derived from Chat Engine base schemas. During `on_session_type_configured`, the LLM gateway plugin queries the **Model Registry** service to retrieve the capabilities supported by the configured model (available parameters, allowed values, defaults) and returns them as `Vec<Capability>`. The first-party `webhook-compat` plugin similarly makes outbound HTTP requests to legacy webhook endpoints, keeping Chat Engine core free of any webhook logic.

### Consequences

* Good, because Chat Engine core has zero auth, retry, circuit breaker, or timeout code
* Good, because plugin-to-external-service communication is fully encapsulated in the plugin
* Good, because request/response format is governed by Chat Engine schemas — plugins cannot break the contract
* Good, because `SessionType.available_capabilities` is populated by the plugin via `on_session_type_configured`, eliminating developer-plugin catalog drift
* Good, because the `webhook-compat` plugin preserves backward compatibility for existing HTTP webhook services
* Bad, because adding a new plugin requires a code change and rebuild of Chat Engine
* Bad, because the `webhook-compat` plugin adds a thin indirection layer for legacy webhook services

### Confirmation

Confirmed when:

- A session type configured with `plugin_instance_id` calls the correct plugin trait method on each operation
- `on_session_type_configured` is called on session type setup and its result stored as `SessionType.available_capabilities`
- LLM gateway plugin queries Model Registry during `on_session_type_configured` to resolve available capabilities for the configured model
- LLM gateway plugin successfully makes HTTP requests to the LLM gateway service using Chat Engine-derived schemas
- `webhook-compat` plugin wraps a legacy HTTP webhook service without any changes to Chat Engine core

## Pros and Cons of the Options

### Option 1: HTTP webhooks (ADR-0006)

Chat Engine makes outbound HTTP POST to `webhook_url` per session type and manages all resilience logic.

* Good, because any HTTP server can serve as a backend without code inside Chat Engine
* Bad, because Chat Engine must implement and maintain auth, retry, circuit breaker, and timeout per session type
* Bad, because `webhook_url` is an unversioned string with no schema enforcement

### Option 2: Internal plugin trait (pure)

Plugins are code inside Chat Engine; no compatibility mechanism for external HTTP services.

* Good, because zero transport boilerplate in Chat Engine core
* Good, because trait interface is type-safe and schema-enforced at compile time
* Bad, because legacy webhook-based services cannot integrate without being rewritten as plugins

### Option 3: Hybrid (chosen)

Internal plugin trait as primary; `webhook-compat` plugin wraps legacy HTTP webhook services.

* Good, because Chat Engine core stays transport-free
* Good, because legacy HTTP webhook services remain supported via the compat plugin
* Good, because plugin is authoritative for capabilities, schemas, and resilience strategy
* Bad, because new plugins require a Chat Engine code change and rebuild

## Plugin API Contract

Chat Engine defines the `ChatEngineBackendPlugin` trait in the `chat-engine-sdk` crate. Plugin methods:

| Method | Trigger | Returns |
|--------|---------|---------|
| `on_session_type_configured` | Session type is configured with this plugin | `Vec<Capability>` stored as `SessionType.available_capabilities` |
| `on_session_created` | Session is created | Establishes any per-session plugin state |
| `on_message` | User sends a message | `ResponseStream` of content chunks |
| `on_message_recreate` | User recreates a message | `ResponseStream` of content chunks |
| `on_session_summary` | Summarization triggered | `ResponseStream` of summary content |
| `health_check` | Optional liveness probe | Health status |

Full trait and context struct definitions are in `chat-engine-sdk` and documented in DESIGN.md §3.3.2.

## N:1 Session Types → Plugin

Multiple session types can share the same `plugin_instance_id`. Each session type carries a `metadata` configuration bag forwarded to the plugin in every call context (`session_type_metadata` field). This allows a single plugin instance to serve multiple differently-configured session types — for example, a single LLM gateway plugin serving session types that differ only in system prompt or default model.

## Related Design Elements

**Actors**:

* `cpt-cf-chat-engine-actor-backend-plugin` — internal plugin code within Chat Engine; implements `ChatEngineBackendPlugin` trait

**Requirements**:

* `cpt-cf-chat-engine-fr-send-message` — plugin `on_message` call replaces webhook POST
* `cpt-cf-chat-engine-fr-create-session` — plugin `on_session_created` call replaces session.created webhook event
* `cpt-cf-chat-engine-fr-schema-extensibility` — plugin registers GTS derived schemas at startup

**Superseded ADRs**:

* ADR-0006 (Webhook Protocol) — superseded; `webhook_url` replaced by `plugin_instance_id`
* ADR-0011 (Circuit Breaker) — responsibility moved to plugin
* ADR-0013 (Timeout Configuration) — responsibility moved to plugin

**Related ADRs**:

* ADR-0003 (Streaming Architecture) — unchanged; plugin provides `ResponseStream`
* ADR-0010 (Stateless Scaling) — unchanged; plugin resolved per-request from registry
* ADR-0027 (LLM Gateway Plugin) — LLM gateway plugin: capability resolution via Model Registry, schema extensions, message processing via LLM gateway service