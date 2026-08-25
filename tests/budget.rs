//! Response budget at realistic scale.
//!
//! # Why this is a test and not a one-off measurement
//!
//! The plan's caps were picked on paper. A cap that is wrong costs nothing in any small
//! fixture and everything on a real year-old board: the response gets expensive, the agent
//! starts avoiding the tool, and the project fails at its own primary goal without a single
//! test going red. Encoding the budget as an assertion is the only way that failure becomes
//! visible at the moment someone widens a limit.
//!
//! The ceilings are deliberately generous -- they are a tripwire against an order-of-
//! magnitude regression, not a style guide.

use ai_kanban::core::model::*;
use ai_kanban::core::note::NoteDraft;
use ai_kanban::core::recall::{RecallQuery, DEFAULT_LIMIT};
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::Store;
use ai_kanban::render;

/// ~4 characters per token for English prose. Rough on purpose: the question here is
/// "hundreds or thousands", and no precision beyond that changes a decision.
fn tokens(s: &str) -> usize { s.len() / 4 }

/// A year-old project, at the scale the plan names: 200 tasks, 40 notes, 800 events.
fn year_old_project() -> (Store, i64) {
    let s = Store::open_in_memory().unwrap();
    let dir = std::env::temp_dir().join("aik-budget");
    std::fs::create_dir_all(&dir).unwrap();
    let pid = s.resolve_project(&dir).unwrap().project.id;

    for i in 0..200 {
        let t = s.create_task(pid, TaskDraft {
            body: format!("Long-form description for task {i}. Users report intermittent failures \
                           in the redirect chain when the cache layer is warm."),
            ..TaskDraft::new(format!("Task {i}: fix the thing that broke in the auth layer"))
        }).unwrap();
        // Most work is finished; that is what makes an old board large.
        if i % 5 != 0 {
            s.update_task(pid, t.id, TaskPatch {
                status: Some(Status::Done),
                log: Some(format!("resolved {i}: root cause was middleware ordering, not the handler")),
                ..Default::default()
            }).unwrap();
        }
    }
    for i in 0..40 {
        s.create_note(pid, NoteDraft {
            body: format!("Note {i}: the middleware rewrites Location headers before the redirect \
                           guard runs, so the guard never fires on a warm cache."),
            paths: vec![format!("src/auth/middleware_{i}.rs")],
            ..NoteDraft::new(format!("What we learned about subsystem {i}"))
        }, Actor::Agent).unwrap();
    }
    while s.overview(Some(pid)).unwrap().events < 800 {
        s.log(pid, Actor::Agent, "session summary: touched the cache layer and the redirect chain").unwrap();
    }
    (s, pid)
}

#[test]
fn a_year_old_board_still_costs_about_what_a_small_one_does() {
    let (s, pid) = year_old_project();
    let o = s.overview(Some(pid)).unwrap();
    assert!(o.tasks >= 200 && o.notes >= 40 && o.events >= 800, "fixture must be realistic: {o:?}");

    let snap = s.board(pid, &BoardQuery::board()).unwrap();
    let text = render::board(&snap);
    eprintln!("\n=== board() at 200 tasks / 40 notes / 800 events -- ~{} tokens ===\n{}", tokens(&text), text);

    // The whole point of the caps: cost is bounded by the limits, not by history.
    assert!(tokens(&text) < 900, "board cost {} tokens", tokens(&text));
    assert!(text.contains("+"), "the omitted remainder must be reported, not dropped");
}

#[test]
fn filing_a_side_quest_stays_cheap() {
    // The most important number here. This is what the agent pays every time it spots an
    // unrelated bug and files it -- the priority-#2 path. If this is expensive, filing gets
    // skipped, and skipping is the exact failure this project exists to fix.
    let (s, pid) = year_old_project();
    let t = s.create_task(pid, TaskDraft::new("side quest: the config loader ignores env overrides")).unwrap();
    let text = render::board(&s.board_after_mutation(pid, t.id).unwrap());
    eprintln!("\n=== task_add response -- ~{} tokens ===\n{}", tokens(&text), text);

    // Ceiling set just above the measured cost: this is the number that decides whether
    // filing a side quest feels free, so a regression here should be loud.
    assert!(tokens(&text) < 200, "task_add cost {} tokens", tokens(&text));
    assert!(text.contains(&format!("#{}", t.id)), "the new task must be visible without a second call");
    assert!(text.contains("backlog"), "the unlisted backlog must still be accounted for as a count");
}

