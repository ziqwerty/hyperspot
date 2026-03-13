use std::sync::Arc;

use modkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::repos::{ChatRepository, TurnRepository};
use crate::domain::service::TurnService;
use crate::domain::service::test_helpers::*;
use crate::domain::service::turn_service::MutationError;
use crate::infra::db::entity::chat_turn::TurnState;
use crate::infra::db::repo;

// ════════════════════════════════════════════════════════════════════════════
// Helpers
// ════════════════════════════════════════════════════════════════════════════

async fn setup() -> (
    TurnService<
        repo::turn_repo::TurnRepository,
        repo::message_repo::MessageRepository,
        repo::chat_repo::ChatRepository,
        repo::message_attachment_repo::MessageAttachmentRepository,
    >,
    modkit_security::SecurityContext,
    Uuid, // chat_id
    Uuid, // tenant_id
) {
    let db = inmem_db().await;
    let db = mock_db_provider(db);
    let tenant_id = Uuid::new_v4();
    let ctx = test_security_ctx(tenant_id);

    let chat_repo = Arc::new(repo::chat_repo::ChatRepository::new(
        modkit_db::odata::LimitCfg {
            default: 20,
            max: 100,
        },
    ));
    let turn_repo = Arc::new(repo::turn_repo::TurnRepository);
    let message_repo = Arc::new(repo::message_repo::MessageRepository::new(
        modkit_db::odata::LimitCfg {
            default: 20,
            max: 100,
        },
    ));

    // Create a chat first
    let chat_id = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant_id);
    let conn = db.conn().unwrap();
    chat_repo
        .create(
            &conn,
            &scope,
            crate::domain::models::Chat {
                id: chat_id,
                tenant_id,
                user_id: ctx.subject_id(),
                model: "gpt-5.2".to_owned(),
                title: Some("Test chat".to_owned()),
                is_temporary: false,
                created_at: time::OffsetDateTime::now_utc(),
                updated_at: time::OffsetDateTime::now_utc(),
            },
        )
        .await
        .unwrap();

    let svc = TurnService::new(
        Arc::clone(&db),
        turn_repo,
        message_repo,
        chat_repo,
        Arc::new(crate::infra::db::repo::message_attachment_repo::MessageAttachmentRepository),
        mock_enforcer(),
    );

    (svc, ctx, chat_id, tenant_id)
}

/// Create a completed turn with a user message. Returns the `request_id`.
async fn create_completed_turn(
    db: &crate::domain::service::DbProvider,
    turn_repo: &impl TurnRepository,
    message_repo: &impl crate::domain::repos::MessageRepository,
    tenant_id: Uuid,
    chat_id: Uuid,
    user_id: Uuid,
) -> Uuid {
    let request_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant_id);
    let conn = db.conn().unwrap();

    // Create turn
    turn_repo
        .create_turn(
            &conn,
            &scope,
            crate::domain::repos::CreateTurnParams {
                id: turn_id,
                tenant_id,
                chat_id,
                request_id,
                requester_type: "user".to_owned(),
                requester_user_id: Some(user_id),
                reserve_tokens: None,
                max_output_tokens_applied: None,
                reserved_credits_micro: None,
                policy_version_applied: None,
                effective_model: Some("gpt-5.2".to_owned()),
                minimal_generation_floor_applied: None,
            },
        )
        .await
        .unwrap();

    // Create user message
    message_repo
        .insert_user_message(
            &conn,
            &scope,
            crate::domain::repos::InsertUserMessageParams {
                id: Uuid::new_v4(),
                tenant_id,
                chat_id,
                request_id,
                content: "Hello world".to_owned(),
            },
        )
        .await
        .unwrap();

    // Create assistant message (required by FK on assistant_message_id)
    let assistant_msg_id = Uuid::new_v4();
    message_repo
        .insert_assistant_message(
            &conn,
            &scope,
            crate::domain::repos::InsertAssistantMessageParams {
                id: assistant_msg_id,
                tenant_id,
                chat_id,
                request_id,
                content: "Assistant reply".to_owned(),
                input_tokens: Some(10),
                output_tokens: Some(5),
                model: Some("gpt-5.2".to_owned()),
                provider_response_id: None,
            },
        )
        .await
        .unwrap();

    // Transition to completed
    turn_repo
        .cas_update_state(
            &conn,
            &scope,
            crate::domain::repos::CasTerminalParams {
                turn_id,
                state: TurnState::Completed,
                error_code: None,
                error_detail: None,
                assistant_message_id: Some(assistant_msg_id),
                provider_response_id: None,
            },
        )
        .await
        .unwrap();

    request_id
}

// ════════════════════════════════════════════════════════════════════════════
// TurnService::get
// ════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn get_returns_completed_turn() {
    let (svc, ctx, chat_id, tenant_id) = setup().await;

    let request_id = create_completed_turn(
        &svc.db,
        &*svc.turn_repo,
        &*svc.message_repo,
        tenant_id,
        chat_id,
        ctx.subject_id(),
    )
    .await;

    let turn = svc.get(&ctx, chat_id, request_id).await.unwrap();
    assert_eq!(turn.request_id, request_id);
    assert_eq!(turn.chat_id, chat_id);
    assert_eq!(turn.state, TurnState::Completed);
    assert!(turn.assistant_message_id.is_some());
}

