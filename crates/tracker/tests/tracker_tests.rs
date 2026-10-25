use mb_tracker::{ExperimentStore, NewRun, TrackerError};

fn store() -> (tempfile::TempDir, ExperimentStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = ExperimentStore::new(dir.path().join("tracking"));
    (dir, store)
}

#[test]
fn creates_immutable_run_and_appends_metrics() {
    let (_dir, store) = store();
    let mut request = NewRun::new(
        "baseline-logreg",
        serde_json::json!({"seed": 7, "features": "v1"}),
    );
    request.dataset_fingerprint = Some("abc123".to_string());
    request.notes = "first wiring test".to_string();
    let run = store.create_run(request).unwrap();
    assert!(run.dir().join("run.json").exists());
    assert!(run.dir().join("config.json").exists());

    let run_json_before = std::fs::read(run.dir().join("run.json")).unwrap();
    let record = run.record().unwrap();
    assert_eq!(record.name, "baseline-logreg");
    assert_eq!(record.dataset_fingerprint.as_deref(), Some("abc123"));

    run.log_metrics(serde_json::json!({"loss": 0.7, "step": 0}))
        .unwrap();
    run.log_metrics(serde_json::json!({"loss": 0.6, "step": 1}))
        .unwrap();
    let metrics = run.read_metrics().unwrap();
    assert_eq!(metrics.len(), 2);
    assert_eq!(metrics[1].metrics["step"], 1);
    assert_eq!(
        std::fs::read(run.dir().join("run.json")).unwrap(),
        run_json_before
    );
    assert!(!run.is_finished());

    let summary_path = run.finish(&serde_json::json!({"accuracy": 0.51})).unwrap();
    assert!(summary_path.exists());
    assert!(run.is_finished());
    assert!(matches!(
        run.finish(&serde_json::json!({"accuracy": 0.6})),
        Err(TrackerError::AlreadyFinalized(_))
    ));
    assert_eq!(run.read_summary().unwrap().unwrap()["accuracy"], 0.51);

    let listed = store.list_runs().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].run_id, run.run_id());
    let opened = store.open_run(run.run_id()).unwrap();
    assert_eq!(opened.read_metrics().unwrap().len(), 2);
}

#[test]
fn rejects_invalid_names_and_missing_runs() {
    let (_dir, store) = store();
    for name in ["", "../escape", "has space", "slash/name"] {
        let error = store
            .create_run(NewRun::new(name, serde_json::json!({})))
            .unwrap_err();
        assert!(matches!(error, TrackerError::InvalidName(_)), "{name}");
    }
    let error = store.open_run("does-not-exist").unwrap_err();
    assert!(matches!(error, TrackerError::RunNotFound(_)));
}

#[test]
fn metrics_must_be_objects_and_runs_are_unique() {
    let (_dir, store) = store();
    let run = store
        .create_run(NewRun::new("run-a", serde_json::json!({"x": 1})))
        .unwrap();
    assert!(run.log_metrics(serde_json::json!([1, 2, 3])).is_err());
    let second = store
        .create_run(NewRun::new("run-a", serde_json::json!({"x": 1})))
        .unwrap();
    assert_ne!(run.run_id(), second.run_id());
    assert_eq!(store.list_runs().unwrap().len(), 2);
}
