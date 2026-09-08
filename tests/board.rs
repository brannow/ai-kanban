//! Board behaviour, with emphasis on the caps.
//!
//! The caps get the most coverage here because they are the design's load-bearing
//! compromise: an uncapped board is cheap to write, works fine in every small test, and
//! quietly becomes unaffordable on a real year-old project -- at which point the agent
//! stops calling it and the whole system is dead weight. That failure is invisible without
//! a test that builds a board big enough to trip it.

use ai_kanban::core::event::status_transition;
use ai_kanban::core::model::*;
use ai_kanban::core::note::{NoteDraft, NotePatch};
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::Store;

fn fixture() -> (Store, i64) {
    let s = Store::open_in_memory().unwrap();
    let tmp = std::env::temp_dir().join(format!("aik-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let p = s.resolve_project(&tmp).unwrap();
    (s, p.project.id)
}

#[test]
fn a_filed_task_shows_up_on_the_board() {
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("fix the redirect loop")).unwrap();
    let b = s.board(pid, &BoardQuery::board()).unwrap();

    assert_eq!(b.tasks.len(), 1);
    assert_eq!(b.tasks[0].id, t.id);
    assert_eq!(b.open_count(), 1);
    // Defaults the agent never had to supply.
    assert_eq!(t.status, Status::Backlog);
    assert_eq!(t.origin, Origin::Agent);
    assert_eq!(t.priority, Priority::Normal);
}

#[test]
fn the_default_board_hides_finished_work_but_still_counts_it() {
    let (s, pid) = fixture();
    let open = s.create_task(pid, TaskDraft::new("open one")).unwrap();
    let done = s.create_task(pid, TaskDraft::new("finished one")).unwrap();
    s.update_task(pid, done.id, TaskPatch { status: Some(Status::Done), ..Default::default() }).unwrap();

    let b = s.board(pid, &BoardQuery::board()).unwrap();
    assert_eq!(b.tasks.iter().map(|t| t.id).collect::<Vec<_>>(), vec![open.id]);
    // Hidden from the list, still visible as a number -- the point of the counts.
    assert_eq!(b.count_of(Status::Done), 1);
    assert_eq!(b.open_count(), 1);
}

#[test]
fn a_big_board_is_capped_and_says_what_it_left_out() {
    let (s, pid) = fixture();
    for i in 0..200 {
        s.create_task(pid, TaskDraft::new(format!("task {i}"))).unwrap();
    }

    let b = s.board(pid, &BoardQuery::board()).unwrap();
    assert_eq!(b.tasks.len(), BoardQuery::BOARD_LIMIT);
    let (status, n) = b.omitted[0];
    assert_eq!(status, Status::Backlog);
    // Nothing vanishes silently: 200 - 30 must be accounted for.
    assert_eq!(n, 200 - BoardQuery::BOARD_LIMIT);
    assert_eq!(b.count_of(Status::Backlog), 200);
}

#[test]
fn a_mutation_response_is_tighter_than_a_board_call() {
    // This is the response an agent pays for on *every* task_add, so it is the one that
    // decides whether filing a side quest feels cheap.
    let (s, pid) = fixture();
    for i in 0..100 {
        s.create_task(pid, TaskDraft::new(format!("task {i}"))).unwrap();
    }
    let new = s.create_task(pid, TaskDraft::new("the side quest")).unwrap();

    let b = s.board_after_mutation(pid, new.id).unwrap();
    assert!(b.tasks.len() <= BoardQuery::MUTATION_LIMIT);
    assert!(b.recent.len() <= BoardQuery::MUTATION_RECENT);
    assert_eq!(b.highlight, Some(new.id));
    assert!(b.tasks.iter().any(|t| t.id == new.id), "the task just written must be visible");
}

#[test]
fn archiving_still_shows_the_task_that_changed() {
    // An archived task would fall off an open-only board, and a response that omits what
    // you just did reads as "it didn't work" -- provoking exactly the follow-up call the
    // design is trying to eliminate.
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("obsolete idea")).unwrap();
    s.update_task(pid, t.id, TaskPatch { status: Some(Status::Archived), ..Default::default() }).unwrap();

    let b = s.board_after_mutation(pid, t.id).unwrap();
    assert!(b.tasks.iter().any(|x| x.id == t.id));
}

