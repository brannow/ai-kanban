//! Backup, export and import.
//!
//! The property under test throughout is *no silent loss*. The bug that produced this
//! module (#14) was not a crash -- it was a copy that succeeded and returned a board four
//! days stale. So these assert on content, never on "the call returned Ok".

use ai_kanban::core::model::*;
use ai_kanban::core::note::NoteDraft;
use ai_kanban::core::task::TaskDraft;
use ai_kanban::core::transfer::Export;
use ai_kanban::core::page::Cursor;
use ai_kanban::core::Store;


/// Every task on a board, ignoring the paging the real callers use. Tests here care about
/// what survived a transfer, not about how it is served.
fn all_tasks(s: &Store, project_id: i64) -> Result<Vec<ai_kanban::core::model::Task>, ai_kanban::core::Error> {
    let mut out = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = s.tasks_page(project_id, &Status::ALL.to_vec(), cursor, 200)?;
        out.extend(page.items);
        match page.next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    Ok(out)
}

/// A board with the awkward parts present: a task that blocks another, a note bound to a
/// task and to files, and history on both.
fn seeded(dir: &std::path::Path) -> (Store, i64) {
    let s = Store::open(&dir.join("store.db")).unwrap();
    let work = dir.join("repo");
    std::fs::create_dir_all(&work).unwrap();
    let pid = s.resolve_project(&work).unwrap().project.id;

    let a = s.create_task(pid, TaskDraft { status: Status::Doing, priority: Priority::High,
        ..TaskDraft::new("migrate the store") }).unwrap();
    let b = s.create_task(pid, TaskDraft { status: Status::Blocked, blocked_by: Some(a.id),
        ..TaskDraft::new("ship the plugin") }).unwrap();
    s.log_on(pid, Some(b.id), Actor::Agent, "waiting on the migration").unwrap();

    let n = s.create_note(pid, NoteDraft {
        task_id: Some(a.id),
        paths: vec!["src/core/store.rs".into()],
        tags: vec!["storage".into()],
        ..NoteDraft::new("WAL keeps recent writes in a sidecar")
    }, Actor::Agent).unwrap();
    assert!(n.id > 0);
    (s, pid)
}

#[test]
fn a_backup_contains_writes_that_are_still_only_in_the_wal() {
    // The exact failure from #14: `kanban.db` was days stale because the recent work had
    // not been checkpointed, and copying it looked like it worked. A backup taken from a
    // live store must carry everything, checkpointed or not.
    let tmp = tempfile::tempdir().unwrap();
    let (s, pid) = seeded(tmp.path());
    for i in 0..20 {
        s.create_task(pid, TaskDraft::new(&format!("late task {i}"))).unwrap();
    }

    let dest = tmp.path().join("backup.db");
    s.backup_to(&dest).unwrap();

    // Opened as its own store, with the original still live and unclosed.
    let restored = Store::open(&dest).unwrap();
    let rp = restored.all_projects().unwrap();
    assert_eq!(rp.len(), 1);
    let tasks = all_tasks(&restored, rp[0].id).unwrap();
    assert_eq!(tasks.len(), 22, "a backup that loses uncheckpointed writes is the bug this fixes");
    assert!(tasks.iter().any(|t| t.title == "late task 19"));
}

#[test]
fn a_backup_refuses_to_overwrite() {
    // Destructive-by-default is wrong for the one command whose entire job is not losing
    // data, and `VACUUM INTO` reports the collision as a bare sqlite error otherwise.
    let tmp = tempfile::tempdir().unwrap();
    let (s, _) = seeded(tmp.path());
    let dest = tmp.path().join("backup.db");
    s.backup_to(&dest).unwrap();
    let err = s.backup_to(&dest).unwrap_err().to_string();
    assert!(err.contains("already exists"), "got: {err}");
}

#[test]
fn a_round_trip_through_json_preserves_the_board() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, pid) = seeded(tmp.path());
    let export = src.export(&[]).unwrap();
    let before = src.board(pid, &BoardQuery::board()).unwrap();

    let dst = Store::open(&tmp.path().join("other.db")).unwrap();
    let report = dst.import(&export).unwrap();
    assert_eq!(report.projects.len(), 1);
    assert!(report.skipped_existing.is_empty());

    let pid2 = dst.all_projects().unwrap()[0].id;
    let after = dst.board(pid2, &BoardQuery::board()).unwrap();
    assert_eq!(before.counts, after.counts, "the status breakdown must survive the trip");
    assert_eq!(
        before.tasks.iter().map(|t| t.title.clone()).collect::<Vec<_>>(),
        after.tasks.iter().map(|t| t.title.clone()).collect::<Vec<_>>(),
    );
}

