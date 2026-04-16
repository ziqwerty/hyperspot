// Updated: 2026-04-14 by Constructor Tech
//! Constraint types for authorization decisions.
//!
//! Constraints represent row-level filtering conditions returned by the PDP.
//! They are compiled into `AccessScope` by the PEP compiler.
//!
//! ## Supported predicates
//!
//! - `Eq` / `In` - scalar value predicates (tenant scoping, resource ID)
//! - `InGroup` - group membership subquery: resource visible if member of any listed group
//! - `InGroupSubtree` - group subtree subquery: resource visible if member of any descendant of listed ancestors

use crate::pep::IntoPropertyValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A constraint on a specific resource property.
///
/// Multiple constraints within a response are `ORed`:
/// a resource matches if it satisfies ANY constraint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Constraint {
    /// The predicates within this constraint. All predicates are `ANDed`:
    /// a resource matches this constraint only if ALL predicates are satisfied.
    pub predicates: Vec<Predicate>,
}

/// A predicate comparing a resource property to a value or subquery.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Predicate {
    /// Equality: `resource_property = value`
    Eq(EqPredicate),
    /// Set membership: `resource_property IN (values)`
    In(InPredicate),
    /// Group membership: `resource_property IN (SELECT resource_id FROM membership WHERE group_id IN (group_ids))`
    InGroup(InGroupPredicate),
    /// Group subtree: `resource_property IN (SELECT resource_id FROM membership WHERE group_id IN (SELECT descendant_id FROM closure WHERE ancestor_id IN (ancestor_ids)))`
    InGroupSubtree(InGroupSubtreePredicate),
}

/// Equality predicate: `property = value`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqPredicate {
    /// Resource property name (e.g., `pep_properties::OWNER_TENANT_ID`, `pep_properties::RESOURCE_ID`).
    pub property: String,
    /// The value to match (UUID string, plain string, number, bool, etc.).
    pub value: Value,
}

impl EqPredicate {
    /// Create an equality predicate with any convertible value.
    #[must_use]
    pub fn new(property: impl Into<String>, value: impl IntoPropertyValue) -> Self {
        Self {
            property: property.into(),
            value: value.into_filter_value(),
        }
    }
}

/// Set membership predicate: `property IN (values)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InPredicate {
    /// Resource property name (e.g., `pep_properties::OWNER_TENANT_ID`, `pep_properties::RESOURCE_ID`).
    pub property: String,
    /// The set of values to match against.
    pub values: Vec<Value>,
}

impl InPredicate {
    /// Create an `IN` predicate from an iterator of convertible values.
    #[must_use]
    pub fn new<V: IntoPropertyValue>(
        property: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        Self {
            property: property.into(),
            values: values
                .into_iter()
                .map(IntoPropertyValue::into_filter_value)
                .collect(),
        }
    }
}

/// Group membership predicate: resource is visible if it belongs to any of the listed groups.
///
/// Compiles to: `property IN (SELECT resource_id FROM resource_group_membership WHERE group_id IN (group_ids))`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InGroupPredicate {
    /// Resource property to filter (e.g., `pep_properties::RESOURCE_ID`).
    pub property: String,
    /// Group UUIDs - the resource must be a member of at least one.
    pub group_ids: Vec<Value>,
}

impl InGroupPredicate {
    /// Create an `InGroup` predicate.
    #[must_use]
    pub fn new<V: IntoPropertyValue>(
        property: impl Into<String>,
        group_ids: impl IntoIterator<Item = V>,
    ) -> Self {
        Self {
            property: property.into(),
            group_ids: group_ids
                .into_iter()
                .map(IntoPropertyValue::into_filter_value)
                .collect(),
        }
    }
}

/// Group subtree predicate: resource is visible if it belongs to any group
/// that is a descendant of the listed ancestor groups.
///
/// Compiles to: `property IN (SELECT resource_id FROM resource_group_membership
///   WHERE group_id IN (SELECT descendant_id FROM resource_group_closure WHERE ancestor_id IN (ancestor_ids)))`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InGroupSubtreePredicate {
    /// Resource property to filter (e.g., `pep_properties::RESOURCE_ID`).
    pub property: String,
    /// Ancestor group UUIDs - the resource must be a member of any descendant.
    pub ancestor_ids: Vec<Value>,
}

impl InGroupSubtreePredicate {
    /// Create an `InGroupSubtree` predicate.
    #[must_use]
    pub fn new<V: IntoPropertyValue>(
        property: impl Into<String>,
        ancestor_ids: impl IntoIterator<Item = V>,
    ) -> Self {
        Self {
            property: property.into(),
            ancestor_ids: ancestor_ids
                .into_iter()
                .map(IntoPropertyValue::into_filter_value)
                .collect(),
        }
    }
}

#[cfg(test)]
#[path = "constraints_tests.rs"]
mod constraints_tests;
