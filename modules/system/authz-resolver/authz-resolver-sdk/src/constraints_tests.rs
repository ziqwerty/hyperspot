// Created: 2026-04-14 by Constructor Tech
use super::*;
use modkit_security::pep_properties;
use serde_json::json;

#[test]
fn constraint_serialization_roundtrip() {
    let constraint = Constraint {
        predicates: vec![
            Predicate::In(InPredicate {
                property: pep_properties::OWNER_TENANT_ID.to_owned(),
                values: vec![
                    json!("11111111-1111-1111-1111-111111111111"),
                    json!("22222222-2222-2222-2222-222222222222"),
                ],
            }),
            Predicate::Eq(EqPredicate {
                property: pep_properties::RESOURCE_ID.to_owned(),
                value: json!("33333333-3333-3333-3333-333333333333"),
            }),
        ],
    };

    let json_str = serde_json::to_string(&constraint).unwrap();
    let deserialized: Constraint = serde_json::from_str(&json_str).unwrap();
    assert_eq!(deserialized.predicates.len(), 2);
}

#[test]
fn predicate_tag_serialization() {
    let eq = Predicate::Eq(EqPredicate {
        property: pep_properties::RESOURCE_ID.to_owned(),
        value: json!("00000000-0000-0000-0000-000000000000"),
    });

    let json_str = serde_json::to_string(&eq).unwrap();
    assert!(json_str.contains(r#""op":"eq""#));

    let in_pred = Predicate::In(InPredicate {
        property: pep_properties::OWNER_TENANT_ID.to_owned(),
        values: vec![json!("00000000-0000-0000-0000-000000000000")],
    });

    let json_str = serde_json::to_string(&in_pred).unwrap();
    assert!(json_str.contains(r#""op":"in""#));
}

#[test]
fn in_group_predicate_serialization() {
    let pred = Predicate::InGroup(InGroupPredicate {
        property: pep_properties::RESOURCE_ID.to_owned(),
        group_ids: vec![json!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")],
    });

    let json_str = serde_json::to_string(&pred).unwrap();
    assert!(json_str.contains(r#""op":"in_group""#));

    let deserialized: Predicate = serde_json::from_str(&json_str).unwrap();
    assert!(matches!(deserialized, Predicate::InGroup(_)));
}

#[test]
fn in_group_subtree_predicate_serialization() {
    let pred = Predicate::InGroupSubtree(InGroupSubtreePredicate {
        property: pep_properties::RESOURCE_ID.to_owned(),
        ancestor_ids: vec![json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")],
    });

    let json_str = serde_json::to_string(&pred).unwrap();
    assert!(json_str.contains(r#""op":"in_group_subtree""#));

    let deserialized: Predicate = serde_json::from_str(&json_str).unwrap();
    assert!(matches!(deserialized, Predicate::InGroupSubtree(_)));
}

#[test]
fn constraint_with_group_predicates_roundtrip() {
    let constraint = Constraint {
        predicates: vec![
            Predicate::In(InPredicate {
                property: pep_properties::OWNER_TENANT_ID.to_owned(),
                values: vec![json!("11111111-1111-1111-1111-111111111111")],
            }),
            Predicate::InGroup(InGroupPredicate {
                property: pep_properties::RESOURCE_ID.to_owned(),
                group_ids: vec![
                    json!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
                    json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
                ],
            }),
        ],
    };

    let json_str = serde_json::to_string(&constraint).unwrap();
    let deserialized: Constraint = serde_json::from_str(&json_str).unwrap();
    assert_eq!(deserialized.predicates.len(), 2);
    assert!(matches!(deserialized.predicates[1], Predicate::InGroup(_)));
}