#[test]
fn recall_with_a_full_page_of_hits_stays_affordable() {
    let (s, pid) = year_old_project();
    let r = s.recall(&RecallQuery { text: "middleware redirect", project_id: Some(pid), limit: DEFAULT_LIMIT }).unwrap();
    let text = render::recall(&r, false);
    eprintln!("\n=== recall (10 hits) -- ~{} tokens ===\n{}", tokens(&text), text);

    assert_eq!(r.hits.len(), DEFAULT_LIMIT);
    assert!(tokens(&text) < 900, "recall cost {} tokens", tokens(&text));
}

#[test]
fn resuming_one_task_costs_more_than_a_board_and_should() {
    // task_show is the deliberate exception: it is asked for when the agent has committed
    // to one piece of work, so full history is what it came for.
    let (s, pid) = year_old_project();
    let t = s.create_task(pid, TaskDraft::new("the task being resumed")).unwrap();
    for i in 0..15 {
        s.update_task(pid, t.id, TaskPatch {
            status: Some(if i % 2 == 0 { Status::Doing } else { Status::Blocked }),
            log: Some(format!("step {i}: tried the middleware ordering fix, still fails on warm cache")),
            ..Default::default()
        }).unwrap();
    }
    let text = render::task_detail(&s.task_detail(pid, t.id).unwrap());
    eprintln!("\n=== task_show (16 events) -- ~{} tokens ===\n{}", tokens(&text), text);
    assert!(tokens(&text) < 700, "task_show cost {} tokens", tokens(&text));
}

#[test]
fn a_mutation_response_still_shows_what_is_in_flight() {
    // The narrow filter is only defensible if it keeps the part that matters. A write
    // response that showed *only* the changed task would answer "did it land" and drop
    // "what else is going on", which is half the reason to return a board at all.
    let (s, pid) = year_old_project();
    let doing = s.create_task(pid, TaskDraft { status: Status::Doing, ..TaskDraft::new("the work in progress") }).unwrap();
    let blocked = s.create_task(pid, TaskDraft { status: Status::Blocked, ..TaskDraft::new("the stalled work") }).unwrap();
    let filed = s.create_task(pid, TaskDraft::new("a newly filed side quest")).unwrap();

    let text = render::board(&s.board_after_mutation(pid, filed.id).unwrap());
    eprintln!("\n=== task_add with in-flight work -- ~{} tokens ===\n{}", tokens(&text), text);

    assert!(text.contains(&format!("#{}", doing.id)), "in-flight work must survive the narrow filter");
    assert!(text.contains(&format!("#{}", blocked.id)), "stalled work must survive too");
    assert!(text.contains(&format!("#{}", filed.id)));
    assert!(tokens(&text) < 250, "cost {} tokens", tokens(&text));
}

#[test]
fn the_all_boards_view_is_capped_by_board_count() {
    // The one call whose cost scales with something the project does not control: how many
    // repositories the user has touched this year. Uncapped, it grows without bound.
    let s = Store::open_in_memory().unwrap();
    for i in 0..60 {
        let dir = std::env::temp_dir().join(format!("aik-many-{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        let pid = s.resolve_project(&dir).unwrap().project.id;
        s.create_task(pid, TaskDraft { status: Status::Doing, ..TaskDraft::new(format!("in flight on board {i}")) }).unwrap();
    }
    let (sums, total) = s.project_summaries(Store::SUMMARY_LIMIT).unwrap();
    let text = render::project_summaries(&sums, total, ai_kanban::core::now());
    eprintln!("\n=== board(project: \"all\") across 60 boards -- ~{} tokens ===\n{}", tokens(&text), text);

    assert_eq!(total, 60, "the true count must survive the cap");
    assert_eq!(sums.len(), Store::SUMMARY_LIMIT);
    assert!(text.contains("35 more"), "the omitted boards must be reported: {text}");
    assert!(tokens(&text) < 700, "all-boards view cost {} tokens", tokens(&text));
}
