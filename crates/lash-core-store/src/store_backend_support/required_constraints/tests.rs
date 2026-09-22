use super::*;

#[test]
fn postgres_deparser_rewrites_compare_as_equal() {
    let source = "state IN ('pending', 'done')";
    let deparsed = "(state = ANY (ARRAY['pending'::text, 'done'::text]))";
    assert_eq!(parse_expression(source), parse_expression(deparsed));
}

#[test]
fn quoted_identifier_comparison_preserves_sql_identity() {
    let expected = parse_expression("state IN ('pending', 'done')").unwrap();
    let quoted_lowercase = parse_expression("\"state\" IN ('pending', 'done')").unwrap();
    let quoted_uppercase = parse_expression("\"STATE\" IN ('pending', 'done')").unwrap();

    assert_eq!(expected, quoted_lowercase);
    assert_ne!(expected, quoted_uppercase);
    assert_ne!(
        parse_expression("'pending'::text").unwrap(),
        parse_expression("'pending'::\"text\"").unwrap(),
        "only PostgreSQL's unquoted built-in text cast may be discarded"
    );
}

#[test]
fn parser_preserves_grouping_literal_operator_and_cast_changes() {
    let expected = parse_expression("(a = 'x' AND b = 'y') OR c = 'z'").unwrap();
    for altered in [
        "a = 'x' AND (b = 'y' OR c = 'z')",
        "(a = 'X' AND b = 'y') OR c = 'z'",
        "(a <> 'x' AND b = 'y') OR c = 'z'",
        "(a::text = 'x' AND b = 'y') OR c = 'z'",
    ] {
        assert_ne!(expected, parse_expression(altered).unwrap(), "{altered}");
    }
}

#[test]
fn extraction_ignores_comments_and_preserves_quoted_literals() {
    let ddl = r#"CREATE TABLE example (
        value TEXT,
        CONSTRAINT ck_example CHECK (
            value IN ('comma,paren)', 'quote''inside') /* comment ) */
        )
    )"#;
    let found = extract_named_check_expressions(ddl).unwrap();
    assert_eq!(
        parse_expression(&found["ck_example"]),
        parse_expression("value IN ('comma,paren)', 'quote''inside')")
    );
}

#[test]
fn sqlite_extraction_requires_a_real_declaration_keyword() {
    let forged = r#"CREATE TABLE example (
        "constraint" ck_example CHECK(value = 'ok'),
        [CONSTRAINT ck_bracket CHECK (value = 'ok')] TEXT,
        value TEXT CHECK (
            coalesce(value, 'CONSTRAINT ck_nested CHECK (value = ''ok'')') <> ''
        )
    )"#;
    assert!(extract_named_check_expressions(forged).unwrap().is_empty());

    let genuine = r#"CREATE TABLE example (
        value TEXT CONSTRAINT "ck_column" CHECK (value = 'column'),
        CONSTRAINT `ck_table` CHECK (value = 'table')
    )"#;
    let found = extract_named_check_expressions(genuine).unwrap();
    assert_eq!(found["ck_column"], "value = 'column'");
    assert_eq!(found["ck_table"], "value = 'table'");
}

#[test]
fn sqlite_extraction_rejects_virtual_table_module_arguments() {
    let virtual_table = "CREATE VIRTUAL TABLE runtime_effect_replay USING rtree(\
        id, min, max, +status CONSTRAINT ck_runtime_effect_replay_status \
        CHECK(status IN ('in_progress', 'completed', 'failed')))";
    assert!(
        extract_named_check_expressions(virtual_table)
            .unwrap_err()
            .contains("virtual tables")
    );
}

#[test]
fn unsupported_syntax_is_inconclusive() {
    let expected = [RenderedConstraint {
        table: "t",
        name: "ck",
        expression: "value = 'ok'",
    }];
    let error = compare_required_constraints(
        "test",
        &expected,
        vec![InspectedConstraint {
            table: "t".to_string(),
            name: "ck".to_string(),
            expression: "pg_catalog.lower(value) = 'ok'".to_string(),
            validated: true,
            enforced: true,
        }],
    )
    .unwrap_err();
    assert!(matches!(
        error,
        StoreError::RequiredConstraintInspectionInconclusive { .. }
    ));
}

#[test]
fn queued_work_predecessor_pairs_are_typed_and_complete() {
    for (claim_id, claim_token) in [
        (Some("claim".to_string()), None),
        (None, Some("token".to_string())),
    ] {
        let error =
            crate::store_backend_support::queued_work_claim_data(Vec::new(), claim_id, claim_token)
                .unwrap_err();
        assert!(matches!(
            error,
            StoreError::QueuedWorkPredecessorClaimCorrupt { .. }
        ));
    }

    for (claim_id, claim_token) in [
        (None, None),
        (Some("claim".to_string()), Some("token".to_string())),
    ] {
        let data = crate::store_backend_support::queued_work_claim_data(
            Vec::new(),
            claim_id.clone(),
            claim_token.clone(),
        )
        .expect("complete predecessor pairs are valid");
        assert_eq!(data.abandon_restore_claim_id, claim_id);
        assert_eq!(
            data.abandon_restore_claim_token.as_deref(),
            claim_token.as_deref()
        );
    }
}
