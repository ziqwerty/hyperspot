use std::sync::Arc;

use futures::StreamExt;
use modkit_security::SecurityContext;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, warn};

use crate::domain::llm::ToolPhase;
use crate::domain::ports::metric_labels::{stage, trigger};
use crate::domain::repos::{MessageRepository, TurnRepository};
use crate::domain::stream_events::{DoneData, ErrorData, StreamEvent};
use crate::infra::db::entity::chat_turn::TurnState;
use crate::infra::llm::{
    ClientSseEvent, LlmMessage, LlmProvider, LlmProviderError, LlmRequestBuilder, LlmTool,
    RequestMetadata, RequestType, TerminalOutcome,
};

use modkit_macros::domain_model;

use super::types::{
    ActiveStreamGuard, FinalizationCtx, PROGRESS_UPDATE_INTERVAL, StreamOutcome, StreamTerminal,
    determine_features, normalize_error,
};

/// Model and provider configuration for a single provider task invocation.
#[domain_model]
pub(super) struct ProviderTaskConfig {
    pub llm: Arc<dyn LlmProvider>,
    pub upstream_alias: String,
    pub messages: Vec<LlmMessage>,
    pub system_instructions: Option<String>,
    pub tools: Vec<LlmTool>,
    pub model: String,
    pub provider_model_id: String,
    pub max_output_tokens: u32,
    pub max_tool_calls: u32,
    pub web_search_max_calls: u32,
    pub code_interpreter_max_calls: u32,
    pub api_params: mini_chat_sdk::ModelApiParams,
    pub provider_file_id_map: std::collections::HashMap<String, crate::domain::llm::AttachmentRef>,
}

