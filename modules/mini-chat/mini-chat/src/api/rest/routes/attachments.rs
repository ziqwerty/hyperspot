use axum::Router;
use modkit::api::OpenApiRegistry;
use modkit::api::operation_builder::OperationBuilder;

use super::AiChatLicense;
use crate::api::rest::handlers;

pub(super) fn register_attachment_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    prefix: &str,
) -> Router {
    // POST {prefix}/v1/chats/{id}/attachments (multipart/form-data)
    router = OperationBuilder::post(format!("{prefix}/v1/chats/{{id}}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .tag("attachments")
        .authenticated()
        .require_license_features([&AiChatLicense])
        .path_param("id", "Chat UUID")
        .handler(handlers::attachments::upload_attachment)
        .json_response(http::StatusCode::CREATED, "Attachment uploaded")
        .error_415(openapi)
        .standard_errors(openapi)
        .register(router, openapi);

    // GET {prefix}/v1/chats/{id}/attachments/{attachment_id}
    router = OperationBuilder::get(format!(
        "{prefix}/v1/chats/{{id}}/attachments/{{attachment_id}}"
    ))
    .operation_id("mini_chat.get_attachment")
    .summary("Get attachment metadata")
    .tag("attachments")
    .authenticated()
    .require_license_features([&AiChatLicense])
    .path_param("id", "Chat UUID")
    .path_param("attachment_id", "Attachment UUID")
    .handler(handlers::attachments::get_attachment)
    .json_response(http::StatusCode::OK, "Attachment metadata")
    .standard_errors(openapi)
    .register(router, openapi);

    // DELETE {prefix}/v1/chats/{id}/attachments/{attachment_id}
    router = OperationBuilder::delete(format!(
        "{prefix}/v1/chats/{{id}}/attachments/{{attachment_id}}"
    ))
    .operation_id("mini_chat.delete_attachment")
    .summary("Delete an attachment")
    .tag("attachments")
    .authenticated()
    .require_license_features([&AiChatLicense])
    .path_param("id", "Chat UUID")
    .path_param("attachment_id", "Attachment UUID")
    .handler(handlers::attachments::delete_attachment)
    .json_response(http::StatusCode::NO_CONTENT, "Attachment deleted")
    .error_409(openapi)
    .standard_errors(openapi)
    .register(router, openapi);

    router
}
