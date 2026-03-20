---
status: proposed
date: 2026-03-12
---

# ADR-0005: Adopt Open Responses Protocol for LLM Completion Requests

**ID**: `cpt-cf-llm-gateway-adr-open-responses-protocol`

## Context and Problem Statement

LLM Gateway provides unified access to multiple LLM providers through a single API. As the AI ecosystem evolves, providers are diverging beyond simple chat completions — introducing reasoning tokens, internal tool calls, citations, and richer structured outputs. The Gateway needs to define its external API protocol for LLM completion requests in a way that balances broad client compatibility with the ability to expose these advanced provider-specific features without breaking the base contract.

The legacy OpenAI Chat Completions API (`/v1/chat/completions`) has become a de facto standard, but its output format is limited and cannot represent newer model capabilities. Providers are gradually moving away from this API, with the majority offering an analog of OpenAI Responses API for newer models. We need a protocol that serves as the Gateway's canonical request/response format while remaining extensible and avoiding vendor lock-in.

## Decision Drivers

* Broad client compatibility — existing OpenAI-compatible tools and SDKs should work with minimal changes
* Extensibility — protocol must accommodate provider-specific features (reasoning tokens, internal tool use, citations, web search results) without breaking the base contract
* Open specification — avoid vendor lock-in to a single provider's proprietary format
* Future-proofing — providers are moving beyond simple chat completions towards richer response models with structured output items
* Industry adoption trajectory — the chosen protocol should have growing, not declining, ecosystem support
* Compliance tooling — the protocol should facilitate audit, content filtering, and governance at the response-item level

## Considered Options

* OpenAI Chat Completions API
* Custom CyberFabric API
* Vendor-specific API (OpenAI, Anthropic, or other)
* Open Responses Protocol

## Decision Outcome

Chosen option: "Open Responses Protocol", because it combines OpenAI client compatibility with an extensible, item-based response model that accommodates provider-specific features. As an open specification it avoids vendor lock-in while benefiting from existing ecosystem tooling. Its design explicitly supports the richer output structures (reasoning, internal tool calls, citations) that newer models produce — capabilities the legacy Chat Completions API cannot represent.

### Consequences

* Gateway API layer must implement the Open Responses protocol request/response format, replacing the current OpenAI-style endpoints with Responses-style semantics
* Provider adapters must translate between Open Responses format and each provider's native API, mapping provider-specific output items to the protocol's extension points
* SDK schemas (`llm-gateway-sdk/schemas/`) must be updated to align with Open Responses types (items-based response structure instead of single-message choices)
* Existing consumers using OpenAI-compatible clients can connect with minimal migration effort, since Open Responses maintains backward compatibility with the OpenAI SDK interface
* The team must track Open Responses specification evolution and update the Gateway implementation as the spec matures
* Streaming format shifts from choice-delta chunks to item-based streaming events, requiring updates to the SSE contract

### Confirmation

Implementation verified via:

* Code review confirming the API layer implements the Open Responses specification for request/response handling
* Integration tests using standard OpenAI SDK clients to confirm backward compatibility
* Provider adapter tests validating correct translation to/from each provider's native format (OpenAI, Anthropic, Google)
* Streaming tests confirming item-based SSE events are correctly emitted and consumable by compatible clients

## Pros and Cons of the Options

### OpenAI Chat Completions API

The legacy `/v1/chat/completions` endpoint format that has become a de facto industry standard for LLM interaction.

* Good, because wide client support — most AI tools and SDKs implement this format
* Good, because well-documented and battle-tested in production
* Good, because simple request/response model is easy to understand
* Bad, because limited output format — single message with optional tool calls, no structured output items
* Bad, because no support for reasoning tokens, internal tool calls, or citations in the response
* Bad, because providers are migrating new models to richer response APIs, making this format increasingly incomplete
* Bad, because extending it requires non-standard modifications that break client assumptions

### Custom CyberFabric API

A bespoke protocol designed specifically for CyberFabric Gateway's requirements, with full control over request/response structure.

* Good, because complete control over API design — can implement any feature needed
* Good, because can be optimized for Gateway-specific patterns (provider routing, fallback, hooks)
* Good, because no dependency on external specification evolution
* Bad, because requires custom clients for every consumer — high integration cost
* Bad, because no existing ecosystem tooling, SDKs, or documentation to leverage
* Bad, because ongoing maintenance burden to design, document, and evolve the specification
* Bad, because creates vendor lock-in to CyberFabric — consumers cannot reuse their API integration elsewhere

