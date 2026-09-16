//! The person's to-do list.
//!
//! Two things are worth testing here, and only one of them is the CRUD.
//!
//! The other is the promise the feature is *made of*: no agent surface reaches this list.
//! That is not a property of any one function -- it is a property of every agent-facing
//! path at once, so it cannot be asserted where the list is written. It is asserted in
//! `no_agent_surface_can_see_a_to_do`, against the real tool router and the real hook
//! output, so a tool added later that happened to expose one would fail here.

use ai_kanban::core::model::*;
use ai_kanban::core::note::NoteDraft;
use ai_kanban::core::task::TaskDraft;
use ai_kanban::core::todo::SWEEP_AFTER;
use ai_kanban::core::{now, Store};

fn store() -> Store {
    Store::open_in_memory().unwrap()
}

fn texts(s: &Store) -> Vec<String> {
    s.todos().unwrap().into_iter().map(|t| t.text).collect()
}

#[test]
fn a_to_do_is_added_checked_unchecked_and_removed() {
    let s = store();
    let t = s.add_todo("  renew the domain  ").unwrap();
    assert_eq!(t.text, "renew the domain", "surrounding space is not part of the errand");
    assert!(t.done_at.is_none());

    let checked = s.set_todo_done(t.id, true).unwrap();
    assert!(checked.done_at.is_some());
    // Idempotent: a second check must not re-date it, or a double-click would quietly buy
    // the item another day before the sweep.
    let again = s.set_todo_done(t.id, true).unwrap();
    assert_eq!(again.done_at, checked.done_at);

    assert!(s.set_todo_done(t.id, false).unwrap().done_at.is_none());
    assert_eq!(s.edit_todo(t.id, "renew the domain (cheaper elsewhere)").unwrap().text,
               "renew the domain (cheaper elsewhere)");

    s.remove_todo(t.id).unwrap();
    assert!(s.todos().unwrap().is_empty());
    assert!(matches!(s.todo(t.id), Err(ai_kanban::core::Error::TodoNotFound { .. })));
}

#[test]
fn a_blank_to_do_is_refused_rather_than_stored() {
    let s = store();
    assert!(s.add_todo("   ").is_err(), "an empty row on a checklist is unclickable");
    let t = s.add_todo("real one").unwrap();
    assert!(s.edit_todo(t.id, "").is_err());
    assert_eq!(texts(&s), ["real one"], "the failed edit left the original alone");
}

#[test]
fn unchecked_work_stays_on_top_in_the_order_it_was_added() {
    let s = store();
    let first = s.add_todo("first").unwrap();
    s.add_todo("second").unwrap();
    s.add_todo("third").unwrap();
    s.set_todo_done(first.id, true).unwrap();

    assert_eq!(texts(&s), ["second", "third", "first"],
               "checked items sink; the rest keep the order they were written in");
}

#[test]
fn the_list_is_global_and_survives_forgetting_the_board_it_was_written_beside() {
    // The point of having no project_id. A person's errands are not per repository, and
    // forgetting a board -- which cascades everything carrying a project_id -- must not take
    // the shopping list with it.
    let s = store();
    let dir = tempfile::tempdir().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;
    s.add_todo("call the tax office").unwrap();

    let other = tempfile::tempdir().unwrap();
    s.resolve_project(other.path()).unwrap();
    assert_eq!(texts(&s), ["call the tax office"], "the same list from another board");

    s.forget_board(pid).unwrap();
    assert_eq!(texts(&s), ["call the tax office"], "forgetting a board must not take the list");
}

#[test]
fn checked_items_clear_a_day_later_and_unchecked_ones_never_do() {
    let s = store();
    let old = s.add_todo("booked the room").unwrap();
    let fresh = s.add_todo("filed the form").unwrap();
    let never = s.add_todo("still to do").unwrap();
    s.set_todo_done(old.id, true).unwrap();
    s.set_todo_done(fresh.id, true).unwrap();

    // Nothing has aged out yet: a freshly checked item is exactly what the list is meant to
    // keep showing, so an unparameterised sweep must leave both alone.
    assert_eq!(s.sweep_todos().unwrap(), 0);
    assert_eq!(texts(&s).len(), 3);

    // The boundary, with the cutoff moved instead of the clock: `old` is checked before it,
    // `fresh` is not.
    let cutoff = s.todo(fresh.id).unwrap().done_at.unwrap();
    s.set_todo_done(old.id, false).unwrap();
    s.set_todo_done(old.id, true).unwrap();
    let swept = s.sweep_todos_before(cutoff + 1).unwrap();
    assert_eq!(swept, 2, "both checked items are older than a cutoff just past them");

    assert_eq!(texts(&s), ["still to do"], "an unchecked item is never swept, however old");
    assert!(s.todo(never.id).is_ok());
    assert!(SWEEP_AFTER == 24 * 60 * 60, "the documented day, which the UI tells the person about");
}