#[test]
fn in_flight_and_urgent_work_survives_the_cap() {
    // Ordering happens in SQL before the LIMIT, so the cap keeps the most relevant tasks
    // rather than an arbitrary slice.
    let (s, pid) = fixture();
    for i in 0..50 {
        s.create_task(pid, TaskDraft::new(format!("filler {i}"))).unwrap();
    }
    let doing = s.create_task(pid, TaskDraft { status: Status::Doing, ..TaskDraft::new("in flight") }).unwrap();
    let urgent = s.create_task(pid, TaskDraft { priority: Priority::Urgent, ..TaskDraft::new("urgent backlog") }).unwrap();

    let b = s.board(pid, &BoardQuery::board()).unwrap();
    assert_eq!(b.tasks[0].id, doing.id, "doing sorts first");
    assert!(b.tasks.iter().any(|t| t.id == urgent.id), "urgent must not be capped out");
}

#[test]
fn an_update_records_the_reason_not_just_the_change() {
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("auth redirect loop")).unwrap();
    s.update_task(pid, t.id, TaskPatch {
        status: Some(Status::Done),
        log: Some("root cause was middleware ordering, not the handler".into()),
        ..Default::default()
    }).unwrap();

    let ev = s.task_events(t.id).unwrap();
    let last = ev.last().unwrap();
    assert_eq!(status_transition(&last.kind), Some("done"));
    assert!(last.body.contains("middleware ordering"), "the why is what makes history worth reading");
}

#[test]
fn an_update_without_a_reason_still_records_what_changed() {
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("thing")).unwrap();
    s.update_task(pid, t.id, TaskPatch { priority: Some(Priority::Urgent), ..Default::default() }).unwrap();

    let ev = s.task_events(t.id).unwrap();
    assert!(ev.last().unwrap().body.contains("priority normal -> urgent"));
}

#[test]
fn status_is_authoritative_and_blocked_by_is_only_annotation() {
    // Legal on purpose: blocked on something outside the board.
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("waiting on vendor")).unwrap();
    let t = s.update_task(pid, t.id, TaskPatch { status: Some(Status::Blocked), ..Default::default() }).unwrap();

    assert_eq!(t.status, Status::Blocked);
    assert_eq!(t.blocked_by, None);
}

#[test]
fn a_task_cannot_block_itself() {
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("thing")).unwrap();
    assert!(s.update_task(pid, t.id, TaskPatch { blocked_by: Some(Some(t.id)), ..Default::default() }).is_err());
}

#[test]
fn a_missing_task_reports_what_does_exist() {
    // The design law: errors self-correct. A bare "not found" causes a follow-up call.
    let (s, pid) = fixture();
    s.create_task(pid, TaskDraft::new("a real task")).unwrap();

    match s.task(pid, 9999) {
        Err(ai_kanban::core::Error::TaskNotFound { existing, .. }) => {
            assert!(!existing.is_empty(), "must list what is actually on the board");
        }
        other => panic!("expected TaskNotFound, got {other:?}"),
    }
}

#[test]
fn task_detail_carries_everything_needed_to_resume_cold() {
    let (s, pid) = fixture();
    let blocker = s.create_task(pid, TaskDraft::new("the blocker")).unwrap();
    let t = s.create_task(pid, TaskDraft::new("blocked work")).unwrap();
    s.update_task(pid, t.id, TaskPatch {
        status: Some(Status::Blocked),
        blocked_by: Some(Some(blocker.id)),
        log: Some("needs the MCP tool surface first".into()),
        ..Default::default()
    }).unwrap();
    s.create_note(pid, NoteDraft { task_id: Some(t.id), ..NoteDraft::new("what we learned") }, Actor::Agent).unwrap();

    let d = s.task_detail(pid, t.id).unwrap();
    assert_eq!(d.blocker.unwrap().id, blocker.id);
    assert_eq!(d.notes.len(), 1);
    assert!(d.events.iter().any(|e| e.body.contains("MCP tool surface")));
    assert_eq!(s.task_detail(pid, blocker.id).unwrap().blocking[0].id, t.id);
}

