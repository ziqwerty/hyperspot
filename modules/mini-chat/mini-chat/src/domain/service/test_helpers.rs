use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::{
    AuthZResolverClient, AuthZResolverError, PolicyEnforcer,
    constraints::{Constraint, EqPredicate, Predicate},
    models::{DenyReason, EvaluationRequest, EvaluationResponse, EvaluationResponseContext},
};
use modkit_db::{
    ConnectOpts, DBProvider, Db, connect_db, migration_runner::run_migrations_for_testing,
};
use modkit_security::{SecurityContext, pep_properties};
use sea_orm_migration::MigratorTrait;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::ResolvedModel;
use crate::domain::repos::{
    ModelResolver, PolicySnapshotProvider, ThreadSummaryRepository, UserLimitsProvider,
};

// ── Mock AuthZ Resolver ──

pub struct MockAuthZResolver;

#[async_trait]
impl AuthZResolverClient for MockAuthZResolver {
    async fn evaluate(
        &self,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        let subject_tenant_id = request
            .subject
            .properties
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok());

        let subject_id = request.subject.id;

        // Deny when resource tenant_id differs from subject tenant_id
        if let Some(res_tenant) = request
            .resource
            .properties
            .get(pep_properties::OWNER_TENANT_ID)
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            && subject_tenant_id.is_some_and(|st| st != res_tenant)
        {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext {
                    deny_reason: Some(DenyReason {
                        error_code: "tenant_mismatch".to_owned(),
                        details: Some("subject tenant does not match resource tenant".to_owned()),
                    }),
                    ..Default::default()
                },
            });
        }

        // Deny when resource owner_id differs from subject id
        if let Some(res_owner) = request
            .resource
            .properties
            .get(pep_properties::OWNER_ID)
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            && res_owner != subject_id
        {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext {
                    deny_reason: Some(DenyReason {
                        error_code: "owner_mismatch".to_owned(),
                        details: Some("subject id does not match resource owner".to_owned()),
                    }),
                    ..Default::default()
                },
            });
        }

        // Build constraints from subject identity
        if request.context.require_constraints {
            let mut predicates = Vec::new();

            if let Some(tid) = subject_tenant_id {
                predicates.push(Predicate::Eq(EqPredicate::new(
                    pep_properties::OWNER_TENANT_ID,
                    tid,
                )));
            }

            predicates.push(Predicate::Eq(EqPredicate::new(
                pep_properties::OWNER_ID,
                subject_id,
            )));

            let constraints = vec![Constraint { predicates }];

            Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext {
                    constraints,
                    ..Default::default()
                },
            })
        } else {
            Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext::default(),
            })
        }
    }
}

// ── Mock Model Resolver ──

use mini_chat_sdk::ModelCatalogEntry;

/// Mock model resolver with a configurable catalog.
///
/// Default catalog: `gpt-5.2` (enabled, default) and `gpt-5-mini` (disabled).
pub struct MockModelResolver {
    catalog: Mutex<Vec<ModelCatalogEntry>>,
}

impl MockModelResolver {
    pub fn new(catalog: Vec<ModelCatalogEntry>) -> Self {
        Self {
            catalog: Mutex::new(catalog),
        }
    }
}

impl Default for MockModelResolver {
    fn default() -> Self {
        Self::new(vec![
            ModelCatalogEntry {
                model_id: "gpt-5.2".to_owned(),
                provider_model_id: "azure-gpt-5.2-2025-03".to_owned(),
                display_name: "GPT-5.2".to_owned(),
                tier: mini_chat_sdk::ModelTier::Premium,
                global_enabled: true,
                is_default: true,
                input_tokens_credit_multiplier_micro: 2_000_000,
                output_tokens_credit_multiplier_micro: 6_000_000,
                multimodal_capabilities: vec![],
                context_window: 128_000,
                max_output_tokens: 16_384,
                description: String::new(),
                provider_display_name: "OpenAI".to_owned(),
                multiplier_display: "2x".to_owned(),
                provider_id: "openai".to_owned(),
            },
            ModelCatalogEntry {
                model_id: "gpt-5-mini".to_owned(),
                provider_model_id: "azure-gpt-5-mini-2025-03".to_owned(),
                display_name: "GPT-5 Mini".to_owned(),
                tier: mini_chat_sdk::ModelTier::Standard,
                global_enabled: false,
                is_default: false,
                input_tokens_credit_multiplier_micro: 1_000_000,
                output_tokens_credit_multiplier_micro: 3_000_000,
                multimodal_capabilities: vec![],
                context_window: 64_000,
                max_output_tokens: 8_192,
                description: String::new(),
                provider_display_name: "OpenAI".to_owned(),
                multiplier_display: "1x".to_owned(),
                provider_id: "openai".to_owned(),
            },
        ])
    }
}

