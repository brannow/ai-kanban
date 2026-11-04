//! `board(since, until)`: what changed on a board in a time range (#1172).
//!
//! The board's `recent` section is a fixed count, so it cannot answer "what happened since
//! Friday". These pin the parts of the changes view that would go wrong quietly: the range a
//! date resolves to, which event a row reports, what the cap leaves out, and who is named.

mod common;
use common::sync::*;

use ai_kanban::core::changes::{Change, ChangeQuery};
use ai_kanban::core::model::*;
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::{Error, Store};
use ai_kanban::render;

const DAY: i64 = 86400;

fn fixture(name: &str) -> (Store, i64) {
    let s = Store::open_in_memory().unwrap();
    let tmp = std::env::temp_dir().join(format!("aik-changes-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let pid = s.resolve_project(&tmp).unwrap().project.id;
    (s, pid)
}

/// Every event of a task moved `days` into the past. Timestamps are whole seconds of real
/// time, so a range test has to place its events rather than wait for them.
fn age_task(s: &Store, task_id: i64, days: i64) {
    s.conn.execute("UPDATE events SET ts = ts - ?1 WHERE task_id = ?2", [days * DAY, task_id]).unwrap();
}

fn query(s: &Store, since: &str) -> ChangeQuery {
    let (since, until) = s.change_range(Some(since), None, ai_kanban::core::now()).unwrap();
    ChangeQuery { since, until, status: vec![], limit: ChangeQuery::LIMIT, workstream: None }
}

fn tasks(c: &ai_kanban::core::changes::Changes) -> Vec<i64> {
    c.rows.iter().filter_map(|r| match r { Change::Task(t) => Some(t.task_id), _ => None }).collect()
}

// ---------------------------------------------------------------------------
// The range
// ---------------------------------------------------------------------------

#[test]
fn relative_ages_count_back_from_now() {
    let (s, _) = fixture("rel");
    let now = 1_800_000_000;
    for (raw, back) in [("3d", 3 * DAY), ("12h", 12 * 3600), ("30m", 1800), ("2w", 14 * DAY), ("3 days", 3 * DAY), ("1 hour ago", 3600)] {
        let (since, until) = s.change_range(Some(raw), None, now).unwrap();
        assert_eq!(since, now - back, "{raw}");
        assert_eq!(until, now + 1, "until defaults to now, inclusive");
    }
}

#[test]
fn a_day_given_as_until_includes_that_whole_day() {
    let (s, _) = fixture("day");
    let now = ai_kanban::core::now();
    let (since, until) = s.change_range(Some("2026-11-02"), Some("2026-11-03"), now).unwrap();
    assert_eq!(until - since, 2 * DAY, "Nov 2 00:00 up to, not including, Nov 4 00:00 (local)");
    let (_, until_at) = s.change_range(Some("2026-11-02"), Some("2026-11-03 12:00"), now).unwrap();
    assert_eq!(until_at - since, DAY + 12 * 3600, "a time is taken as it is");
}

#[test]
fn until_alone_means_the_seven_days_before_it() {
    let (s, _) = fixture("until");
    let now = 1_800_000_000;
    let (since, until) = s.change_range(None, Some("2d"), now).unwrap();
    assert_eq!(until, now - 2 * DAY);
    assert_eq!(until - since, ChangeQuery::DEFAULT_WINDOW);
}

#[test]
fn what_cannot_be_read_as_a_moment_is_refused_with_the_forms_that_can() {
    let (s, _) = fixture("bad");
    let now = ai_kanban::core::now();
    // "3" would be a Julian day to SQLite: 4710 BC, and an empty view that looks like news.
    for raw in ["3", "3mo", "last friday", "2026-11", "11/02/2026", "2026-11-02 25"] {
        match s.change_range(Some(raw), None, now) {
            Err(Error::InvalidValue { field: "since", valid, .. }) => assert!(valid.contains("3d"), "{raw}: {valid}"),
            other => panic!("{raw} should be refused, got {other:?}"),
        }
    }
    assert!(s.change_range(Some("1d"), Some("3d"), now).is_err(), "since after until");
}

// ---------------------------------------------------------------------------
// The rows
// ---------------------------------------------------------------------------

#[test]
fn a_task_shows_once_with_its_transition_even_when_a_log_came_after() {
    let (s, pid) = fixture("once");
    let t = s.create_task(pid, TaskDraft::new("fix the redirect loop")).unwrap();
    s.update_task(pid, t.id, TaskPatch { status: Some(Status::Testing), log: Some("fixed in the guard".into()), ..Default::default() }).unwrap();
    s.log_on(pid, Some(t.id), Actor::Agent, "checked on staging too").unwrap();

    let c = s.changes(pid, &query(&s, "1d")).unwrap();
    assert_eq!(tasks(&c), vec![t.id], "one row per task, however many events");
    let Change::Task(row) = &c.rows[0] else { panic!() };
    assert_eq!(row.shown.kind, "status:testing", "the transition is the point of the line");
    assert_eq!(row.last.body, "checked on staging too", "the time and editor come from the newest event");
    assert_eq!(row.status, Some(Status::Testing));
    assert!(row.events >= 3, "created, testing, log: {}", row.events);

    let text = render::changes(None, &c);
    assert!(text.contains("-> testing: fixed in the guard"), "{text}");
    assert!(text.contains("(+"), "the other events are counted: {text}");
}

#[test]
fn only_what_happened_inside_the_range_is_listed() {
    let (s, pid) = fixture("range");
    let old = s.create_task(pid, TaskDraft::new("last month's work")).unwrap();
    age_task(&s, old.id, 30);
    let new = s.create_task(pid, TaskDraft::new("this week's work")).unwrap();
    s.log(pid, Actor::Agent, "Released as 1.2.0").unwrap();

    let c = s.changes(pid, &query(&s, "7d")).unwrap();
    assert_eq!(tasks(&c), vec![new.id]);
    assert!(c.rows.iter().any(|r| matches!(r, Change::Entry(e) if e.body == "Released as 1.2.0")),
        "a project-level log is often the most important line, and has no task");

    let wide = s.changes(pid, &query(&s, "40d")).unwrap();
    assert!(tasks(&wide).contains(&old.id));
}

#[test]
fn a_status_filter_keeps_tasks_now_in_it_and_drops_entries() {
    let (s, pid) = fixture("status");
    let done = s.create_task(pid, TaskDraft::new("finished")).unwrap();
    s.update_task(pid, done.id, TaskPatch { status: Some(Status::Done), ..Default::default() }).unwrap();
    let open = s.create_task(pid, TaskDraft::new("still open")).unwrap();
    s.log(pid, Actor::Agent, "a decision").unwrap();

    let mut q = query(&s, "1d");
    q.status = vec![Status::Done];
    let c = s.changes(pid, &q).unwrap();
    assert_eq!(tasks(&c), vec![done.id], "{:?}", open.id);
    assert_eq!(c.rows.len(), 1, "entries have no status to match");
}

#[test]
fn the_cap_states_what_it_left_out() {
    let (s, pid) = fixture("cap");
    for i in 0..40 {
        s.create_task(pid, TaskDraft::new(format!("task {i}"))).unwrap();
    }
    let c = s.changes(pid, &query(&s, "1d")).unwrap();
    assert_eq!(c.rows.len(), ChangeQuery::LIMIT);
    // 40 tasks plus the board's own `project_created` entry.
    assert_eq!(c.omitted, 41 - ChangeQuery::LIMIT);
    assert!(render::changes(None, &c).contains("+11 more in this range"));
}

#[test]
fn a_task_moved_away_leaves_a_line_on_the_board_it_left() {
    // Its events go with it (`move_task`), so the board it left keeps only the project-level
    // `moved_out` entry -- which is exactly what that board's history should say.
    let s = Store::open_in_memory().unwrap();
    let base = std::env::temp_dir().join(format!("aik-changes-move-{}", std::process::id()));
    let (a, b) = (base.join("a"), base.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let pa = s.resolve_project(&a).unwrap().project.id;
    let pb = s.resolve_project(&b).unwrap().project.id;
    let t = s.create_task(pa, TaskDraft::new("filed on the wrong board")).unwrap();
    s.move_task(pa, t.id, pb, Actor::Agent, None, None).unwrap();

    let left = s.changes(pa, &query(&s, "1d")).unwrap();
    assert!(tasks(&left).is_empty(), "no row borrows the task's new home");
    assert!(render::changes(None, &left).contains("filed on the wrong board moved to board"), "{}", render::changes(None, &left));
    assert_eq!(tasks(&s.changes(pb, &query(&s, "1d")).unwrap()), vec![t.id]);
}

#[test]
fn every_board_shares_one_cap_filled_newest_first() {
    let s = Store::open_in_memory().unwrap();
    let base = std::env::temp_dir().join(format!("aik-changes-all-{}", std::process::id()));
    let (a, b) = (base.join("a"), base.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let pa = s.resolve_project(&a).unwrap().project.id;
    let pb = s.resolve_project(&b).unwrap().project.id;
    // Board a: many older changes. Board b: one new one. A per-board cap taken in board order
    // would fill the view with a and never reach b.
    for i in 0..40 {
        let t = s.create_task(pa, TaskDraft::new(format!("older {i}"))).unwrap();
        age_task(&s, t.id, 2);
    }
    let fresh = s.create_task(pb, TaskDraft::new("the newest change")).unwrap();

    let c = s.changes_everywhere(&query(&s, "7d")).unwrap();
    assert_eq!(c.rows.len(), ChangeQuery::LIMIT);
    assert_eq!(tasks(&c)[0], fresh.id);
    assert_eq!(c.projects.first().map(|p| p.id), Some(pb), "sections in the order of their newest row");
    let text = render::changes(None, &c);
    assert!(text.starts_with("Every board"), "{text}");
}

#[test]
fn the_view_states_its_range_and_a_solo_board_names_nobody() {
    let (s, pid) = fixture("solo");
    let t = s.create_task(pid, TaskDraft::new("mine")).unwrap();
    s.update_task(pid, t.id, TaskPatch { status: Some(Status::Doing), ..Default::default() }).unwrap();
    let head = s.board(pid, &BoardQuery { status: vec![], limit: 0, recent_limit: 0, workstream: None }).unwrap();
    let text = render::changes(Some(&head), &s.changes(pid, &query(&s, "3d")).unwrap());
    assert!(text.starts_with("Board: "), "the board is named first, as everywhere: {text}");
    assert!(text.contains("changed since ") && text.contains("(3d ago), until now"), "{text}");
    assert!(!text.contains(" by "), "on a board without a room every line would say you: {text}");
}

// ---------------------------------------------------------------------------
// Who, on a shared board
// ---------------------------------------------------------------------------

#[test]
fn a_teammates_change_says_who_made_it() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "x");
    let volker = member(&relay, "volker", "v1", "x");
    let mine = benny.store.create_task(benny.pid, TaskDraft::new("mine, moved by a teammate")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let theirs = task_by_title(&volker, "mine, moved by a teammate");
    volker.store.update_task(volker.pid, theirs.id, TaskPatch {
        status: Some(Status::Testing), log: Some("verified".into()), actor: Actor::Agent, ..Default::default()
    }).unwrap();
    converge(&relay, &[&benny, &volker]);
    // Names arrive with a sync round's member list; the in-process relay records none.
    benny.store.sync_succeeded(benny.pid, &[("b1".into(), "benny".into()), ("v1".into(), "Volker".into())]).unwrap();

    let c = benny.store.changes(benny.pid, &query(&benny.store, "1d")).unwrap();
    let text = render::changes(None, &c);
    let line = text.lines().find(|l| l.contains("mine, moved by a teammate")).unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains("-> testing") && line.contains("\u{b7} by Volker"), "{line}");
    assert!(!line.contains("filed by"), "benny filed it; that is not news to benny: {line}");
    let _ = mine;
}

#[test]
fn a_teammates_move_followed_by_my_log_still_says_who_moved_it() {
    // The line reports the transition but its "by" is the newest event's writer. Split across
    // two people, the transition has to name its own author or it reads as mine.
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "x");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("split authorship")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let theirs = task_by_title(&volker, "split authorship");
    volker.store.update_task(volker.pid, theirs.id, TaskPatch {
        status: Some(Status::Testing), actor: Actor::Agent, ..Default::default()
    }).unwrap();
    converge(&relay, &[&benny, &volker]);
    benny.store.log_on(benny.pid, Some(t.id), Actor::Agent, "looked at it after him").unwrap();
    benny.store.sync_succeeded(benny.pid, &[("b1".into(), "benny".into()), ("v1".into(), "Volker".into())]).unwrap();

    let text = render::changes(None, &benny.store.changes(benny.pid, &query(&benny.store, "1d")).unwrap());
    let line = text.lines().find(|l| l.contains("split authorship")).unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains("Volker: -> testing"), "the move is his: {line}");
    assert!(!line.contains("\u{b7} by"), "the newest event is mine, so no latest-editor name: {line}");
}
