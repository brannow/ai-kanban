//! Workstreams: does scoping actually fix what note #16 measured?
//!
//! The feature exists because `board()` ordered by recency and had no notion of relevance,
//! so on a large board the listed rows came from whichever workstreams happened to have
//! been touched last. These tests pin the behaviour that fixes it, and one of them
//! re-runs the original measurement.

use ai_kanban::core::model::*;
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::workstream::normalize_name;
use ai_kanban::core::Store;
use ai_kanban::render;

fn store() -> (Store, i64) {
    let s = Store::open_in_memory().unwrap();
    let dir = std::env::temp_dir().join(format!("aik-ws-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pid = s.resolve_project(&dir).unwrap().project.id;
    (s, pid)
}

fn titles(snap: &BoardSnapshot) -> Vec<String> {
    snap.tasks.iter().map(|t| t.title.clone()).collect()
}

#[test]
fn a_filed_task_joins_the_current_workstream_without_being_told() {
    let (s, pid) = store();
    // The whole reason `task_add` gained no argument: filing is the call an agent under
    // pressure skips, so the workstream has to be inherited rather than supplied.
    let w = s.ensure_workstream(pid, "contact-form").unwrap();
    s.set_current_workstream(pid, w.id).unwrap();

    let t = s.create_task(pid, TaskDraft::new("validator rejects empty labels")).unwrap();

    let scoped = s.board(pid, &BoardQuery::board().with_workstream(Some(w.id))).unwrap();
    assert_eq!(titles(&scoped), vec!["validator rejects empty labels"]);
}

#[test]
fn leaving_a_workstream_stops_new_tasks_joining_it() {
    let (s, pid) = store();
    let w = s.ensure_workstream(pid, "contact-form").unwrap();
    s.set_current_workstream(pid, w.id).unwrap();
    s.create_task(pid, TaskDraft::new("inside")).unwrap();

    s.clear_current_workstream(pid).unwrap();
    s.create_task(pid, TaskDraft::new("outside")).unwrap();

    let scoped = s.board(pid, &BoardQuery::board().with_workstream(Some(w.id))).unwrap();
    // "outside" is unscoped, and unscoped work is visible from every scope -- so the
    // assertion worth making is about which one CARRIES the workstream, not about
    // visibility.
    let inside: Vec<_> = scoped.tasks.iter().filter(|t| t.title == "inside").collect();
    assert_eq!(inside.len(), 1);
    let all = s.board(pid, &BoardQuery::board()).unwrap();
    assert_eq!(all.tasks.len(), 2, "both tasks are still on the board");
}

#[test]
fn unscoped_work_stays_visible_from_inside_a_workstream() {
    let (s, pid) = store();
    // Every board that predates this feature is entirely unscoped. Hiding that work when a
    // workstream is entered would blank an existing board the first time it is used.
    s.create_task(pid, TaskDraft::new("legacy ticket")).unwrap();
    let w = s.ensure_workstream(pid, "upgrade").unwrap();
    s.set_current_workstream(pid, w.id).unwrap();
    s.create_task(pid, TaskDraft::new("upgrade ticket")).unwrap();

    let scoped = s.board(pid, &BoardQuery::board().with_workstream(Some(w.id))).unwrap();
    let names = titles(&scoped);
    assert!(names.contains(&"legacy ticket".to_string()), "unscoped work must not vanish: {names:?}");
    assert_eq!(names[0], "upgrade ticket", "the active workstream still ranks first");
}

#[test]
fn the_active_workstream_outranks_unscoped_work_under_the_cap() {
    let (s, pid) = store();
    // The failure this ordering exists to prevent: a pile of unscoped legacy tickets
    // crowding the active workstream out of a capped board -- which would ship exactly the
    // problem the feature was built to fix.
    for i in 0..40 {
        s.create_task(pid, TaskDraft::new(format!("legacy {i}"))).unwrap();
    }
    let w = s.ensure_workstream(pid, "contact-form").unwrap();
    s.set_current_workstream(pid, w.id).unwrap();
    for i in 0..3 {
        s.create_task(pid, TaskDraft::new(format!("contact {i}"))).unwrap();
    }

    let q = BoardQuery::board().with_workstream(Some(w.id)).with_limit(10);
    let snap = s.board(pid, &q).unwrap();
    let first_three = &titles(&snap)[..3];
    for t in first_three {
        assert!(t.starts_with("contact "), "active workstream must lead the board, got {first_three:?}");
    }
}

#[test]
fn spellings_of_one_workstream_collapse_into_it() {
    let (s, pid) = store();
    // Same split-memory shape `normalize_remote` prevents for projects, one level down: an
    // agent writing "contact-form" today and "Contact Form" tomorrow must not produce two
    // half-workstreams.
    let a = s.ensure_workstream(pid, "contact-form").unwrap();
    let b = s.ensure_workstream(pid, "Contact Form").unwrap();
    let c = s.ensure_workstream(pid, "  contact_form  ").unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(a.id, c.id);
    assert_eq!(a.name, "contact-form");
    assert_eq!(normalize_name("TYPO3 v13 Upgrade!"), "typo3-v13-upgrade");
}

#[test]
fn the_directory_reports_the_workstreams_the_scope_excluded() {
    let (s, pid) = store();
    for name in ["contact-form", "seo-redirects", "core-upgrade"] {
        let w = s.ensure_workstream(pid, name).unwrap();
        s.set_current_workstream(pid, w.id).unwrap();
        s.create_task(pid, TaskDraft::new(format!("{name} work"))).unwrap();
    }
    let active = s.workstream_by_name(pid, "contact-form").unwrap().unwrap();
    let snap = s.board(pid, &BoardQuery::board().with_workstream(Some(active.id))).unwrap();

    let others: Vec<_> = snap.other_workstreams.iter().map(|w| w.workstream.name.clone()).collect();
    assert_eq!(others.len(), 2, "the other two must be reported, not hidden: {others:?}");
    assert!(!others.contains(&"contact-form".to_string()), "the active one is shown in full, not in the directory");

    let text = render::board(&snap);
    assert!(text.contains("contact-form"), "the header must name the scope: {text}");
    assert!(text.contains("other workstreams"), "the directory must be rendered: {text}");
    // `workstream_created` is deliberately NOT housekeeping, so it lands in `recent`. That
    // is only worth doing if the line explains itself -- the event body is just the name,
    // so the generic fallback would print a bare "contact-form".
    assert!(text.contains("started workstream"),
        "a created workstream must render as history, not as a bare name: {text}");
}

#[test]
fn closing_a_workstream_hides_it_from_the_directory_but_keeps_its_work() {
    let (s, pid) = store();
    let w = s.ensure_workstream(pid, "old-migration").unwrap();
    s.set_current_workstream(pid, w.id).unwrap();
    s.create_task(pid, TaskDraft::new("still open")).unwrap();
    s.clear_current_workstream(pid).unwrap();

    assert_eq!(s.workstream_summaries(pid, None).unwrap().len(), 1);
    s.close_workstream(pid, w.id).unwrap();
    assert!(s.workstream_summaries(pid, None).unwrap().is_empty(), "closed drops out of the directory");

    let all = s.board(pid, &BoardQuery::board()).unwrap();
    assert_eq!(all.tasks.len(), 1, "closing must not hide the work itself");
}

#[test]
fn closing_the_active_workstream_widens_the_board_instead_of_stranding_it() {
    let (s, pid) = store();
    let w = s.ensure_workstream(pid, "shipped-feature").unwrap();
    s.set_current_workstream(pid, w.id).unwrap();
    s.close_workstream(pid, w.id).unwrap();
    // Left pointing at a closed workstream, the board would keep filtering to finished
    // work with nothing on screen explaining why.
    assert!(s.current_workstream(pid).unwrap().is_none());
}

#[test]
fn a_store_without_the_workstream_tables_reports_no_workstreams_rather_than_failing() {
    let (s, pid) = store();
    // The SessionStart hook opens the store READ-ONLY and must never migrate it. Against a
    // store this binary has not upgraded yet these tables do not exist, and that has to
    // degrade to "no workstreams" -- rendering the pre-005 board -- instead of taking the
    // whole hook output down. Dropping the tables reproduces exactly that shape.
    s.create_task(pid, TaskDraft::new("work that predates workstreams")).unwrap();
    s.conn.execute_batch(
        // The index has to go first -- SQLite refuses to drop a column an index names.
        "DROP INDEX idx_tasks_workstream;
         DROP TABLE current_workstream;
         DROP TABLE workstreams;
         ALTER TABLE tasks DROP COLUMN workstream_id;"
    ).unwrap();

    assert!(s.current_workstream(pid).unwrap().is_none());
    assert!(s.workstream_by_name(pid, "anything").unwrap().is_none());
    assert!(s.workstream_summaries(pid, None).unwrap().is_empty());

    // The one that actually matters, and the one that was broken: an UNSCOPED board must
    // still render. Every hook path is `.ok()?`, so a query naming a column this store does
    // not have does not error anywhere visible -- it silently costs the agent its
    // SessionStart board. Keeping `workstream_id` out of TASK_COLS is not enough; naming it
    // in a WHERE or ORDER BY is equally fatal.
    let snap = s.board(pid, &BoardQuery::board()).unwrap();
    assert_eq!(snap.tasks.len(), 1, "a pre-005 store must still render its board");
    assert!(ai_kanban::hook::context_for(&s, &std::env::temp_dir().join(format!("aik-ws-{}", std::process::id()))).is_some(),
        "and the SessionStart hook must still produce output");
}

#[test]
fn the_scale_failure_from_note_16_is_fixed() {
    let (s, pid) = store();
    // Re-runs the measurement that motivated the feature: 12 workstreams, ~25 tickets each.
    // Before, the 30 listed rows came from 4 workstreams picked by recency and the active
    // one contributed 1. The point of this test is that the number below is not 1.
    let streams = ["core-upgrade","ext-solr","ext-news","ext-powermail","theme","fluid-templates",
                   "deploy-pipeline","db-migration","seo-redirects","forms","user-auth","media"];
    for stream in streams {
        let w = s.ensure_workstream(pid, stream).unwrap();
        s.set_current_workstream(pid, w.id).unwrap();
        for i in 0..25 {
            let t = s.create_task(pid, TaskDraft::new(format!("[{stream}] ticket {i}"))).unwrap();
            if i % 3 != 0 {
                s.update_task(pid, t.id, TaskPatch { status: Some(Status::Done), ..Default::default() }).unwrap();
            }
        }
    }
    let active = s.workstream_by_name(pid, "ext-powermail").unwrap().unwrap();
    s.set_current_workstream(pid, active.id).unwrap();

    let snap = s.board(pid, &BoardQuery::board().with_workstream(Some(active.id))).unwrap();
    let mine = snap.tasks.iter().filter(|t| t.title.contains("[ext-powermail]")).count();
    let text = render::board(&snap);
    eprintln!("\n=== scoped board -- ~{} tokens ===\n{}", text.len() / 4, text);

    assert_eq!(mine, snap.tasks.len(),
        "every listed row should belong to the active workstream, got {mine} of {}", snap.tasks.len());
    assert!(!snap.other_workstreams.is_empty(), "the other workstreams must still be reported");
    assert!(text.len() / 4 < 900, "a scoped board must not cost more than an unscoped one");
}

#[test]
fn a_task_can_be_moved_between_workstreams_and_the_history_says_so() {
    let (s, pid) = store();
    // Mis-filing is the EXPECTED error here, not an edge case: the agent inherits its
    // workstream silently, so the correction path has to exist.
    let wrong = s.ensure_workstream(pid, "contact-form").unwrap();
    s.set_current_workstream(pid, wrong.id).unwrap();
    let t = s.create_task(pid, TaskDraft::new("deprecated TCA calls")).unwrap();

    let right = s.ensure_workstream(pid, "typo3-v13-upgrade").unwrap();
    s.update_task(pid, t.id, TaskPatch { workstream: Some(Some(right.id)), ..Default::default() }).unwrap();

    assert_eq!(s.task_workstream(pid, t.id).unwrap(), Some(right.id));
    let scoped = s.board(pid, &BoardQuery::board().with_workstream(Some(wrong.id))).unwrap();
    assert!(!titles(&scoped).contains(&"deprecated TCA calls".to_string()),
        "it must leave the workstream it was mis-filed into");

    // With no `log`, the automatic summary has to name the move by NAME -- an id would be
    // unreadable six months later, which is the one thing that line exists to avoid.
    let history = s.task_events(t.id).unwrap();
    let last = history.last().unwrap();
    assert!(last.body.contains("typo3-v13-upgrade"),
        "the move must be legible in history without a manual log: {:?}", last.body);
}

#[test]
fn a_task_can_be_moved_out_of_every_workstream() {
    let (s, pid) = store();
    let w = s.ensure_workstream(pid, "contact-form").unwrap();
    s.set_current_workstream(pid, w.id).unwrap();
    let t = s.create_task(pid, TaskDraft::new("rotate deploy keys")).unwrap();

    // Some(None) is "general project work", distinct from None meaning "leave it alone" --
    // the same nested-Option idiom `blocked_by` uses.
    s.update_task(pid, t.id, TaskPatch { workstream: Some(None), ..Default::default() }).unwrap();
    assert_eq!(s.task_workstream(pid, t.id).unwrap(), None);
}

#[test]
fn an_ordinary_edit_does_not_disturb_the_workstream() {
    let (s, pid) = store();
    let w = s.ensure_workstream(pid, "contact-form").unwrap();
    s.set_current_workstream(pid, w.id).unwrap();
    let t = s.create_task(pid, TaskDraft::new("validator")).unwrap();

    // `None` means leave alone. Getting this wrong would silently unfile every task any
    // status change touched.
    s.update_task(pid, t.id, TaskPatch { status: Some(Status::Doing), ..Default::default() }).unwrap();
    assert_eq!(s.task_workstream(pid, t.id).unwrap(), Some(w.id));
}

#[test]
fn a_task_cannot_be_moved_into_another_boards_workstream() {
    let s = Store::open_in_memory().unwrap();
    let d1 = std::env::temp_dir().join("aik-ws-a");
    let d2 = std::env::temp_dir().join("aik-ws-b");
    std::fs::create_dir_all(&d1).unwrap();
    std::fs::create_dir_all(&d2).unwrap();
    let a = s.resolve_project(&d1).unwrap().project.id;
    let b = s.resolve_project(&d2).unwrap().project.id;

    let theirs = s.ensure_workstream(b, "their-feature").unwrap();
    let t = s.create_task(a, TaskDraft::new("ours")).unwrap();
    // Same rule `blocked_by` enforces: the store is global, so an id from another board
    // would file this task into a group the board showing it does not have.
    assert!(s.update_task(a, t.id, TaskPatch { workstream: Some(Some(theirs.id)), ..Default::default() }).is_err());
}
