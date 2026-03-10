Created:  2026-02-04 by Constructor Tech
Updated:  2026-03-06 by Constructor Tech
# ADR-0019: Token-Based Session Sharing with Branching

**Date**: 2026-02-04

**Status**: accepted

**ID**: `cpt-cf-chat-engine-adr-session-sharing`

## Context and Problem Statement

Users want to share conversations with others for collaboration, review, or assistance. Recipients should view the original conversation (read-only) and optionally create branches. How should Chat Engine enable secure session sharing without exposing session_id or requiring recipient authentication?

## Decision Drivers

* Secure sharing (no session_id exposure)
* Read-only access to original conversation
* Recipients can branch (not modify original)
* Cryptographically secure tokens (not guessable)
* Revocable sharing (owner can revoke access)
* Optional expiration (time-limited sharing)
* Track share token creator (audit trail)
* Multiple tokens per session (share with different groups)

## Considered Options

* **Option 1: Cryptographic share token with separate table** - ShareToken entity maps token to session_id
* **Option 2: Signed session_id JWT** - Encode session_id in JWT, verify signature
* **Option 3: Publicly readable sessions** - Sessions publicly accessible by default

## Decision Outcome

Chosen option: "Cryptographic share token with separate table", because it provides cryptographically secure tokens (min 32 chars random), enables revocation via database flag, supports optional expiration, tracks creator for audit, allows multiple tokens per session, and keeps session_id hidden from recipients.

### Consequences

* Good, because share tokens cryptographically secure (not guessable)
* Good, because revocation instant (database flag, no token re-issue)
* Good, because optional expiration (time-limited sharing)
* Good, because audit trail (created_by, created_at tracking)
* Good, because multiple tokens per session (different recipient groups)
* Good, because session_id hidden (token maps to session internally)
* Good, because recipients branch without owning session
* Bad, because separate table join required (token → session_id lookup)
* Bad, because token generation requires crypto library
* Bad, because no token refresh mechanism (expired = generate new)
* Bad, because share_tokens table grows unbounded (cleanup needed)

## Related Design Elements

**Actors**:
* `cpt-cf-chat-engine-actor-client` - Creates share token, shares URL with recipients
* `cpt-cf-chat-engine-actor-end-user` - Accesses shared session via token
* `cpt-cf-chat-engine-session-management` - Generates tokens, validates access

**Requirements**:
* `cpt-cf-chat-engine-fr-share-session` - Generate token, recipients view and branch
* `cpt-cf-chat-engine-usecase-share-session` - Full use case for sharing

**Design Elements**:
* `cpt-cf-chat-engine-entity-share-token` - Cryptographic token, session mapping, metadata
* `cpt-cf-chat-engine-db-table-share-tokens` - ShareToken table with constraints
* Sequence diagram S10 (Share Session)

**Related ADRs**:
* ADR-0017 (Conversation Branching from Any Historical Message) - Recipients branch from last message
* ADR-0018 (Session Type Switching with Capability Updates) - Branched sessions use original session type
