Created:  2026-02-04 by Constructor Tech
Updated:  2026-03-06 by Constructor Tech
# ADR-0013: Per-Session-Type Timeout Configuration

**Date**: 2026-02-04

**Status**: superseded by ADR-0026 — timeout responsibility moved to plugin / webhook-compat plugin

**ID**: `cpt-cf-chat-engine-adr-timeout-configuration`

## Context and Problem Statement

Different webhook backends have different processing times (fast rule-based vs slow backends with complex processing). How should Chat Engine configure request timeouts to balance responsiveness with backend processing needs?

## Decision Drivers

* Different backends need different timeouts (complex-backend: 30s, simple rules: 5s, web search: 60s)
* Prevent indefinite hangs from slow/broken backends
* Enable backend-specific tuning without code changes
* Default timeout for new backends (30s)
* Maximum timeout limit (300s = 5 minutes)
* Per-backend configuration (not global)
* Timeout errors distinguishable from other errors
* Observable timeout metrics per backend

## Considered Options

* **Option 1: Per-session-type timeout in database** - SessionType entity stores timeout field (1-300 seconds)
* **Option 2: Global timeout** - Single timeout for all backends
* **Option 3: Client-specified timeout** - Client sends timeout per request

## Decision Outcome

Chosen option: "Per-session-type timeout in database", because it enables backend-specific tuning based on processing characteristics, provides reasonable default (30s) for new backends, enforces maximum limit (300s) preventing abuse, stores configuration in database for easy updates, and supports different backend types (fast vs slow) appropriately.

### Consequences

* Good, because backends configured independently (fast timeout for rules, slow for complex+search)
* Good, because timeout tuning without code deployment (database update)
* Good, because reasonable default (30s) for new backends
* Good, because maximum enforced (300s prevents indefinite hang)
* Good, because timeout errors distinguishable (BACKEND_TIMEOUT error code)
* Good, because metrics per backend (observable timeout frequency)
* Bad, because configuration complexity (admins must tune per backend)
* Bad, because no dynamic adjustment (fixed timeout per backend)
* Bad, because timeout may abort long-valid processing (no progress tracking)
* Bad, because client cannot override for specific requests

## Related Design Elements

**Actors**:
* `cpt-cf-chat-engine-webhook-integration` - Enforces timeout per backend
* `cpt-cf-chat-engine-actor-developer` - Configures timeout per session type

**Requirements**:
* `cpt-cf-chat-engine-nfr-backend-isolation` - Configurable timeout per session type

**Design Elements**:
* `cpt-cf-chat-engine-entity-session-type` - timeout field (INTEGER, CHECK 1-300)
* `cpt-cf-chat-engine-db-table-session-types` - timeout column with constraints

**Related ADRs**:
* ADR-0006 (Webhook Protocol) - HTTP timeout for backend requests
* ADR-0011 (Circuit Breaker per Webhook Backend) - Timeout failures contribute to circuit breaker state
