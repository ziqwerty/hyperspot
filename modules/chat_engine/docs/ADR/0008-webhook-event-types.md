Created:  2026-02-04 by Constructor Tech
Updated:  2026-03-06 by Constructor Tech
# ADR-0008: Webhook Event Schema with Typed Events

**Date**: 2026-02-04

**Status**: accepted

**ID**: `cpt-cf-chat-engine-adr-webhook-event-types`

## Context and Problem Statement

Chat Engine needs to communicate different types of events to webhook backends (session created, new message, message recreated, session deleted, summarization request). How should these events be structured to enable backends to handle different scenarios while maintaining protocol extensibility?

## Decision Drivers

* Clear differentiation between event types (creation vs recreation vs deletion)
* Extensibility for new event types without breaking changes
* Type safety for backend implementations
* Context completeness (backends need full session context)
* Backward compatibility when adding new event types
* Debugging and logging clarity (event type visible in logs)
* Support for different backend responses based on event type

## Considered Options

* **Option 1: Typed events with event field** - JSON payload with "event" field discriminating type
* **Option 2: Separate endpoints per event** - Different URLs for different event types
* **Option 3: Generic events with action hints** - Single event structure with optional action metadata

## Decision Outcome

Chosen option: "Typed events with event field", because it provides clear type discrimination via "event" field, enables single webhook URL handling multiple event types, supports extensibility by adding new event values, maintains protocol simplicity, and allows backends to route internally based on event type.

### Consequences

* Good, because event type explicit in payload (event: "message.new" vs "message.recreate")
* Good, because single webhook URL can handle all event types (simpler configuration)
* Good, because new event types addable without backend changes (unknown events ignored)
* Good, because event schemas can evolve per type (message.new can differ from session.created)
* Good, because debugging clear (event type visible in logs and traces)
* Good, because backend routing straightforward (switch on event field)
* Bad, because backends must handle multiple event types (cannot specialize per endpoint)
* Bad, because event schema validation more complex (discriminated union)
* Bad, because misrouted events not caught at URL routing level

## Related Design Elements

**Actors**:
* `cpt-cf-chat-engine-actor-backend-plugin` - Receives typed events, routes internally
* `cpt-cf-chat-engine-webhook-integration` - Constructs event payloads with correct type

**Requirements**:
* `cpt-cf-chat-engine-fr-create-session` - session.created event
* `cpt-cf-chat-engine-fr-send-message` - message.new event
* `cpt-cf-chat-engine-fr-recreate-response` - message.recreate event
* `cpt-cf-chat-engine-fr-delete-session` - session.deleted event
* `cpt-cf-chat-engine-fr-session-summary` - session.summary event
* `cpt-cf-chat-engine-fr-stop-streaming` - message.aborted event

**Design Elements**:
* Webhook API specification (Section 3.3.2 of DESIGN.md) defines all event schemas
* `cpt-cf-chat-engine-webhook-integration` - Event payload construction

**Related ADRs**:
* ADR-0006 (Webhook Protocol) - HTTP protocol carrying these events
* ADR-0016 (Recreation Creates Variants, Branching Creates Children) - message.recreate event semantics
