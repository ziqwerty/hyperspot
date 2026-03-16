# Static Mini-Chat Audit Plugin

Mini-chat audit plugin that logs audit events via `tracing`. Designed for development, testing, and deployments where a full eventing backend is unnecessary.

## Overview

The `cf-static-mini-chat-audit-plugin` implements `MiniChatAuditPluginClientV1` and emits structured log records for four audit event types:

- **`emit_turn_audit`** — completed chat turn (prompt, response, usage, policy decisions)
- **`emit_turn_retry_audit`** — turn retry (mutation of an existing turn)
- **`emit_turn_edit_audit`** — turn edit (prompt replacement)
- **`emit_turn_delete_audit`** — turn deletion

The plugin registers itself via the types registry as a `MiniChatAuditPluginSpecV1` instance and is discovered by the `mini-chat` module through `ClientHub`.

## Configuration

Add the plugin section under your module configuration:

```yaml
static-mini-chat-audit-plugin:
  config:
    enabled: true   # default: true
```

When `enabled: false`, the plugin still registers in the types registry and `ClientHub`, but all `emit_*` methods return immediately without logging.

## Architecture

```text
module.rs          ModKit module — init, config loading, GTS registration
config.rs          YAML config model (enabled flag)
domain/
  service.rs       Service — holds enabled state
  client.rs        MiniChatAuditPluginClientV1 impl (structured tracing logs)
  mod.rs           Re-exports
```

### Init sequence

1. Load `StaticMiniChatAuditPluginConfig` from module config
2. Create `Service` with the `enabled` flag
3. Register GTS plugin instance in types-registry
4. Register `MiniChatAuditPluginClientV1` scoped client in `ClientHub`

## License

Apache-2.0