#[test]
fn import_rewrites_references_rather_than_carrying_source_ids() {
    // Ids are row numbers in one store and mean nothing in another. If `blocked_by` were
    // carried across verbatim it would point at whatever task happened to hold that id --
    // a board that looks fine and is wrong, which is the worst failure this can have.
    let tmp = tempfile::tempdir().unwrap();
    let (src, _) = seeded(tmp.path());
    let export = src.export(&[]).unwrap();

    // Give the destination a different id space, so carrying ids over cannot accidentally
    // land on the right rows.
    let dst = Store::open(&tmp.path().join("other.db")).unwrap();
    let decoy = dst.resolve_project(&tmp.path().join("decoy")).unwrap().project.id;
    for i in 0..7 {
        dst.create_task(decoy, TaskDraft::new(&format!("decoy {i}"))).unwrap();
    }

    dst.import(&export).unwrap();
    let pid = dst.all_projects().unwrap().iter().find(|p| p.name != "decoy").unwrap().id;
    let tasks = all_tasks(&dst, pid).unwrap();

    let blocked = tasks.iter().find(|t| t.title == "ship the plugin").unwrap();
    let blocker = tasks.iter().find(|t| t.title == "migrate the store").unwrap();
    assert_eq!(blocked.blocked_by, Some(blocker.id));
    assert_ne!(blocker.id, 1, "the destination must have allocated its own ids");
}

#[test]
fn import_refuses_a_project_that_is_already_here() {
    // The boundary this module deliberately stops at: merging two histories of one board
    // is task #3. Refusing is recoverable; guessing which side wins is not.
    let tmp = tempfile::tempdir().unwrap();
    let (src, pid) = seeded(tmp.path());
    let export = src.export(&[]).unwrap();
    let before = all_tasks(&src, pid).unwrap().len();

    let report = src.import(&export).unwrap();
    assert_eq!(report.skipped_existing.len(), 1);
    assert!(report.projects.is_empty());
    assert_eq!(
        all_tasks(&src, pid).unwrap().len(),
        before,
        "a refused import must write nothing at all"
    );
}

#[test]
fn an_import_is_visible_to_the_change_feed() {
    // `CLAUDE.md`: every mutation writes an event, because the events table is what the
    // HTTP live stream polls. A whole board appearing is the largest mutation there is.
    let tmp = tempfile::tempdir().unwrap();
    let (src, _) = seeded(tmp.path());
    let export = src.export(&[]).unwrap();

    let dst = Store::open(&tmp.path().join("other.db")).unwrap();
    let before = dst.change_cursor().unwrap();
    dst.import(&export).unwrap();
    assert!(dst.change_cursor().unwrap() > before, "the import left the live feed unaware");

    let pid = dst.all_projects().unwrap()[0].id;
    let recent = dst.recent_events(pid, 50).unwrap();
    assert!(
        recent.iter().any(|e| e.kind == "imported"),
        "an imported board must say so in recent -- it explains why history stops at a date"
    );
}

#[test]
fn a_claimed_path_is_reported_rather_than_stolen() {
    // Taking a path another board already holds would silently redirect that board's future
    // sessions into the imported one. Skipping loses an alias the next session rebuilds.
    //
    // Building this case takes a moment's care, and the reason is worth recording: a board
    // sitting at the *same* path usually has the same key too, so it is skipped as an
    // existing project long before any path is examined. A collision needs a
    // differently-keyed board that has learned the path as an alias -- two clones with
    // different remotes at a reused location, which is what a restore onto a working
    // machine actually looks like.
    let tmp = tempfile::tempdir().unwrap();
    let (src, _) = seeded(tmp.path());
    let export = src.export(&[]).unwrap();
    // The path exactly as the export recorded it. Taking it from the export rather than
    // rebuilding it matters: paths are canonicalised on the way in, so a literal
    // `tmp/repo` here is a different string from the stored `/private/.../repo` on macOS
    // and the collision under test would never fire.
    let contested = export.projects[0].paths[0].clone();

    let dst = Store::open(&tmp.path().join("other.db")).unwrap();
    let squatter = dst.resolve_project(&tmp.path().join("elsewhere")).unwrap().project.id;
    dst.add_path_alias(squatter, std::path::Path::new(&contested)).unwrap();

    let report = dst.import(&export).unwrap();
    assert_eq!(report.projects.len(), 1, "the project itself still imports");
    assert_eq!(
        report.projects[0].paths_skipped,
        vec![contested.clone()],
        "the collision must be named, not just counted"
    );

    // And the original owner still owns it: opening that directory must not have moved.
    let owner = dst.resolve_project(std::path::Path::new(&contested)).unwrap().project.id;
    assert_eq!(owner, squatter, "an import must not redirect another board's directory");
}

#[test]
fn exporting_one_project_leaves_the_others_out() {
    let tmp = tempfile::tempdir().unwrap();
    let (s, _) = seeded(tmp.path());
    let other = s.resolve_project(&tmp.path().join("elsewhere")).unwrap().project.id;
    s.create_task(other, TaskDraft::new("not in the export")).unwrap();

    let export = s.export(&["repo".to_string()]).unwrap();
    assert_eq!(export.projects.len(), 1);
    assert!(!export.projects[0].tasks.iter().any(|t| t.title == "not in the export"));
}