### Vendor-specific API (OpenAI, Anthropic, or other)

Adopting one vendor's native API as the Gateway's canonical format (e.g., Anthropic Messages API or OpenAI Responses API as a proprietary format).

* Good, because established API with production track record
* Good, because existing clients and SDKs for the chosen vendor
* Good, because well-documented with vendor support
* Bad, because feature support is limited to the chosen vendor's view of the world — other providers' unique capabilities are difficult to represent
* Bad, because creates dependency on a single vendor's API evolution decisions
* Bad, because would need proprietary extensions anyway to cover capabilities from other providers
* Bad, because consumers locked into one vendor's API paradigm even when using different underlying providers

### Open Responses Protocol

An open specification based on OpenAI's Responses API design, governed as an open standard. Defines an items-based response model where each response contains typed output items (message, tool call, reasoning, etc.) that are extensible via provider-specific item types.

* Good, because OpenAI-compatible — existing clients using OpenAI SDKs can connect with minimal changes
* Good, because extensible by design — provider-specific items can be added without breaking the base contract
* Good, because open specification — not controlled by a single vendor, community-governed evolution
* Good, because embraced by xAI and other providers, indicating growing industry adoption (see [Open Responses specification](https://www.openresponses.org/) and [xAI Grok API compatibility](https://docs.x.ai/docs/api-reference))
* Good, because compliance tooling support — item-level granularity facilitates content filtering, audit, and governance
* Good, because items-based response model naturally represents reasoning tokens, internal tool calls, citations, and other rich outputs
* Bad, because relatively new specification — less production mileage than the Chat Completions API
* Bad, because adoption trajectory is uncertain — may or may not achieve wide acceptance beyond early adopters
* Bad, because specification may evolve in ways that require Gateway updates

## More Information

The Open Responses protocol emerged as an effort to standardize the richer response model that providers are converging on. Unlike the legacy Chat Completions API which returns a single message per choice, Open Responses returns a list of typed output items — enabling first-class representation of reasoning traces, internal tool invocations, citations, and other structured outputs that newer models produce.

This decision aligns with the Gateway's pass-through design principle (`cpt-cf-llm-gateway-principle-pass-through`): the protocol's item-based structure allows the Gateway to forward provider-specific items transparently without needing to interpret their content.

### Provider Parameter Extensions

The Open Responses specification covers a common denominator of parameters across providers, but individual providers expose additional capabilities not represented in the base schema (e.g., Anthropic's `top_k`, Google's `safety_settings`, provider-specific caching directives). The Gateway may extend the `CreateResponseBody` input schema with additional fields beyond the Open Responses specification to expose these provider-specific parameters. Such extensions must follow these principles:

* **Additive only** — extensions add new optional fields; they never modify or remove fields defined by the Open Responses spec
* **Namespaced when provider-specific** — fields that apply to a single provider should be grouped under a provider-keyed object (e.g., `"provider_options": {"anthropic": {"top_k": 40}}`) to avoid top-level namespace pollution and make it clear these are non-standard
* **Pass-through for unknown fields** — provider adapters should forward recognized extension fields to the underlying provider and ignore unrecognized ones, consistent with the Gateway's pass-through design
* **Documented in SDK schemas** — any extension fields must be reflected in the `llm-gateway-sdk` JSON schemas with clear descriptions indicating they are Gateway extensions beyond the Open Responses spec

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-llm-gateway-fr-chat-completion-v1` — Defines the protocol format for chat completion requests and responses
* `cpt-cf-llm-gateway-fr-streaming-v1` — Streaming format follows from the protocol choice (item-based streaming events)
* `cpt-cf-llm-gateway-fr-tool-calling-v1` — Tool calling request/response format is defined by the protocol's tool items
* `cpt-cf-llm-gateway-fr-structured-output-v1` — Structured output support follows the protocol's schema mechanism
* `cpt-cf-llm-gateway-component-api-layer` — API layer implements this protocol as the Gateway's external interface
* `cpt-cf-llm-gateway-component-provider-adapters` — Adapters translate between this protocol and each provider's native API format