#[test]
fn editing_a_note_keeps_the_superseded_fact_in_history() {
    // notes hold current state only. Without this event, the record that a fact *changed*
    // would be destroyed -- on a board whose first priority is history, that is the worst
    // possible place to lose it.
    let (s, pid) = fixture();
    let n = s.create_note(pid, NoteDraft {
        body: "the guard runs before the rewrite".into(),
        ..NoteDraft::new("auth middleware")
    }, Actor::Agent).unwrap();

    s.update_note(pid, n.id, NotePatch {
        body: Some("actually the rewrite runs first".into()),
        ..Default::default()
    }).unwrap();

    let updated = s.note(pid, n.id).unwrap();
    assert_eq!(updated.body, "actually the rewrite runs first");
    assert!(updated.updated_at >= updated.created_at);

    let ev = s.recent_events(pid, 10).unwrap();
    let e = ev.iter().find(|e| e.kind == "note_updated").expect("note edits are events");
    assert!(e.body.contains("the guard runs before the rewrite"), "previous value must survive");
}

#[test]
fn project_level_history_needs_no_task() {
    let (s, pid) = fixture();
    s.log(pid, Actor::Agent, "decided to key projects on the git remote URL").unwrap();

    let b = s.board(pid, &BoardQuery::board()).unwrap();
    // Its reader is the board's `recent` section -- stated in the code, asserted here.
    assert!(b.recent.iter().any(|e| e.task_id.is_none() && e.body.contains("git remote")));
}

