//! Template results in the index (design §7.1, layout 6).

use std::collections::BTreeMap;

use super::*;
use crate::schema::TemplateResult;

fn result(id: &str, name: &str, markdown: &str, line_people: &[(&str, &str)]) -> TemplateResult {
    TemplateResult {
        template_id: id.into(),
        template_name: name.into(),
        instructions_sha256: "0".repeat(64),
        provider: "fake".into(),
        model: None,
        requested_model: None,
        model_reported: false,
        created_at: ts("2026-11-15T10:00:00-03:00"),
        revision: String::new(),
        attribution_revision: String::new(),
        line_people: line_people
            .iter()
            .map(|(l, p)| ((*l).to_owned(), pid(p)))
            .collect::<BTreeMap<_, _>>(),
        stale: false,
        markdown: markdown.into(),
    }
}

/// Meeting 1 with one template result naming speaker 1 and, through its own
/// line label, Caio (whom the meeting names nowhere else).
fn with_template(lib: &Library) -> Meeting {
    let mut m = meetings().remove(0);
    m.set_template_result(result(
        "bant",
        "BANT insights",
        "## Need\n{{S1}} needs the café launch soon; {{P1}} holds the purse strings.",
        &[("P1", CAIO)],
    ));
    lib.write(|tx| tx.save_meeting(&m)).unwrap();
    m
}

fn template_ids(hits: &[TemplateHit]) -> Vec<(String, String)> {
    hits.iter()
        .map(|h| (h.meeting_id.to_string(), h.template_id.clone()))
        .collect()
}

#[test]
fn template_results_are_searchable_with_current_names() {
    let (_tmp, lib) = setup();
    with_template(&lib);
    rebuild(&lib);
    let idx = reader(&lib);
    let hits = idx.search_templates("cafe", None).unwrap();
    assert_eq!(template_ids(&hits), [(M1.to_owned(), "bant".to_owned())]);
    let hit = &hits[0];
    assert_eq!(
        (hit.kind, hit.template_name.as_str()),
        ("template", "BANT insights")
    );
    assert!(hit.snippet.contains("**café**"), "{}", hit.snippet);
    assert!(hit.snippet.contains("Ana Example"), "{}", hit.snippet);
    // The line label resolves through the result's own map.
    assert_eq!(
        template_ids(&idx.search_templates("Caio purse", None).unwrap()).len(),
        1
    );
    // Utterance search is unchanged; nothing to find for blank text.
    assert!(idx.search_templates("  ", None).unwrap().is_empty());
    assert!(idx.search_templates("budget", None).unwrap().is_empty());
    assert_eq!(idx.search(&text("budget")).unwrap().len(), 3);
}

#[test]
fn renaming_a_person_named_only_by_a_template_result_refreshes_it() {
    let (_tmp, lib) = setup();
    rebuild(&lib);
    let m = with_template(&lib);
    let mut idx = Index::open(lib.root()).unwrap();
    lib.write(|tx| idx.upsert_meeting(tx, &m)).unwrap();
    assert!(
        reader(&lib)
            .search_templates("Renamed", None)
            .unwrap()
            .is_empty()
    );
    let mut caio = people().remove(2);
    caio.name = "Caio Renamed".into();
    lib.write(|tx| {
        tx.save_person(&caio)?;
        idx.upsert_person(tx, &caio)
    })
    .unwrap();
    let hits = reader(&lib).search_templates("Renamed", None).unwrap();
    assert_eq!(template_ids(&hits), [(M1.to_owned(), "bant".to_owned())]);
    // A rebuild from the files gives the same rows.
    rebuild(&lib);
    assert_eq!(
        template_ids(&reader(&lib).search_templates("Renamed", None).unwrap()),
        template_ids(&hits)
    );

    // Results the meeting no longer has leave the index with it.
    let mut m = lib.load_meeting(&mid(M1)).unwrap();
    m.template_results.clear();
    lib.write(|tx| {
        tx.save_meeting(&m)?;
        idx.upsert_meeting(tx, &m)
    })
    .unwrap();
    assert!(
        reader(&lib)
            .search_templates("Renamed", None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn version_4_and_5_indexes_migrate_to_current_in_place() {
    for downgrade in [downgrade_to_v4 as fn(&Library), downgrade_to_v5] {
        let (_tmp, lib) = setup();
        rebuild(&lib);
        downgrade(&lib);
        // A reader upgrades it; the other rows stay.
        let idx = reader(&lib);
        assert_eq!(idx.version, INDEX_SCHEMA_VERSION);
        assert_eq!(idx.search(&text("budget")).unwrap().len(), 3);
        assert!(idx.search_templates("cafe", None).unwrap().is_empty());
        drop(idx);
        assert_eq!(
            meta_value(&lib, "schema_version"),
            INDEX_SCHEMA_VERSION.to_string()
        );
        // A writer then stores template results.
        let m = with_template(&lib);
        let mut writer = Index::open(lib.root()).unwrap();
        lib.write(|tx| writer.upsert_meeting(tx, &m)).unwrap();
        assert_eq!(
            reader(&lib).search_templates("cafe", None).unwrap().len(),
            1
        );
    }
}

#[cfg(unix)]
#[test]
fn read_only_version_5_index_answers_template_search_with_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, lib) = setup();
    with_template(&lib);
    rebuild(&lib);
    downgrade_to_v5(&lib);
    let db = Index::path(lib.root());
    let mode = |m| fs::set_permissions(&db, fs::Permissions::from_mode(m)).unwrap();
    mode(0o444);
    if fs::OpenOptions::new().write(true).open(&db).is_ok() {
        // Running as root: file modes do not stop writes.
        mode(0o644);
        return;
    }
    let idx = reader(&lib);
    assert_eq!(idx.version, 5);
    assert!(idx.search_templates("cafe", None).unwrap().is_empty());
    // Existing reads still work.
    assert_eq!(idx.search(&text("budget")).unwrap().len(), 3);
    assert!(idx.resolved_models("fake").unwrap().is_empty());
    assert_eq!(idx.search_meetings(&text("roadmap")).unwrap().len(), 1);
    drop(idx);
    mode(0o644);
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        if side.exists() {
            fs::set_permissions(&side, fs::Permissions::from_mode(0o644)).unwrap();
        }
    }
    assert_eq!(meta_value(&lib, "schema_version"), "5");
}
