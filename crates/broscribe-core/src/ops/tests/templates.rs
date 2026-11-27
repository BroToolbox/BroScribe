//! Template results (design §8.4) through the identity and attribution
//! writes: staleness, forgetting people, derived files.

use super::*;
use crate::queue::{TEMPLATE_KIND, template_job_key};
use crate::schema::TemplateResult;

fn result(
    m: &Meeting,
    id: &str,
    markdown: &str,
    line_people: &[(&str, &PersonId)],
) -> TemplateResult {
    TemplateResult {
        template_id: id.into(),
        template_name: format!("Template {id}"),
        instructions_sha256: String::new(),
        provider: "test".into(),
        model: None,
        requested_model: None,
        model_reported: false,
        created_at: schema::now(),
        revision: crate::index::meeting_window_revision(m),
        attribution_revision: attribution_revision(m),
        line_people: line_people
            .iter()
            .map(|(l, p)| ((*l).to_owned(), (*p).clone()))
            .collect(),
        stale: false,
        markdown: markdown.into(),
    }
}

fn store(lib: &Library, f: impl FnOnce(&Meeting) -> TemplateResult) {
    with_index(lib, |tx, idx| {
        let mut m = tx.library().load_meeting(&mid(MA))?;
        let r = f(&m);
        m.set_template_result(r);
        tx.save_meeting(&m)?;
        idx.upsert_meeting(tx, &m)
    })
    .unwrap();
}

fn template_file(lib: &Library, id: &str) -> Option<String> {
    let dir = lib.meeting_dir(&mid(MA)).unwrap();
    fs::read_to_string(
        dir.join(paths::MEETING_TEMPLATES_DIR)
            .join(format!("{id}.md")),
    )
    .ok()
}

#[test]
fn forgetting_a_person_scrubs_template_results_without_an_enrichment() {
    let (_tmp, lib) = corrected_setup();
    attribute(&lib, &[2], Some(BIA_NAME)).unwrap();
    let bia = find_person(&lib, BIA_NAME).unwrap().unwrap().id;
    store(&lib, |m| {
        result(
            m,
            "bant",
            "{{P1}} owns the budget; {{S1}} decides.",
            &[("P1", &bia)],
        )
    });
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert!(m.enrichment.is_none());
    assert!(meeting_references_person(&m, &bia));
    assert!(!template_is_stale(&m, &m.template_results[0]));
    let file = template_file(&lib, "bant").unwrap();
    assert!(
        file.contains("Bia Example owns the budget; Ana Example decides."),
        "{file}"
    );
    assert!(!file.contains("predates"), "{file}");

    with_index(&lib, |tx, idx| forget_person(tx, idx, BIA_NAME)).unwrap();
    let m = lib.load_meeting(&mid(MA)).unwrap();
    let r = &m.template_results[0];
    assert!(r.line_people.is_empty());
    assert_eq!(r.markdown, "Unknown owns the budget; {{S1}} decides.");
    assert!(r.stale && template_is_stale(&m, r));
    assert!(!meeting_references_person(&m, &bia));
    let file = template_file(&lib, "bant").unwrap();
    assert!(!file.contains(BIA_NAME), "{file}");
    assert!(file.contains("predates changes"), "{file}");
    let idx = Index::open_readonly(lib.root()).unwrap();
    assert!(idx.search_templates("Bia", None).unwrap().is_empty());
    assert_eq!(idx.search_templates("budget", None).unwrap().len(), 1);
}

#[test]
fn line_corrections_stale_template_results_and_are_allowed_while_one_runs() {
    let (_tmp, lib) = corrected_setup();
    lib.write(|tx| tx.save_person(&Person::new(pid(BIA), BIA_NAME)))
        .unwrap();
    store(&lib, |m| result(m, "notes", "{{S1}} spoke.", &[]));
    // A template job of this meeting is queued: attribution goes ahead and
    // the job's later commit stores its result stale (see the pipeline).
    lib.write(|tx| {
        crate::queue::Queue::open(tx.root())?.restart_finished(
            tx,
            TEMPLATE_KIND,
            &template_job_key(&mid(MA), "notes"),
            serde_json::json!({ "meeting_id": MA, "template_id": "notes" }),
        )
    })
    .unwrap();
    let m = attribute(&lib, &[1], Some(BIA)).unwrap();
    assert!(m.template_results[0].stale && template_is_stale(&m, &m.template_results[0]));
    assert!(
        template_file(&lib, "notes")
            .unwrap()
            .contains("predates changes")
    );
}

#[test]
fn derived_template_files_follow_the_stored_results() {
    let (_tmp, lib) = corrected_setup();
    store(&lib, |m| result(m, "notes", "Notes.", &[]));
    store(&lib, |m| result(m, "bant", "Budget.", &[]));
    assert!(
        template_file(&lib, "notes")
            .unwrap()
            .starts_with("# Template notes\n")
    );
    // Replacing one keeps a single result per template.
    store(&lib, |m| result(m, "notes", "Better notes.", &[]));
    let m = lib.load_meeting(&mid(MA)).unwrap();
    assert_eq!(m.template_results.len(), 2);
    assert!(
        template_file(&lib, "notes")
            .unwrap()
            .contains("Better notes.")
    );
    // A user file in the folder that is not a result copy is left alone.
    let folder = lib
        .meeting_dir(&mid(MA))
        .unwrap()
        .join(paths::MEETING_TEMPLATES_DIR);
    fs::write(folder.join("README.txt"), "mine").unwrap();
    lib.write(|tx| {
        tx.update_meeting(&mid(MA), |m| {
            m.template_results.retain(|r| r.template_id != "bant");
            Ok(())
        })
        .map(|_| ())
    })
    .unwrap();
    assert!(template_file(&lib, "bant").is_none());
    assert!(template_file(&lib, "notes").is_some());
    assert!(folder.join("README.txt").exists());
}
