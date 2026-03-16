use std::sync::Arc;

use axum::Extension;
use axum::extract::{Multipart, Path};
use modkit::api::prelude::*;
use modkit_security::SecurityContext;

use crate::api::rest::dto::AttachmentDetailDto;
use crate::module::AppServices;

/// POST /mini-chat/v1/chats/{id}/attachments
///
/// Multipart upload: field name `"file"` with the file content.
/// Content-Type of the file is validated against the MIME allowlist.
#[tracing::instrument(skip(svc, ctx, multipart), fields(chat_id = %chat_id))]
pub(crate) async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(chat_id): Path<uuid::Uuid>,
    mut multipart: Multipart,
) -> ApiResult<impl IntoResponse> {
    // Extract file from multipart (field name: "file")
    let mut filename: Option<String> = None;
    let mut content_type: Option<String> = None;
    let mut file_bytes: Option<bytes::Bytes> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        Problem::new(
            http::StatusCode::BAD_REQUEST,
            "Multipart Error",
            format!("Failed to read multipart field: {e}"),
        )
    })? {
        let field_name = field.name().unwrap_or("").to_owned();
        if field_name == "file" {
            filename = field.file_name().map(ToString::to_string);
            content_type = field.content_type().map(ToString::to_string);
            file_bytes = Some(field.bytes().await.map_err(|e| {
                Problem::new(
                    http::StatusCode::BAD_REQUEST,
                    "Multipart Error",
                    format!("Failed to read file data: {e}"),
                )
            })?);
            break;
        }
    }

    let filename = filename.unwrap_or_else(|| "upload".to_owned());
    let content_type = content_type.ok_or_else(|| {
        Problem::new(
            http::StatusCode::BAD_REQUEST,
            "Missing File",
            "No file field found in multipart request",
        )
    })?;
    let file_bytes = file_bytes.ok_or_else(|| {
        Problem::new(
            http::StatusCode::BAD_REQUEST,
            "Missing File",
            "No file data found in multipart request",
        )
    })?;

    let row = svc
        .attachments
        .upload_file(&ctx, chat_id, filename, &content_type, file_bytes)
        .await?;

    Ok((
        http::StatusCode::CREATED,
        Json(AttachmentDetailDto::from(row)),
    )
        .into_response())
}

/// GET /mini-chat/v1/chats/{id}/attachments/{attachment_id}
#[tracing::instrument(skip(svc, ctx), fields(chat_id = %chat_id, attachment_id = %attachment_id))]
pub(crate) async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((chat_id, attachment_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> ApiResult<JsonBody<AttachmentDetailDto>> {
    let row = svc
        .attachments
        .get_attachment(&ctx, chat_id, attachment_id)
        .await?;
    Ok(Json(AttachmentDetailDto::from(row)))
}

/// DELETE /mini-chat/v1/chats/{id}/attachments/{attachment_id}
#[tracing::instrument(skip(svc, ctx), fields(chat_id = %chat_id, attachment_id = %attachment_id))]
pub(crate) async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((chat_id, attachment_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> ApiResult<StatusCode> {
    svc.attachments
        .delete_attachment(&ctx, chat_id, attachment_id)
        .await?;
    Ok(http::StatusCode::NO_CONTENT)
}