#[tokio::test]
async fn get_returns_running_turn() {
    let (svc, ctx, chat_id, tenant_id) = setup().await;

    let request_id = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant_id);
    let conn = svc.db.conn().unwrap();
    svc.turn_repo
        .create_turn(
            &conn,
            &scope,
            crate::domain::repos::CreateTurnParams {
                id: Uuid::new_v4(),
                tenant_id,
                chat_id,
                request_id,
                requester_type: "user".to_owned(),
                requester_user_id: Some(ctx.subject_id()),
                reserve_tokens: None,
                max_output_tokens_applied: None,
                reserved_credits_micro: None,
                policy_version_applied: None,
                effective_model: None,
                minimal_generation_floor_applied: None,
            },
        )
        .await
        .unwrap();

    let turn = svc.get(&ctx, chat_id, request_id).await.unwrap();
    assert_eq!(turn.request_id, request_id);
    assert_eq!(turn.state, TurnState::Running);
}

#[tokio::test]
async fn get_nonexistent_turn_returns_not_found() {
    let (svc, ctx, chat_id, _) = setup().await;

    let err = svc.get(&ctx, chat_id, Uuid::new_v4()).await.unwrap_err();
    assert!(
        matches!(err, MutationError::TurnNotFound { .. }),
        "expected TurnNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn get_nonexistent_chat_returns_chat_not_found() {
    let (svc, ctx, _, _) = setup().await;

    let err = svc
        .get(&ctx, Uuid::new_v4(), Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(
        matches!(err, MutationError::ChatNotFound { .. }),
        "expected ChatNotFound, got: {err:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 7.4: validate_mutation — 5 checks in order
// ════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn delete_nonexistent_turn_returns_turn_not_found() {
    let (svc, ctx, chat_id, _) = setup().await;
    let fake_rid = Uuid::new_v4();

    let err = svc.delete(&ctx, chat_id, fake_rid).await.unwrap_err();
    assert!(
        matches!(err, MutationError::TurnNotFound { .. }),
        "expected TurnNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn delete_nonexistent_chat_returns_chat_not_found() {
    let (svc, ctx, _, _) = setup().await;
    let fake_chat = Uuid::new_v4();
    let fake_rid = Uuid::new_v4();

    let err = svc.delete(&ctx, fake_chat, fake_rid).await.unwrap_err();
    assert!(
        matches!(err, MutationError::ChatNotFound { .. }),
        "expected ChatNotFound, got: {err:?}"
    );
}

#[tokio::test]
async fn delete_wrong_owner_returns_forbidden() {
    let (svc, ctx, chat_id, tenant_id) = setup().await;

    // Create a turn with a DIFFERENT requester_user_id than ctx.subject_id()
    let other_user_id = Uuid::new_v4();
    let request_id = create_completed_turn(
        &svc.db,
        &*svc.turn_repo,
        &*svc.message_repo,
        tenant_id,
        chat_id,
        other_user_id,
    )
    .await;

    // ctx.subject_id() != other_user_id → ownership check fails
    let err = svc.delete(&ctx, chat_id, request_id).await.unwrap_err();
    assert!(
        matches!(err, MutationError::Forbidden),
        "expected Forbidden, got: {err:?}"
    );
}

#[tokio::test]
async fn delete_running_turn_returns_invalid_turn_state() {
    let (svc, ctx, chat_id, tenant_id) = setup().await;

    // Create a running turn (don't transition to completed)
    let request_id = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant_id);
    let conn = svc.db.conn().unwrap();
    svc.turn_repo
        .create_turn(
            &conn,
            &scope,
            crate::domain::repos::CreateTurnParams {
                id: Uuid::new_v4(),
                tenant_id,
                chat_id,
                request_id,
                requester_type: "user".to_owned(),
                requester_user_id: Some(ctx.subject_id()),
                reserve_tokens: None,
                max_output_tokens_applied: None,
                reserved_credits_micro: None,
                policy_version_applied: None,
                effective_model: None,
                minimal_generation_floor_applied: None,
            },
        )
        .await
        .unwrap();

    let err = svc.delete(&ctx, chat_id, request_id).await.unwrap_err();
    assert!(
        matches!(
            err,
            MutationError::InvalidTurnState {
                state: TurnState::Running
            }
        ),
        "expected InvalidTurnState(Running), got: {err:?}"
    );
}

#[tokio::test]
async fn delete_non_latest_turn_returns_not_latest() {
    let (svc, ctx, chat_id, tenant_id) = setup().await;

    // Create two completed turns
    let rid1 = create_completed_turn(
        &svc.db,
        &*svc.turn_repo,
        &*svc.message_repo,
        tenant_id,
        chat_id,
        ctx.subject_id(),
    )
    .await;
    // Small delay to ensure different started_at
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let _rid2 = create_completed_turn(
        &svc.db,
        &*svc.turn_repo,
        &*svc.message_repo,
        tenant_id,
        chat_id,
        ctx.subject_id(),
    )
    .await;

    // Try to delete the FIRST turn (not the latest)
    let err = svc.delete(&ctx, chat_id, rid1).await.unwrap_err();
    assert!(
        matches!(err, MutationError::NotLatestTurn),
        "expected NotLatestTurn, got: {err:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 7.1: TurnService::delete — success + edge cases
// ════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn delete_success_soft_deletes_turn() {
    let (svc, ctx, chat_id, tenant_id) = setup().await;

    let request_id = create_completed_turn(
        &svc.db,
        &*svc.turn_repo,
        &*svc.message_repo,
        tenant_id,
        chat_id,
        ctx.subject_id(),
    )
    .await;

    svc.delete(&ctx, chat_id, request_id).await.unwrap();

    // Verify the turn is soft-deleted (deleted_at != NULL)
    let scope = AccessScope::for_tenant(tenant_id);
    let conn = svc.db.conn().unwrap();
    let turn = svc
        .turn_repo
        .find_by_chat_and_request_id(&conn, &scope, chat_id, request_id)
        .await
        .unwrap()
        .unwrap();
    assert!(turn.deleted_at.is_some());
    assert!(turn.replaced_by_request_id.is_none());
}

#[tokio::test]
async fn delete_already_deleted_turn_returns_not_latest() {
    let (svc, ctx, chat_id, tenant_id) = setup().await;

    let request_id = create_completed_turn(
        &svc.db,
        &*svc.turn_repo,
        &*svc.message_repo,
        tenant_id,
        chat_id,
        ctx.subject_id(),
    )
    .await;

    // Delete once
    svc.delete(&ctx, chat_id, request_id).await.unwrap();

    // Try to delete again — should fail (soft-deleted turns excluded from latest check)
    let err = svc.delete(&ctx, chat_id, request_id).await.unwrap_err();
    // After soft-delete, find_latest_for_update won't find the turn,
    // so the turn won't match the latest. The exact error depends on
    // whether there are other turns or not.
    assert!(
        matches!(
            err,
            MutationError::NotLatestTurn | MutationError::TurnNotFound { .. }
        ),
        "expected NotLatestTurn or TurnNotFound, got: {err:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 7.2: TurnService::retry
// ════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn retry_success_returns_new_request_id_and_content() {
    let (svc, ctx, chat_id, tenant_id) = setup().await;

    let request_id = create_completed_turn(
        &svc.db,
        &*svc.turn_repo,
        &*svc.message_repo,
        tenant_id,
        chat_id,
        ctx.subject_id(),
    )
    .await;

    let result = svc.retry(&ctx, chat_id, request_id).await.unwrap();
    assert_ne!(result.new_request_id, request_id);
    assert_eq!(result.user_content, "Hello world");

    // Verify old turn is soft-deleted with replacement link
    let scope = AccessScope::for_tenant(tenant_id);
    let conn = svc.db.conn().unwrap();
    let old_turn = svc
        .turn_repo
        .find_by_chat_and_request_id(&conn, &scope, chat_id, request_id)
        .await
        .unwrap()
        .unwrap();
    assert!(old_turn.deleted_at.is_some());
    assert_eq!(old_turn.replaced_by_request_id, Some(result.new_request_id));

    // Verify new turn exists in running state
    let new_turn = svc
        .turn_repo
        .find_by_chat_and_request_id(&conn, &scope, chat_id, result.new_request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new_turn.state, TurnState::Running);
}

// ════════════════════════════════════════════════════════════════════════════
// 7.3: TurnService::edit
// ════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn edit_success_returns_updated_content() {
    let (svc, ctx, chat_id, tenant_id) = setup().await;

    let request_id = create_completed_turn(
        &svc.db,
        &*svc.turn_repo,
        &*svc.message_repo,
        tenant_id,
        chat_id,
        ctx.subject_id(),
    )
    .await;

    let result = svc
        .edit(&ctx, chat_id, request_id, "Updated content".to_owned())
        .await
        .unwrap();
    assert_ne!(result.new_request_id, request_id);
    assert_eq!(result.user_content, "Updated content");

    // Verify old turn soft-deleted with replacement
    let scope = AccessScope::for_tenant(tenant_id);
    let conn = svc.db.conn().unwrap();
    let old_turn = svc
        .turn_repo
        .find_by_chat_and_request_id(&conn, &scope, chat_id, request_id)
        .await
        .unwrap()
        .unwrap();
    assert!(old_turn.deleted_at.is_some());
    assert_eq!(old_turn.replaced_by_request_id, Some(result.new_request_id));
}

#[tokio::test]
async fn edit_uses_same_validation_as_retry() {
    let (svc, ctx, chat_id, _) = setup().await;

    // Non-existent turn
    let err = svc
        .edit(&ctx, chat_id, Uuid::new_v4(), "new".to_owned())
        .await
        .unwrap_err();
    assert!(matches!(err, MutationError::TurnNotFound { .. }));
}