#[test]
fn all_projects_render_as_summaries_not_a_merged_pile() {
    let s = Store::open_in_memory().unwrap();
    let a = std::env::temp_dir().join("aik-sum-a");
    let b = std::env::temp_dir().join("aik-sum-b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let pa = s.resolve_project(&a).unwrap().project.id;
    let pb = s.resolve_project(&b).unwrap().project.id;
    for i in 0..20 { s.create_task(pa, TaskDraft::new(format!("a{i}"))).unwrap(); }
    let d = s.create_task(pb, TaskDraft { status: Status::Doing, ..TaskDraft::new("b doing") }).unwrap();

    let (sums, total) = s.project_summaries(Store::SUMMARY_LIMIT).unwrap();
    assert_eq!(total, 2);
    assert_eq!(sums.len(), 2);
    let sb = sums.iter().find(|x| x.project.id == pb).unwrap();
    assert_eq!(sb.doing[0].id, d.id);
    let sa = sums.iter().find(|x| x.project.id == pa).unwrap();
    assert_eq!(sa.open, 20);
    assert!(sa.doing.is_empty(), "a summary is counts plus what is in flight, not 20 rows");
}

#[test]
fn a_missing_task_lists_only_this_boards_tasks() {
    // The store is global. An error that lists other projects' tasks suggests ids that
    // still will not work, and gives no hint why -- worse than listing nothing.
    let s = Store::open_in_memory().unwrap();
    let mine = std::env::temp_dir().join("aik-scope-mine");
    let theirs = std::env::temp_dir().join("aik-scope-theirs");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::create_dir_all(&theirs).unwrap();
    let a = s.resolve_project(&mine).unwrap().project.id;
    let b = s.resolve_project(&theirs).unwrap().project.id;
    s.create_task(a, TaskDraft::new("my task")).unwrap();
    s.create_task(b, TaskDraft::new("their task")).unwrap();

    match s.task(a, 9999) {
        Err(ai_kanban::core::Error::TaskNotFound { existing, project, .. }) => {
            assert_eq!(existing.len(), 1, "must not leak the other project's tasks");
            assert_eq!(existing[0].1, "my task");
            assert!(!project.is_empty(), "the error must name the board it is talking about");
        }
        other => panic!("expected TaskNotFound, got {other:?}"),
    }
}

#[test]
fn a_task_from_another_project_cannot_be_touched_or_spliced_in() {
    let s = Store::open_in_memory().unwrap();
    let mine = std::env::temp_dir().join("aik-cross-mine");
    let theirs = std::env::temp_dir().join("aik-cross-theirs");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::create_dir_all(&theirs).unwrap();
    let a = s.resolve_project(&mine).unwrap().project.id;
    let b = s.resolve_project(&theirs).unwrap().project.id;
    let foreign = s.create_task(b, TaskDraft::new("their task")).unwrap();

    assert!(s.update_task(a, foreign.id, TaskPatch { status: Some(Status::Done), ..Default::default() }).is_err());
    assert!(s.task_detail(a, foreign.id).is_err());

    // The highlight-guarantee branch must not become a way to smuggle a foreign task in.
    let b_snap = s.board_after_mutation(a, foreign.id).unwrap();
    assert!(!b_snap.tasks.iter().any(|t| t.id == foreign.id));
}

#[test]
fn a_blocker_must_live_on_the_same_board() {
    let s = Store::open_in_memory().unwrap();
    let mine = std::env::temp_dir().join("aik-blk-mine");
    let theirs = std::env::temp_dir().join("aik-blk-theirs");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::create_dir_all(&theirs).unwrap();
    let a = s.resolve_project(&mine).unwrap().project.id;
    let b = s.resolve_project(&theirs).unwrap().project.id;
    let t = s.create_task(a, TaskDraft::new("my task")).unwrap();
    let foreign = s.create_task(b, TaskDraft::new("their task")).unwrap();

    // Otherwise the board renders "blocked by #12" where #12 is a different task entirely.
    assert!(s.update_task(a, t.id, TaskPatch { blocked_by: Some(Some(foreign.id)), ..Default::default() }).is_err());
}

#[test]
fn a_note_from_another_project_cannot_be_edited() {
    // Reachable in practice: recall(project: "all") renders note ids from other boards, so
    // an agent that spots a wrong note there and corrects it lands exactly here. Without a
    // project check the edit silently lands on the other board while the confirmation names
    // this one -- split memory, arriving through the knowledge layer.
    use ai_kanban::core::note::{NoteDraft, NotePatch};
    let s = Store::open_in_memory().unwrap();
    let mine = std::env::temp_dir().join("aik-note-mine");
    let theirs = std::env::temp_dir().join("aik-note-theirs");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::create_dir_all(&theirs).unwrap();
    let a = s.resolve_project(&mine).unwrap().project.id;
    let b = s.resolve_project(&theirs).unwrap().project.id;
    let foreign = s.create_note(b, NoteDraft::new("their knowledge"), Actor::Agent).unwrap();

    assert!(s.update_note(a, foreign.id, NotePatch { body: Some("overwritten".into()), ..Default::default() }).is_err());
    assert!(s.note(a, foreign.id).is_err());
    assert_eq!(s.note(b, foreign.id).unwrap().title, "their knowledge");
}

#[test]
fn a_note_or_log_cannot_attach_to_another_projects_task() {
    // An event whose project_id is local but whose task_id points into another board shows
    // up in that board's task_show while being counted in this one's recent section.
    use ai_kanban::core::note::NoteDraft;
    let s = Store::open_in_memory().unwrap();
    let mine = std::env::temp_dir().join("aik-attach-mine");
    let theirs = std::env::temp_dir().join("aik-attach-theirs");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::create_dir_all(&theirs).unwrap();
    let a = s.resolve_project(&mine).unwrap().project.id;
    let b = s.resolve_project(&theirs).unwrap().project.id;
    let foreign = s.create_task(b, TaskDraft::new("their task")).unwrap();

    assert!(s.create_note(a, NoteDraft { task_id: Some(foreign.id), ..NoteDraft::new("note") }, Actor::Agent).is_err());
    assert!(s.log_on(a, Some(foreign.id), Actor::Agent, "log line").is_err());
}

#[test]
fn the_board_says_whether_a_blocker_is_still_blocking() {
    // A task row carries only its blocker's ID, so a board that prints the id alone says
    // "blocked by #7" for as long as the row exists -- including long after #7 was finished.
    // It reads as "do not pick this up", it is the cold-start view saying it, and the reader
    // it misleads is the one least able to check. Every blocker eventually gets finished,
    // so this gets worse with time rather than better.
    let (s, pid) = fixture();
    let blocker = s.create_task(pid, TaskDraft::new("ship the migration")).unwrap();
    s.create_task(pid, TaskDraft {
        blocked_by: Some(blocker.id),
        ..TaskDraft::new("backfill the old rows")
    }).unwrap();

    let b = s.board(pid, &BoardQuery::board()).unwrap();
    assert_eq!(b.blocker_status, vec![(blocker.id, Status::Backlog)]);
    let text = ai_kanban::render::board(&b);
    assert!(text.contains(&format!("blocked by #{}", blocker.id)), "{text}");
    assert!(!text.contains("(done)"), "nothing is finished yet: {text}");

    s.update_task(pid, blocker.id,
        TaskPatch { status: Some(Status::Done), ..Default::default() }).unwrap();

    let b = s.board(pid, &BoardQuery::board()).unwrap();
    assert_eq!(b.blocker_status, vec![(blocker.id, Status::Done)]);
    let text = ai_kanban::render::board(&b);
    assert!(text.contains(&format!("blocked by #{} (done)", blocker.id)),
        "a finished blocker has to say so, or the task reads as dead: {text}");
}