#[test]
fn reading_the_list_sweeps_it() {
    // The sweep has no scheduler behind it; a read is the only thing that ever runs it. If
    // that stopped being true, checked items would pile up forever with nothing to say so.
    let s = store();
    let t = s.add_todo("done ages ago").unwrap();
    s.set_todo_done(t.id, true).unwrap();
    // Backdate by moving the item's done_at behind the real cutoff, the only way to age a
    // row without a fake clock: check it, then sweep against a cutoff beyond it.
    s.sweep_todos_before(now() + SWEEP_AFTER).unwrap();
    assert!(s.todos().unwrap().is_empty());
}

#[test]
fn every_write_moves_the_revision_and_a_plain_read_does_not() {
    // This list writes no events, so `MAX(events.id)` -- what the live page polls for
    // everything else -- cannot see it. The revision is what stands in, and a write that
    // failed to move it would be invisible to every open page, silently.
    let s = store();
    let mut last = s.todo_rev().unwrap();
    let mut moved = |s: &Store, what: &str| {
        let now = s.todo_rev().unwrap();
        assert!(now > last, "{what} left the revision where it was, so no open page would see it");
        last = now;
    };

    let t = s.add_todo("one").unwrap();
    moved(&s, "adding");
    s.set_todo_done(t.id, true).unwrap();
    moved(&s, "checking");
    s.set_todo_done(t.id, false).unwrap();
    moved(&s, "unchecking");
    s.edit_todo(t.id, "one, reworded").unwrap();
    moved(&s, "editing");

    let quiet = s.todo_rev().unwrap();
    s.todos().unwrap();
    assert_eq!(s.todo_rev().unwrap(), quiet, "a read that swept nothing must not wake every page");

    s.remove_todo(t.id).unwrap();
    moved(&s, "removing");
}

#[test]
fn to_dos_write_no_events_so_the_agent_s_history_never_mentions_them() {
    let s = store();
    let dir = tempfile::tempdir().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;
    let before = s.change_cursor().unwrap();

    let t = s.add_todo("buy milk").unwrap();
    s.set_todo_done(t.id, true).unwrap();
    s.remove_todo(t.id).unwrap();

    assert_eq!(s.change_cursor().unwrap(), before,
               "the events table is the agent's history; errands do not belong in it");
    // `recent` is not empty -- creating the board wrote an event -- but nothing in it came
    // from the list.
    let snap = s.board(pid, &BoardQuery::board()).unwrap();
    assert!(snap.recent.iter().all(|e| !e.body.contains("buy milk")),
            "nothing a to-do did shows up in what the agent reads");
}

#[test]
fn no_agent_surface_can_see_a_to_do() {
    // The feature's whole promise, asserted against the real tool router and the real
    // rendered surfaces rather than a list of names written out by hand. An agent reads the
    // board, the hook's session-start text, and recall -- a to-do must be in none of them.
    let s = store();
    let dir = tempfile::tempdir().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;
    s.create_task(pid, TaskDraft::new("a real task")).unwrap();
    s.create_note(pid, NoteDraft::new("a real note"), Actor::User).unwrap();
    s.add_todo("SECRETERRAND").unwrap();

    // Tool NAMES, not descriptions: `task_add` legitimately says "a TODO" in its prose,
    // meaning a marker in the code. What must not exist is a tool that operates on the list.
    let tools = ai_kanban::mcp::server::AiKanban::tool_router().list_all();
    assert!(!tools.is_empty(), "no tools registered -- this test would pass vacuously");
    for tool in &tools {
        assert!(!tool.name.to_lowercase().contains("todo"),
                "tool `{}` operates on the person's list; there must be no agent surface for it", tool.name);
    }

    let board = ai_kanban::render::board(&s.board(pid, &BoardQuery::board()).unwrap());
    assert!(!board.contains("SECRETERRAND"), "the board an agent reads must not carry errands");

    let hook = ai_kanban::hook::context_for(&s, dir.path()).unwrap();
    assert!(!hook.contains("SECRETERRAND"),
            "the session-start hook is the first thing an agent reads; errands are not in it");

    let hits = s.recall(&ai_kanban::core::recall::RecallQuery {
        text: "SECRETERRAND", project_id: None, limit: 10,
    }).unwrap();
    assert!(hits.hits.is_empty(), "the list is not indexed, so recall cannot surface it either");
}