/// All five terminal paths (provider done, incomplete, provider error,
/// client disconnect, pre-stream error) route through `finalize_turn_cas()`.
/// SSE terminal events (Done/Error) are emitted only after the CAS winner
/// commits the transaction (D3).
#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::let_underscore_must_use,
    clippy::cast_possible_truncation
)]
pub(super) fn spawn_provider_task<TR: TurnRepository + 'static, MR: MessageRepository + 'static>(
    ctx: SecurityContext,
    config: ProviderTaskConfig,
    cancel: CancellationToken,
    tx: mpsc::Sender<StreamEvent>,
    fin_ctx: Option<FinalizationCtx<TR, MR>>,
) -> tokio::task::JoinHandle<StreamOutcome> {
    let ProviderTaskConfig {
        llm,
        upstream_alias,
        messages,
        system_instructions,
        tools,
        model,
        provider_model_id,
        max_output_tokens,
        max_tool_calls,
        web_search_max_calls,
        code_interpreter_max_calls,
        api_params,
        provider_file_id_map,
    } = config;

    let span = if let Some(ref fctx) = fin_ctx {
        tracing::info_span!(
            "provider_stream",
            chat_id = %fctx.chat_id,
            turn_request_id = %fctx.request_id,
            turn_id = %fctx.turn_id,
            model = %model,
        )
    } else {
        tracing::info_span!("provider_stream", model = %model)
    };

    tokio::spawn(async move {
        let stream_start = std::time::Instant::now();
        let mut first_token_time: Option<std::time::Duration> = None;

        // ── Metrics: stream started + active gauge ──
        // ActiveStreamGuard ensures decrement on every exit path (Drop-based).
        let _stream_guard = if let Some(ref fctx) = fin_ctx {
            fctx.metrics
                .record_stream_started(&fctx.provider_id, &fctx.effective_model);
            fctx.metrics.increment_active_streams();
            Some(ActiveStreamGuard(Arc::clone(&fctx.metrics)))
        } else {
            None
        };

        // Build the LLM request using provider_model_id (the actual provider-facing name)
        let mut builder = LlmRequestBuilder::new(&provider_model_id)
            .messages(messages)
            .max_output_tokens(u64::from(max_output_tokens))
            .max_tool_calls(max_tool_calls);
        if let Some(instructions) = system_instructions {
            builder = builder.system_instructions(instructions);
        }
        let features = determine_features(&tools);
        for tool in tools {
            builder = builder.tool(tool);
        }
        let metadata = RequestMetadata {
            tenant_id: ctx.subject_tenant_id().to_string(),
            user_id: ctx.subject_id().to_string(),
            chat_id: fin_ctx
                .as_ref()
                .map_or_else(String::new, |f| f.chat_id.to_string()),
            request_type: RequestType::Chat,
            features,
        };
        builder = builder.metadata(metadata);

        // Forward model-policy API params (temperature, top_p, etc.) to the
        // provider adapter via the generic `additional_params` escape hatch.
        {
            let mut params = serde_json::json!({
                "temperature": api_params.temperature,
                "top_p": api_params.top_p,
                "frequency_penalty": api_params.frequency_penalty,
                "presence_penalty": api_params.presence_penalty,
            });
            if !api_params.stop.is_empty() {
                params["stop"] = serde_json::json!(api_params.stop);
            }
            if let Some(extra_body) = api_params.extra_body {
                params["extra_body"] = extra_body;
            }
            builder = builder.additional_params(params);
        }

        let request = builder.build_streaming();

        // Call the provider to start streaming
        let stream_result = llm
            .stream(ctx, request, &upstream_alias, cancel.clone())
            .await;

        let mut provider_stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                // Provider failed before any events — finalize first, then emit error.
                warn!(
                    error = %e,
                    raw_detail = e.raw_detail().unwrap_or(""),
                    "LLM provider failed before stream start"
                );
                let (code, message) = normalize_error(&e);

                if let Some(ref fctx) = fin_ctx {
                    let input = fctx.to_finalization_input(
                        TurnState::Failed,
                        "",
                        None,
                        Some(code.clone()),
                        None,
                        None,
                        0,
                        0,
                        None,
                        None,
                    );
                    match fctx.finalization_svc.finalize_turn_cas(input).await {
                        Ok(outcome) if outcome.won_cas => {
                            let _ = tx
                                .send(StreamEvent::Error(ErrorData {
                                    code: code.clone(),
                                    message,
                                }))
                                .await;
                        }
                        Ok(_) => { /* CAS loser — no SSE emission */ }
                        Err(fe) => {
                            warn!(error = %fe, "finalization failed on pre-stream error");
                            // Still emit error so client isn't left hanging
                            let _ = tx
                                .send(StreamEvent::Error(ErrorData {
                                    code: code.clone(),
                                    message,
                                }))
                                .await;
                        }
                    }
                } else {
                    let _ = tx
                        .send(StreamEvent::Error(ErrorData {
                            code: code.clone(),
                            message,
                        }))
                        .await;
                }

                // Metrics: pre-stream failure
                if let Some(ref fctx) = fin_ctx {
                    let ms = stream_start.elapsed().as_secs_f64() * 1000.0;
                    fctx.metrics.record_stream_failed(&fctx.provider_id, &fctx.effective_model, &code);
                    fctx.metrics.record_stream_total_latency_ms(&fctx.provider_id, &fctx.effective_model, ms);
                }

                return StreamOutcome {
                    terminal: StreamTerminal::Failed,
                    accumulated_text: String::new(),
                    usage: None,
                    effective_model: model,
                    error_code: Some(code),
                    provider_response_id: None,
                    provider_partial_usage: false,
                };
            }
        };

        // Read events from provider, translate and forward through channel
        let mut accumulated_text = String::new();
        let mut cancelled = false;
        let mut last_progress_update = std::time::Instant::now();
        let mut web_search_call_count: u32 = 0;
        // TODO(P2): web_search_call_count (Start) is used for enforcement,
        // web_search_completed_count (Done) is used for settlement. If a search
        // starts but never completes (provider error between Start/Done), the
        // daily quota under-counts by one. Acceptable for P1 since OpenAI always
        // pairs searching→completed; revisit if we add providers that don't.
        let mut web_search_completed_count: u32 = 0;
        let mut code_interpreter_call_count: u32 = 0;
        let mut code_interpreter_completed_count: u32 = 0;

        loop {
            tokio::select! {
                biased;

                () = cancel.cancelled() => {
                    debug!("stream cancelled, aborting provider");
                    if let Some(ref fctx) = fin_ctx {
                        fctx.metrics.record_cancel_requested(trigger::DISCONNECT);
                        let disconnect_stage = if first_token_time.is_none() {
                            stage::BEFORE_FIRST_TOKEN
                        } else {
                            stage::MID_STREAM
                        };
                        fctx.metrics.record_stream_disconnected(disconnect_stage);
                    }
                    provider_stream.cancel();
                    cancelled = true;
                    break;
                }

                event = provider_stream.next() => {
                    match event {
                        Some(Ok(client_event)) => {
                            let is_first_token = matches!(client_event, ClientSseEvent::Delta { .. })
                                && first_token_time.is_none();

                            if let ClientSseEvent::Delta { r#type, ref content } = client_event {
                                if first_token_time.is_none() {
                                    let ttft = stream_start.elapsed();
                                    first_token_time = Some(ttft);
                                    info!(
                                        time_to_first_token_ms = ttft.as_millis() as u64,
                                        "first token received"
                                    );
                                    if let Some(ref fctx) = fin_ctx {
                                        let ms = ttft.as_secs_f64() * 1000.0;
                                        fctx.metrics.record_ttft_provider_ms(&fctx.provider_id, &fctx.effective_model, ms);
                                    }
                                }
                                // Only accumulate visible text for DB storage;
                                // reasoning deltas are streamed to the client
                                // but excluded from the persisted content.
                                if r#type == "text" {
                                    accumulated_text.push_str(content);
                                }

                                // Throttled progress timestamp update for orphan detection.
                                // Timer resets only on success — retry sooner on transient
                                // failures to avoid stale last_progress_at triggering false
                                // orphan detection.
                                if let Some(ref fctx) = fin_ctx
                                    && last_progress_update.elapsed() >= PROGRESS_UPDATE_INTERVAL
                                {
                                    let ok = match fctx.db.conn() {
                                        Ok(conn) => {
                                            match fctx.turn_repo.update_progress_at(&conn, &fctx.scope, fctx.turn_id).await {
                                                Ok(_) => true,
                                                Err(e) => {
                                                    warn!(turn_id = %fctx.turn_id, error = %e, "failed to update progress timestamp");
                                                    false
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            warn!(turn_id = %fctx.turn_id, error = %e, "failed to get DB connection for progress update");
                                            false
                                        }
                                    };
                                    if ok {
                                        last_progress_update = std::time::Instant::now();
                                    }
                                }
                            }

                            // Track web search tool calls for per-message limit
                            if let ClientSseEvent::Tool { ref phase, name, .. } = client_event
                                && name == "web_search"
                            {
                                match phase {
                                    ToolPhase::Start => {
                                        web_search_call_count += 1;
                                        if web_search_call_count > web_search_max_calls {
                                            warn!(
                                                web_search_call_count,
                                                limit = web_search_max_calls,
                                                "web search per-message limit exceeded"
                                            );
                                            let code = "web_search_calls_exceeded".to_owned();
                                            let message = "Web search calls exceeded for this message".to_owned();

                                            // Finalize as failed, then emit error (D3)
                                            if let Some(ref fctx) = fin_ctx {
                                                let input = fctx.to_finalization_input(
                                                    TurnState::Failed,
                                                    &accumulated_text,
                                                    None,
                                                    Some(code.clone()),
                                                    None,
                                                    None,
                                                    web_search_completed_count,
                                                    code_interpreter_completed_count,
                                                    None,
                                                    None,
                                                );
                                                match fctx.finalization_svc.finalize_turn_cas(input).await {
                                                    Ok(outcome) if outcome.won_cas => {
                                                        let _ = tx.send(StreamEvent::Error(ErrorData {
                                                            code: code.clone(),
                                                            message,
                                                        })).await;
                                                    }
                                                    Ok(_) => {}
                                                    Err(fe) => {
                                                        warn!(error = %fe, "finalization failed on ws limit exceeded");
                                                        let _ = tx.send(StreamEvent::Error(ErrorData {
                                                            code: code.clone(),
                                                            message,
                                                        })).await;
                                                    }
                                                }
                                            } else {
                                                let _ = tx.send(StreamEvent::Error(ErrorData {
                                                    code: code.clone(),
                                                    message,
                                                })).await;
                                            }

                                            provider_stream.cancel();

                                            // Metrics: web search limit exceeded
                                            if let Some(ref fctx) = fin_ctx {
                                                let ms = stream_start.elapsed().as_secs_f64() * 1000.0;
                                                fctx.metrics.record_stream_failed(
                                                    &fctx.provider_id,
                                                    &fctx.effective_model,
                                                    &code,
                                                );
                                                fctx.metrics.record_stream_total_latency_ms(
                                                    &fctx.provider_id,
                                                    &fctx.effective_model,
                                                    ms,
                                                );
                                            }

                                            let has_partial = !accumulated_text.is_empty();
                                            return StreamOutcome {
                                                terminal: StreamTerminal::Failed,
                                                accumulated_text,
                                                usage: None,
                                                effective_model: model,
                                                error_code: Some(code),
                                                provider_response_id: None,
                                                provider_partial_usage: has_partial,
                                            };
                                        }
                                    }
                                    ToolPhase::Done => {
                                        web_search_completed_count += 1;
                                    }
                                }
                            }

                            // Track code interpreter tool calls
                            if let ClientSseEvent::Tool { ref phase, name, .. } = client_event
                                && name == "code_interpreter"
                            {
                                match phase {
                                    ToolPhase::Start => {
                                        code_interpreter_call_count += 1;
                                        if code_interpreter_call_count > code_interpreter_max_calls {
                                            warn!(
                                                code_interpreter_call_count,
                                                limit = code_interpreter_max_calls,
                                                "code interpreter per-message limit exceeded"
                                            );
                                            let code = "code_interpreter_calls_exceeded".to_owned();
                                            let message = "Code interpreter calls exceeded for this message".to_owned();

                                            if let Some(ref fctx) = fin_ctx {
                                                let input = fctx.to_finalization_input(
                                                    TurnState::Failed,
                                                    &accumulated_text,
                                                    None,
                                                    Some(code.clone()),
                                                    None,
                                                    None,
                                                    web_search_completed_count,
                                                    code_interpreter_completed_count,
                                                    None,
                                                    None,
                                                );
                                                match fctx.finalization_svc.finalize_turn_cas(input).await {
                                                    Ok(outcome) if outcome.won_cas => {
                                                        let _ = tx.send(StreamEvent::Error(ErrorData {
                                                            code: code.clone(),
                                                            message,
                                                        })).await;
                                                    }
                                                    Ok(_) => {}
                                                    Err(fe) => {
                                                        warn!(error = %fe, "finalization failed on ci limit exceeded");
                                                        let _ = tx.send(StreamEvent::Error(ErrorData {
                                                            code: code.clone(),
                                                            message,
                                                        })).await;
                                                    }
                                                }
                                            } else {
                                                let _ = tx.send(StreamEvent::Error(ErrorData {
                                                    code: code.clone(),
                                                    message,
                                                })).await;
                                            }

                                            provider_stream.cancel();

                                            if let Some(ref fctx) = fin_ctx {
                                                let ms = stream_start.elapsed().as_secs_f64() * 1000.0;
                                                fctx.metrics.record_stream_failed(
                                                    &fctx.provider_id,
                                                    &fctx.effective_model,
                                                    &code,
                                                );
                                                fctx.metrics.record_stream_total_latency_ms(
                                                    &fctx.provider_id,
                                                    &fctx.effective_model,
                                                    ms,
                                                );
                                            }

                                            let has_partial = !accumulated_text.is_empty();
                                            return StreamOutcome {
                                                terminal: StreamTerminal::Failed,
                                                accumulated_text,
                                                usage: None,
                                                effective_model: model,
                                                error_code: Some(code),
                                                provider_response_id: None,
                                                provider_partial_usage: has_partial,
                                            };
                                        }
                                    }
                                    ToolPhase::Done => {
                                        code_interpreter_completed_count += 1;
                                    }
                                }
                            }

                            let stream_event = StreamEvent::from(client_event);
                            if tx.send(stream_event).await.is_err() {
                                // Receiver dropped (client disconnect handled by relay)
                                info!("channel closed (client disconnect), exiting provider task");
                                break;
                            }

                            // TTFT overhead: time from provider first-byte to channel send.
                            if is_first_token
                                && let (Some(fctx), Some(provider_ttft)) =
                                    (&fin_ctx, first_token_time)
                                {
                                    let total = stream_start.elapsed().as_secs_f64() * 1000.0;
                                    let provider_ms = provider_ttft.as_secs_f64() * 1000.0;
                                    fctx.metrics.record_ttft_overhead_ms(
                                        &fctx.provider_id,
                                        &fctx.effective_model,
                                        total - provider_ms,
                                    );
                                }
                        }
                        Some(Err(e)) => {
                            warn!(error = %e, "provider stream error");
                            let (code, message) =
                                normalize_error(&LlmProviderError::StreamError(e));

                            // Finalize first, emit error only if CAS winner (D3)
                            if let Some(ref fctx) = fin_ctx {
                                let mid_elapsed = stream_start.elapsed();
                                let input = fctx.to_finalization_input(
                                    TurnState::Failed,
                                    &accumulated_text,
                                    None,
                                    Some(code.clone()),
                                    None,
                                    None,
                                    web_search_completed_count,
                                    code_interpreter_completed_count,
                                    first_token_time.map(|d| d.as_millis() as u64),
                                    Some(mid_elapsed.as_millis() as u64),
                                );
                                match fctx.finalization_svc.finalize_turn_cas(input).await {
                                    Ok(outcome) if outcome.won_cas => {
                                        let _ = tx
                                            .send(StreamEvent::Error(ErrorData {
                                                code: code.clone(),
                                                message,
                                            }))
                                            .await;
                                    }
                                    Ok(_) => {}
                                    Err(fe) => {
                                        warn!(error = %fe, "finalization failed on stream error");
                                        let _ = tx
                                            .send(StreamEvent::Error(ErrorData {
                                                code: code.clone(),
                                                message,
                                            }))
                                            .await;
                                    }
                                }
                            } else {
                                let _ = tx
                                    .send(StreamEvent::Error(ErrorData {
                                        code: code.clone(),
                                        message,
                                    }))
                                    .await;
                            }

                            // Metrics: mid-stream failure
                            if let Some(ref fctx) = fin_ctx {
                                let ms = stream_start.elapsed().as_secs_f64() * 1000.0;
                                fctx.metrics.record_stream_failed(&fctx.provider_id, &fctx.effective_model, &code);
                                fctx.metrics.record_stream_total_latency_ms(&fctx.provider_id, &fctx.effective_model, ms);
                            }

                            provider_stream.cancel();
                            let has_partial = !accumulated_text.is_empty();
                            return StreamOutcome {
                                terminal: StreamTerminal::Failed,
                                accumulated_text,
                                usage: None,
                                effective_model: model,
                                error_code: Some(code),
                                provider_response_id: None,
                                provider_partial_usage: has_partial,
                            };
                        }
                        None => {
                            // Stream ended — terminal captured by ProviderStream
                            break;
                        }
                    }
                }
            }
        }

        if cancelled {
            let elapsed = stream_start.elapsed();
            info!(
                terminal = "cancelled",
                duration_ms = elapsed.as_millis() as u64,
                "stream cancelled"
            );

            // Finalize cancelled turn — no SSE emission (stream already disconnected) (D3)
            if let Some(ref fctx) = fin_ctx {
                let input = fctx.to_finalization_input(
                    TurnState::Cancelled,
                    &accumulated_text,
                    None,
                    None,
                    None,
                    None,
                    web_search_completed_count,
                    code_interpreter_completed_count,
                    first_token_time.map(|d| d.as_millis() as u64),
                    Some(elapsed.as_millis() as u64),
                );
                if let Err(e) = fctx.finalization_svc.finalize_turn_cas(input).await {
                    warn!(error = %e, "finalization failed on cancelled stream");
                }

                // Metrics: cancelled stream
                let ms = elapsed.as_secs_f64() * 1000.0;
                fctx.metrics.record_cancel_effective(trigger::DISCONNECT);
                fctx.metrics.record_time_to_abort_ms(trigger::DISCONNECT, ms);
                fctx.metrics.record_stream_total_latency_ms(&fctx.provider_id, &fctx.effective_model, ms);
            }

            return StreamOutcome {
                terminal: StreamTerminal::Cancelled,
                accumulated_text,
                usage: None,
                effective_model: model,
                error_code: None,
                provider_response_id: None,
                provider_partial_usage: false,
            };
        }

        // Extract the terminal outcome from the provider stream
        let terminal = provider_stream.into_outcome().await;

        match terminal {
            TerminalOutcome::Completed {
                usage,
                content: _,
                citations,
                response_id,
                ..
            } => {
                let elapsed = stream_start.elapsed();
                info!(
                    terminal = "completed",
                    input_tokens = usage.input_tokens,
                    output_tokens = usage.output_tokens,
                    duration_ms = elapsed.as_millis() as u64,
                    "stream completed"
                );

                // Finalize first, then emit Done only if CAS winner (D3)
                if let Some(ref fctx) = fin_ctx {
                    let input = fctx.to_finalization_input(
                        TurnState::Completed,
                        &accumulated_text,
                        Some(usage),
                        None,
                        None,
                        Some(response_id.clone()),
                        web_search_completed_count,
                        code_interpreter_completed_count,
                        first_token_time.map(|d| d.as_millis() as u64),
                        Some(elapsed.as_millis() as u64),
                    );
                    match fctx.finalization_svc.finalize_turn_cas(input).await {
                        Ok(outcome) if outcome.won_cas => {
                            // P4-2: Map provider file_ids to internal UUIDs
                            let mapped = crate::domain::citation_mapping::map_citation_ids(
                                citations,
                                &provider_file_id_map,
                            );
                            if !mapped.is_empty() {
                                let _ = tx
                                    .send(StreamEvent::Citations(
                                        crate::domain::stream_events::CitationsData {
                                            items: mapped,
                                        },
                                    ))
                                    .await;
                            }
                            // Compute quota warnings post-commit (advisory, best-effort)
                            let quota_warnings = match fctx
                                .quota_warnings_provider
                                .get_quota_warnings(&fctx.scope, fctx.tenant_id, fctx.user_id)
                                .await
                            {
                                Ok(w) => Some(w),
                                Err(e) => {
                                    warn!(error = %e, "failed to compute quota_warnings");
                                    None
                                }
                            };
                            let _ = tx
                                .send(StreamEvent::Done(Box::new(DoneData {
                                    usage: Some(usage),
                                    effective_model: fctx.effective_model.clone(),
                                    selected_model: fctx.selected_model.clone(),
                                    quota_decision: fctx.quota_decision.clone(),
                                    downgrade_from: fctx.downgrade_from.clone(),
                                    downgrade_reason: fctx.downgrade_reason.clone(),
                                    quota_warnings,
                                })))
                                .await;
                        }
                        Ok(_) => { /* CAS loser — no SSE emission */ }
                        Err(fe) => {
                            warn!(error = %fe, "finalization failed on completed stream");
                            // Emit Done anyway so client isn't left hanging
                            let _ = tx
                                .send(StreamEvent::Done(Box::new(DoneData {
                                    usage: Some(usage),
                                    effective_model: fctx.effective_model.clone(),
                                    selected_model: fctx.selected_model.clone(),
                                    quota_decision: "allow".into(),
                                    downgrade_from: None,
                                    downgrade_reason: None,
                                    quota_warnings: None,
                                })))
                                .await;
                        }
                    }
                } else {
                    // No finalization context (unit tests) — emit directly
                    let mapped = crate::domain::citation_mapping::map_citation_ids(
                        citations,
                        &provider_file_id_map,
                    );
                    if !mapped.is_empty() {
                        let _ = tx
                            .send(StreamEvent::Citations(
                                crate::domain::stream_events::CitationsData { items: mapped },
                            ))
                            .await;
                    }
                    let _ = tx
                        .send(StreamEvent::Done(Box::new(DoneData {
                            usage: Some(usage),
                            effective_model: model.clone(),
                            selected_model: model.clone(),
                            quota_decision: "allow".into(),
                            downgrade_from: None,
                            downgrade_reason: None,
                            quota_warnings: None,
                        })))
                        .await;
                }

                // Metrics: completed stream
                if let Some(ref fctx) = fin_ctx {
                    let ms = stream_start.elapsed().as_secs_f64() * 1000.0;
                    fctx.metrics.record_stream_completed(&fctx.provider_id, &fctx.effective_model);
                    fctx.metrics.record_stream_total_latency_ms(&fctx.provider_id, &fctx.effective_model, ms);
                }

                StreamOutcome {
                    terminal: StreamTerminal::Completed,
                    accumulated_text,
                    usage: Some(usage),
                    effective_model: model,
                    error_code: None,
                    provider_response_id: Some(response_id),
                    provider_partial_usage: false,
                }
            }
            TerminalOutcome::Incomplete { usage, reason, .. } => {
                let elapsed = stream_start.elapsed();
                warn!(
                    terminal = "incomplete",
                    reason = %reason,
                    duration_ms = elapsed.as_millis() as u64,
                    "stream incomplete"
                );

                // Incomplete maps to Completed in DB — provider finished but hit
                // max_output_tokens. From billing/persistence perspective this is
                // a completed turn with truncated content (see design D10).
                if let Some(ref fctx) = fin_ctx {
                    let input = fctx.to_finalization_input(
                        TurnState::Completed,
                        &accumulated_text,
                        Some(usage),
                        None,
                        None,
                        None,
                        web_search_completed_count,
                        code_interpreter_completed_count,
                        first_token_time.map(|d| d.as_millis() as u64),
                        Some(elapsed.as_millis() as u64),
                    );
                    match fctx.finalization_svc.finalize_turn_cas(input).await {
                        Ok(outcome) if outcome.won_cas => {
                            let quota_warnings = match fctx
                                .quota_warnings_provider
                                .get_quota_warnings(&fctx.scope, fctx.tenant_id, fctx.user_id)
                                .await
                            {
                                Ok(w) => Some(w),
                                Err(e) => {
                                    warn!(error = %e, "failed to compute quota_warnings");
                                    None
                                }
                            };
                            let _ = tx
                                .send(StreamEvent::Done(Box::new(DoneData {
                                    usage: Some(usage),
                                    effective_model: fctx.effective_model.clone(),
                                    selected_model: fctx.selected_model.clone(),
                                    quota_decision: fctx.quota_decision.clone(),
                                    downgrade_from: fctx.downgrade_from.clone(),
                                    downgrade_reason: fctx.downgrade_reason.clone(),
                                    quota_warnings,
                                })))
                                .await;
                        }
                        Ok(_) => {}
                        Err(fe) => {
                            warn!(error = %fe, "finalization failed on incomplete stream");
                            let _ = tx
                                .send(StreamEvent::Done(Box::new(DoneData {
                                    usage: Some(usage),
                                    effective_model: fctx.effective_model.clone(),
                                    selected_model: fctx.selected_model.clone(),
                                    quota_decision: "allow".into(),
                                    downgrade_from: None,
                                    downgrade_reason: None,
                                    quota_warnings: None,
                                })))
                                .await;
                        }
                    }
                } else {
                    let _ = tx
                        .send(StreamEvent::Done(Box::new(DoneData {
                            usage: Some(usage),
                            effective_model: model.clone(),
                            selected_model: model.clone(),
                            quota_decision: "allow".into(),
                            downgrade_from: None,
                            downgrade_reason: None,
                            quota_warnings: None,
                        })))
                        .await;
                }

                // Metrics: incomplete stream
                if let Some(ref fctx) = fin_ctx {
                    let ms = stream_start.elapsed().as_secs_f64() * 1000.0;
                    fctx.metrics.record_stream_incomplete(&fctx.provider_id, &fctx.effective_model, &reason);
                    fctx.metrics.record_stream_completed(&fctx.provider_id, &fctx.effective_model);
                    fctx.metrics.record_stream_total_latency_ms(&fctx.provider_id, &fctx.effective_model, ms);
                }

                StreamOutcome {
                    terminal: StreamTerminal::Incomplete,
                    accumulated_text,
                    usage: Some(usage),
                    effective_model: model,
                    error_code: Some(format!("incomplete:{reason}")),
                    provider_response_id: None,
                    provider_partial_usage: false,
                }
            }
            TerminalOutcome::Failed { error, usage, .. } => {
                let raw_detail = error.raw_detail().map(ToOwned::to_owned);
                let (code, message) = normalize_error(&error);
                let elapsed = stream_start.elapsed();
                warn!(
                    terminal = "failed",
                    error_code = %code,
                    raw_detail = raw_detail.as_deref().unwrap_or(""),
                    duration_ms = elapsed.as_millis() as u64,
                    "stream failed"
                );

                // Finalize first, emit error only if CAS winner (D3)
                if let Some(ref fctx) = fin_ctx {
                    let input = fctx.to_finalization_input(
                        TurnState::Failed,
                        &accumulated_text,
                        usage,
                        Some(code.clone()),
                        None,
                        None,
                        web_search_completed_count,
                        code_interpreter_completed_count,
                        first_token_time.map(|d| d.as_millis() as u64),
                        Some(elapsed.as_millis() as u64),
                    );
                    match fctx.finalization_svc.finalize_turn_cas(input).await {
                        Ok(outcome) if outcome.won_cas => {
                            let _ = tx
                                .send(StreamEvent::Error(ErrorData {
                                    code: code.clone(),
                                    message,
                                }))
                                .await;
                        }
                        Ok(_) => {}
                        Err(fe) => {
                            warn!(error = %fe, "finalization failed on failed stream");
                            let _ = tx
                                .send(StreamEvent::Error(ErrorData {
                                    code: code.clone(),
                                    message,
                                }))
                                .await;
                        }
                    }
                } else {
                    let _ = tx
                        .send(StreamEvent::Error(ErrorData {
                            code: code.clone(),
                            message,
                        }))
                        .await;
                }

                // Metrics: failed stream (post-provider)
                if let Some(ref fctx) = fin_ctx {
                    let ms = stream_start.elapsed().as_secs_f64() * 1000.0;
                    fctx.metrics.record_stream_failed(&fctx.provider_id, &fctx.effective_model, &code);
                    fctx.metrics.record_stream_total_latency_ms(&fctx.provider_id, &fctx.effective_model, ms);
                }

                StreamOutcome {
                    terminal: StreamTerminal::Failed,
                    accumulated_text,
                    usage,
                    effective_model: model,
                    error_code: Some(code),
                    provider_response_id: None,
                    provider_partial_usage: usage.is_some(),
                }
            }
        }
    }.instrument(span))
}