#[async_trait]
impl ModelResolver for MockModelResolver {
    async fn resolve_model(
        &self,
        _user_id: Uuid,
        model: Option<String>,
    ) -> Result<ResolvedModel, DomainError> {
        let catalog = self.catalog.lock().unwrap();
        match model {
            None => {
                let default = catalog
                    .iter()
                    .find(|m| m.is_default && m.global_enabled)
                    .or_else(|| catalog.iter().find(|m| m.global_enabled));
                match default {
                    Some(e) => Ok(ResolvedModel::from(e)),
                    None => Err(DomainError::invalid_model("no models available in catalog")),
                }
            }
            Some(m) if m.is_empty() => Err(DomainError::invalid_model("model must not be empty")),
            Some(m) => {
                let entry = catalog.iter().find(|e| e.model_id == m && e.global_enabled);
                match entry {
                    Some(e) => Ok(ResolvedModel::from(e)),
                    None => Err(DomainError::invalid_model(&m)),
                }
            }
        }
    }

    async fn list_visible_models(&self, _user_id: Uuid) -> Result<Vec<ResolvedModel>, DomainError> {
        let catalog = self.catalog.lock().unwrap();
        Ok(catalog
            .iter()
            .filter(|m| m.global_enabled)
            .map(ResolvedModel::from)
            .collect())
    }

    async fn get_visible_model(
        &self,
        _user_id: Uuid,
        model_id: &str,
    ) -> Result<ResolvedModel, DomainError> {
        let catalog = self.catalog.lock().unwrap();
        catalog
            .iter()
            .find(|m| m.model_id == model_id && m.global_enabled)
            .map(ResolvedModel::from)
            .ok_or_else(|| DomainError::model_not_found(model_id))
    }
}

// ── Test Helpers ──

pub async fn inmem_db() -> Db {
    let opts = ConnectOpts {
        max_conns: Some(1),
        min_conns: Some(1),
        ..Default::default()
    };
    let db = connect_db("sqlite::memory:", opts)
        .await
        .expect("Failed to connect to in-memory database");

    run_migrations_for_testing(&db, crate::infra::db::migrations::Migrator::migrations())
        .await
        .expect("Failed to run migrations");

    db
}

pub fn test_security_ctx(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .expect("failed to build SecurityContext")
}

pub fn test_security_ctx_with_id(tenant_id: Uuid, subject_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject_id)
        .subject_tenant_id(tenant_id)
        .build()
        .expect("failed to build SecurityContext")
}

pub fn mock_enforcer() -> PolicyEnforcer {
    let authz: Arc<dyn AuthZResolverClient> = Arc::new(MockAuthZResolver);
    PolicyEnforcer::new(authz)
}

/// Always-deny `AuthZ` resolver for authorization denial tests.
struct DenyingAuthZResolver;

#[async_trait]
impl AuthZResolverClient for DenyingAuthZResolver {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                deny_reason: Some(DenyReason {
                    error_code: "access_denied".to_owned(),
                    details: Some("mock: always deny".to_owned()),
                }),
                ..Default::default()
            },
        })
    }
}

pub fn mock_denying_enforcer() -> PolicyEnforcer {
    let authz: Arc<dyn AuthZResolverClient> = Arc::new(DenyingAuthZResolver);
    PolicyEnforcer::new(authz)
}

pub fn mock_model_resolver() -> Arc<dyn ModelResolver> {
    Arc::new(MockModelResolver::default())
}

pub fn mock_thread_summary_repo() -> Arc<dyn ThreadSummaryRepository> {
    struct MockThreadSummaryRepo;
    impl ThreadSummaryRepository for MockThreadSummaryRepo {}
    Arc::new(MockThreadSummaryRepo)
}

pub fn mock_db_provider(db: Db) -> Arc<DBProvider<modkit_db::DbError>> {
    Arc::new(DBProvider::new(db))
}

// ── Mock Policy Snapshot Provider ──

use mini_chat_sdk::{PolicySnapshot, UserLimits};

pub struct MockPolicySnapshotProvider {
    snapshot: Mutex<PolicySnapshot>,
}

impl MockPolicySnapshotProvider {
    pub fn new(snapshot: PolicySnapshot) -> Self {
        Self {
            snapshot: Mutex::new(snapshot),
        }
    }
}

#[async_trait]
impl PolicySnapshotProvider for MockPolicySnapshotProvider {
    async fn get_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        Ok(self.snapshot.lock().unwrap().clone())
    }

    async fn get_current_version(&self, _user_id: Uuid) -> Result<u64, DomainError> {
        Ok(self.snapshot.lock().unwrap().policy_version)
    }
}

// ── Mock User Limits Provider ──

pub struct MockUserLimitsProvider {
    limits: Mutex<UserLimits>,
}

impl MockUserLimitsProvider {
    pub fn new(limits: UserLimits) -> Self {
        Self {
            limits: Mutex::new(limits),
        }
    }
}

#[async_trait]
impl UserLimitsProvider for MockUserLimitsProvider {
    async fn get_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, DomainError> {
        Ok(self.limits.lock().unwrap().clone())
    }
}
