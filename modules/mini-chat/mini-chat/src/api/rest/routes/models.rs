use axum::Router;
use modkit::api::OpenApiRegistry;
use modkit::api::operation_builder::OperationBuilder;

use super::AiChatLicense;
use crate::api::rest::dto::{ModelDto, ModelListDto};
use crate::api::rest::handlers;

pub(super) fn register_model_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    prefix: &str,
) -> Router {
    // GET {prefix}/v1/models
    router = OperationBuilder::get(format!("{prefix}/v1/models"))
        .operation_id("mini_chat.list_models")
        .summary("List available AI models")
        .tag("models")
        .authenticated()
        .require_license_features([&AiChatLicense])
        .handler(handlers::models::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, http::StatusCode::OK, "List of models")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET {prefix}/v1/models/{id}
    router = OperationBuilder::get(format!("{prefix}/v1/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get model details")
        .tag("models")
        .authenticated()
        .require_license_features([&AiChatLicense])
        .path_param("id", "Model identifier")
        .handler(handlers::models::get_model)
        .json_response_with_schema::<ModelDto>(openapi, http::StatusCode::OK, "Model details")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}
