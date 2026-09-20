mod common;
use bfw::migration::*;
use bfw::model::*;
use common::*;

fn ctx<'a>(from: &'a Compiled, rule: &'a RuleDoc, to: &'a Compiled) -> MigrationCtx<'a> {
    MigrationCtx { rule, from, to }
}

#[test]
fn dry_run_classifies_lossy_and_lists_provenance() {
    let d1 = v1();
    let d2 = v2();
    let from = compile(&Lookup(vec![d1.clone()]), &d1).unwrap();
    let to = compile(&Lookup(vec![d1, d2.clone()]), &d2).unwrap();
    let rule = rule();
    let result = dry_run(&ctx(&from, &rule, &to), &sample_bytes()).unwrap();
    assert_eq!(result.equivalence, Equivalence::Lossy);
    assert!(result.losses.contains(&"comment".to_string()));
    assert!(result.losses.contains(&"comment_len".to_string()));
    let fps_bound = result
        .provenance
        .iter()
        .any(|p| p["target"] == serde_json::json!("fps") && p["source"] == serde_json::json!("constant"));
    assert!(fps_bound);
    let parsed = bfw::parser::Parser::parse(&to, &result.forward_bytes);
    assert!(parsed.errors.is_empty());
    assert!(parsed.warnings.is_empty());
}

#[test]
fn accepted_losses_must_bind_rule_id_and_version() {
    let d1 = v1();
    let d2 = v2();
    let from = compile(&Lookup(vec![d1.clone()]), &d1).unwrap();
    let to = compile(&Lookup(vec![d1, d2.clone()]), &d2).unwrap();
    let rule = rule();
    let result = dry_run(&ctx(&from, &rule, &to), &sample_bytes()).unwrap();

    let wrong_version = vec![
        AcceptedLoss { path: "comment".into(), rule_id: "r".into(), rule_version: 99, note: "".into() },
        AcceptedLoss { path: "comment_len".into(), rule_id: "r".into(), rule_version: 99, note: "".into() },
    ];
    let err = accepted_losses_cover(&result, &wrong_version, &rule).unwrap_err();
    assert!(err["unaccepted_losses"].as_array().unwrap().len() == 2);

    let correct = vec![
        AcceptedLoss { path: "comment".into(), rule_id: "r".into(), rule_version: 1, note: "".into() },
        AcceptedLoss { path: "comment_len".into(), rule_id: "r".into(), rule_version: 1, note: "".into() },
    ];
    accepted_losses_cover(&result, &correct, &rule).unwrap();
}

#[test]
fn strictly_equal_when_format_and_bytes_match() {
    let d1 = v1();
    let compiled = compile(&Lookup(vec![d1.clone()]), &d1).unwrap();
    let identity_rule = RuleDoc {
        id: "id".into(),
        version: 1,
        from: Ref { id: "img".into(), version: 1 },
        to: Ref { id: "img".into(), version: 1 },
        bindings: vec![
            Binding { target: "width".into(), source: Source::Field { path: "width".into() }, note: "".into() },
            Binding { target: "height".into(), source: Source::Field { path: "height".into() }, note: "".into() },
            Binding { target: "flags.alpha".into(), source: Source::Field { path: "flags.alpha".into() }, note: "".into() },
            Binding { target: "flags.kind".into(), source: Source::Field { path: "flags.kind".into() }, note: "".into() },
            Binding { target: "comment_len".into(), source: Source::Field { path: "comment_len".into() }, note: "".into() },
            Binding { target: "comment".into(), source: Source::Field { path: "comment".into() }, note: "".into() },
        ],
        description: "".into(),
    };
    let result = dry_run(&ctx(&compiled, &identity_rule, &compiled), &sample_bytes()).unwrap();
    assert_eq!(result.equivalence, Equivalence::Strict);
    assert!(result.strict_bytes_equal);
}
