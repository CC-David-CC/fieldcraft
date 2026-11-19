//! Account file fidelity, save acknowledgements and the analytic native solver reference.
use fieldcraft_engine::Session;
use serde_json::json;

#[test]
fn cantilever_round_trip_preserves_load_constraint_and_solve() {
    let mut s = Session::new_account_model(Some("cantilever")).unwrap();
    let path = "Documents/Beam.fieldcraft".to_owned();
    assert!(s.path.is_none());
    let first = s.execute("solve.static", &json!({})).unwrap();
    let expected = first["max_displacement"]["value"].as_f64().unwrap();
    // Euler–Bernoulli PL³/(3EI); FieldCraft's reference admits shear at L/h=5.
    let beam = 4.0 * 1000.0 * 100.0_f64.powi(3) / (210_000.0 * 10.0 * 20.0_f64.powi(3));
    assert!(expected > beam && expected < 1.1 * beam, "{expected} vs {beam}");
    let bytes = s.account_bytes().unwrap();
    let saved = s.account_version();
    s.confirm_account_save(path.clone(), saved).unwrap();
    assert!(!s.is_dirty());
    let mut reopened = Session::open_account_model(Some(path), &bytes, false).unwrap();
    assert!(!reopened.is_dirty());
    assert!(reopened.model.boundary_condition("Clamp").is_some());
    assert!(reopened.model.load("Push").is_some());
    assert!(reopened.result("Load").is_some());
    let second = reopened.execute("solve.static", &json!({})).unwrap();
    assert_eq!(second["max_displacement"], first["max_displacement"]);
    assert!(reopened.is_dirty(), "a solve stores a new result even without a mesh edit");
}

#[test]
fn later_edits_remain_dirty_and_another_project_cannot_adopt_the_receipt() {
    let mut s = Session::new_account_model(None).unwrap();
    let captured = s.account_version();
    s.execute("mesh.node.add", &json!({"label":42,"xyz":[1,2,3]})).unwrap();
    assert!(!captured.matches(&s));
    s.confirm_account_save("Documents/A.fieldcraft".into(), captured).unwrap();
    assert!(s.is_dirty());
    let draft = s.account_bytes().unwrap();
    let restored = Session::open_account_model(s.path.clone(), &draft, true).unwrap();
    assert!(restored.is_dirty());
    assert_eq!(restored.model.nodes().len(), 1);
    assert!(s.confirm_account_save("Documents/B.fieldcraft".into(), restored.account_version()).is_err());
    s.execute("edit.undo", &json!({})).unwrap();
    assert!(!s.is_dirty());
    assert_eq!(s.path.as_deref(), Some("Documents/A.fieldcraft"));
    assert!(s.execute("file.save", &json!({})).is_err());
    assert!(s.execute("file.new", &json!({})).is_err());
}
