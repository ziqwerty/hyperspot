Created:  2026-02-04 by Constructor Tech
Updated:  2026-03-06 by Constructor Tech
# ADR-0003: Streaming-First with HTTP Chunked Transfer

**Date**: 2026-02-04

**Status**: accepted

**ID**: `cpt-cf-chat-engine-adr-streaming-architecture`

## Context and Problem Statement

Chat Engine must minimize time-to-first-byte for assistant responses to provide responsive user experience. Responses from backends (especially LLM-based) can take seconds to complete. How should Chat Engine handle response delivery to maximize perceived responsiveness?

## Decision Drivers

* Minimize time-to-first-byte for user-perceived responsiveness
* Support backends that stream (LLMs) and backends that don't (rule-based)
* Enable client to display partial responses as they arrive
* Allow cancellation of slow responses to save resources
* HTTP for both client and webhook communication (simple integration)
* Backpressure handling for slow clients
* Minimal latency overhead from proxying

## Considered Options

* **Option 1: Streaming-first with HTTP chunked transfer** - All responses stream via HTTP chunked encoding
* **Option 2: Buffered responses** - Wait for complete response from backend, then send to client
* **Option 3: Optional streaming** - Backends declare if they stream, Chat Engine adapts behavior per backend

## Decision Outcome

Chosen option: "Streaming-first with HTTP chunked transfer", because it minimizes time-to-first-byte (< 200ms requirement), enables responsive UX for slow backends, supports cancellation via connection close saving compute resources, and keeps both webhook and client protocols simple (always HTTP streaming with NDJSON format).

### Consequences

* Good, because first response chunk arrives at client within 200ms of backend streaming
* Good, because perceived latency is much lower than buffered approach
* Good, because clients can cancel slow responses (stop button)
* Good, because non-streaming backends work transparently (wrapped in stream adapter)
* Good, because webhook protocol remains simple HTTP (no WebSocket complexity for backend devs)
* Good, because HTTP/2 enables multiple concurrent streams over single connection
* Bad, because streaming overhead adds ~10ms latency per chunk forwarding
* Bad, because partial responses require special handling if connection drops
* Bad, because backpressure management adds complexity (buffer limits, flow control)

## Related Design Elements

**Actors**:
* `cpt-cf-chat-engine-actor-client` - Receives HTTP chunked responses with streaming message chunks (NDJSON)
* `cpt-cf-chat-engine-actor-backend-plugin` - Streams HTTP responses (chunked transfer encoding)

**Requirements**:
* `cpt-cf-chat-engine-fr-send-message` - Streaming response from backend to client
* `cpt-cf-chat-engine-fr-stop-streaming` - Cancel streaming mid-generation
* `cpt-cf-chat-engine-nfr-streaming` - Latency < 10ms overhead, first byte < 200ms
* `cpt-cf-chat-engine-nfr-response-time` - Overall routing latency < 100ms

**Design Elements**:
* `cpt-cf-chat-engine-response-streaming` - Chat Engine's HTTP chunked streaming and backpressure functionality
* `cpt-cf-chat-engine-principle-streaming` - Design principle mandating streaming-first
* `cpt-cf-chat-engine-design-context-backpressure` - Implementation details for flow control

**Related ADRs**:
* ADR-0006 (Webhook Protocol) - HTTP streaming from backends via chunked encoding
* ADR-0007 (HTTP Client Protocol) - HTTP streaming protocol for client communication
* ADR-0009 (Client-Initiated Streaming Cancellation) - Client cancellation mechanism
* ADR-0012 (Streaming Backpressure with Buffer Limits) - Buffer management and flow control strategy