#[test]
fn an_export_naming_no_known_project_is_an_error_not_an_empty_file() {
    // Silently writing `{"projects": []}` for a typo is how someone discovers their backup
    // is empty at the moment they need it.
    let tmp = tempfile::tempdir().unwrap();
    let (s, _) = seeded(tmp.path());
    assert!(s.export(&["typo".to_string()]).is_err());
}

#[test]
fn an_export_from_a_future_version_is_refused_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (s, _) = seeded(tmp.path());
    let mut export = s.export(&[]).unwrap();
    export.format = 99;
    let err = s.import(&export).unwrap_err().to_string();
    assert!(err.contains("newer than this binary"), "got: {err}");
}

#[test]
fn the_json_survives_a_trip_through_text() {
    // The format exists to be written to a file and read back somewhere else; a struct
    // that only round-trips in memory would not deliver that.
    let tmp = tempfile::tempdir().unwrap();
    let (s, _) = seeded(tmp.path());
    let text = serde_json::to_string_pretty(&s.export(&[]).unwrap()).unwrap();
    let parsed: Export = serde_json::from_str(&text).unwrap();

    let dst = Store::open(&tmp.path().join("other.db")).unwrap();
    dst.import(&parsed).unwrap();
    let pid = dst.all_projects().unwrap()[0].id;
    assert!(all_tasks(&dst, pid).unwrap().iter().any(|t| t.title == "migrate the store"));
}

#[test]
fn a_round_trip_keeps_workstreams_and_which_tasks_are_in_them() {
    // The failure this guards is silent by construction: every assertion in this file
    // predates `workstream_id`, so an export that dropped it stayed green while quietly
    // stripping the grouping off every task in the backup.
    let dir = tempfile::tempdir().unwrap();
    let src = Store::open(&dir.path().join("a.db")).unwrap();
    let pid = src.resolve_project(&dir.path().join("repo")).unwrap().project.id;

    let w = src.ensure_workstream(pid, "contact-form").unwrap();
    src.set_current_workstream(pid, w.id).unwrap();
    src.create_task(pid, TaskDraft::new("inside the workstream")).unwrap();
    src.clear_current_workstream(pid).unwrap();
    src.create_task(pid, TaskDraft::new("general work")).unwrap();
    // An empty, closed workstream: the case a "rebuild the list from the tasks" export
    // would lose entirely.
    let old = src.ensure_workstream(pid, "last-years-migration").unwrap();
    src.close_workstream(pid, old.id).unwrap();

    let export = src.export(&[]).unwrap();
    let json = serde_json::to_string(&export).unwrap();

    let dst = Store::open(&dir.path().join("b.db")).unwrap();
    dst.import(&serde_json::from_str::<Export>(&json).unwrap()).unwrap();

    let restored = dst.project_by_key(&src.project(pid).unwrap().key).unwrap().unwrap();
    let names: Vec<String> = {
        let mut v = dst.workstream_summaries(restored.id, None).unwrap()
            .into_iter().map(|w| w.workstream.name).collect::<Vec<_>>();
        v.sort();
        v
    };
    assert_eq!(names, vec!["contact-form"], "open workstreams must survive: {names:?}");

    let closed = dst.workstream_by_name(restored.id, "last-years-migration").unwrap();
    assert!(closed.is_some(), "an empty closed workstream must survive the trip");
    assert!(closed.unwrap().closed_at.is_some(), "and must still be closed");

    let ws = dst.workstream_by_name(restored.id, "contact-form").unwrap().unwrap();
    let scoped = dst.board(restored.id, &BoardQuery::board().with_workstream(Some(ws.id))).unwrap();
    let inside: Vec<_> = scoped.tasks.iter().filter(|t| t.title == "inside the workstream").collect();
    assert_eq!(inside.len(), 1, "the task must come back still in its workstream");
}

#[test]
fn an_export_written_before_workstreams_existed_still_imports() {
    // `serde(default)` is what makes this work, and it is easy to lose. An old backup that
    // stopped restoring would be discovered at exactly the wrong moment.
    let dir = tempfile::tempdir().unwrap();
    let old = r#"{"format":1,"schema_version":4,"exported_at":0,"projects":[
        {"key":"path:/old","name":"old","created_at":0,"paths":[],
         "tasks":[{"id":1,"title":"a task","body":"","status":"backlog","type":"task",
                   "origin":"agent","priority":"normal","blocked_by":null,
                   "created_at":0,"updated_at":0}],
         "notes":[],"events":[]}]}"#;
    let s = Store::open(&dir.path().join("s.db")).unwrap();
    let parsed: Export = serde_json::from_str(old).expect("a pre-005 export must still parse");
    s.import(&parsed).unwrap();

    let p = s.project_by_key("path:/old").unwrap().expect("the project must be restored");
    assert_eq!(all_tasks(&s, p.id).unwrap().len(), 1);
}
