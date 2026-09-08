//! Recall is where this design is most likely to disappoint: everything can be stored
//! correctly and still be unfindable. These tests target the ways that happens.

use ai_kanban::core::model::*;
use ai_kanban::core::note::NoteDraft;
use ai_kanban::core::recall::{fts_query, RecallQuery, DEFAULT_LIMIT};
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::Store;

fn project(s: &Store, name: &str) -> i64 {
    let dir = std::env::temp_dir().join(format!("aik-recall-{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    s.resolve_project(&dir).unwrap().project.id
}

fn q<'a>(text: &'a str, project_id: Option<i64>) -> RecallQuery<'a> {
    RecallQuery { text, project_id, limit: DEFAULT_LIMIT }
}

#[test]
fn natural_phrasing_does_not_have_to_be_fts5_syntax() {
    // The failure this guards: FTS5 MATCH is a query language. `auth-loop` is a NOT
    // expression, a stray quote is a syntax error, and bare `AND` is a keyword. If raw
    // input went through, the agent would have to learn FTS5 to search reliably -- which
    // is the "needs internal knowledge" failure the design law forbids.
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "syntax");
    s.create_note(pid, NoteDraft {
        body: "the auth middleware rewrites Location headers".into(),
        ..NoteDraft::new("auth middleware")
    }, Actor::Agent).unwrap();

    for phrase in ["auth-middleware", "auth \"middleware\"", "middleware AND auth", "auth (middleware)", "  auth  "] {
        let r = s.recall(&q(phrase, Some(pid))).unwrap();
        assert!(!r.hits.is_empty(), "no hits for {phrase:?}");
    }
}

#[test]
fn punctuation_only_input_is_not_an_error() {
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "punct");
    let r = s.recall(&q("!!! ???", Some(pid))).unwrap();
    assert!(r.hits.is_empty());
    assert_eq!(fts_query("!!! ???"), None);
}

#[test]
fn hits_keep_their_kind_and_carry_context() {
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "kinds");
    s.create_note(pid, NoteDraft {
        body: "the middleware rewrites Location headers before the redirect loop guard runs".into(),
        ..NoteDraft::new("auth middleware rewrites redirects")
    }, Actor::Agent).unwrap();
    let t = s.create_task(pid, TaskDraft {
        body: "users bounce between /login and /home in a redirect loop".into(),
        ..TaskDraft::new("Fix auth redirect loop")
    }).unwrap();
    s.update_task(pid, t.id, TaskPatch {
        status: Some(Status::Done),
        log: Some("root cause was middleware ordering in the redirect chain".into()),
        ..Default::default()
    }).unwrap();

    let r = s.recall(&q("redirect", Some(pid))).unwrap();
    let kinds: Vec<HitKind> = r.hits.iter().map(|h| h.kind).collect();
    assert!(kinds.contains(&HitKind::Note));
    assert!(kinds.contains(&HitKind::Task));
    assert!(kinds.contains(&HitKind::Event));

    // A hit with no surrounding text is a title, not an answer.
    let note = r.hits.iter().find(|h| h.kind == HitKind::Note).unwrap();
    assert!(note.snippet.contains(">>"), "snippet must mark the match: {:?}", note.snippet);
    assert!(note.snippet.len() > note.title.len() / 2);

    // A task hit says what became of it, so "already solved" is visible without a follow-up.
    let task = r.hits.iter().find(|h| h.kind == HitKind::Task).unwrap();
    assert_eq!(task.status, Some(Status::Done));
}

#[test]
fn cross_project_recall_names_the_project_on_every_hit() {
    // The feature no per-repo board can offer -- and useless if you cannot tell where a
    // hit came from.
    let s = Store::open_in_memory().unwrap();
    let a = project(&s, "alpha");
    let b = project(&s, "beta");
    s.create_note(a, NoteDraft { body: "nginx proxy_pass drops the trailing slash".into(), ..NoteDraft::new("nginx redirect") }, Actor::Agent).unwrap();
    s.create_note(b, NoteDraft { body: "nginx needs absolute redirect off".into(), ..NoteDraft::new("nginx again") }, Actor::Agent).unwrap();

    let scoped = s.recall(&q("nginx", Some(a))).unwrap();
    assert_eq!(scoped.hits.len(), 1, "a scoped search must not leak other projects");

    let all = s.recall(&q("nginx", None)).unwrap();
    assert_eq!(all.hits.len(), 2);
    assert!(all.hits.iter().all(|h| !h.project.is_empty()));
    assert_eq!(all.hits.iter().map(|h| h.project.clone()).collect::<std::collections::HashSet<_>>().len(), 2);
}

#[test]
fn an_empty_result_can_still_orient() {
    // Per the design law: "no hits" alone causes a follow-up call. Reporting what the
    // store does hold turns a dead end into a next step.
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "empty");
    s.create_note(pid, NoteDraft::new("something unrelated"), Actor::Agent).unwrap();
    let r = s.recall(&q("kubernetes", Some(pid))).unwrap();

    assert!(r.hits.is_empty());
    assert_eq!(r.available.notes, 1, "the response must know what IS there");
    assert!(r.available.projects >= 1);
}

#[test]
fn recall_is_capped() {
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "capped");
    for i in 0..50 {
        s.create_note(pid, NoteDraft { body: format!("redirect note number {i}"), ..NoteDraft::new(format!("note {i}")) }, Actor::Agent).unwrap();
    }
    let r = s.recall(&q("redirect", Some(pid))).unwrap();
    assert_eq!(r.hits.len(), DEFAULT_LIMIT);
}

#[test]
fn an_edited_note_is_found_by_its_new_text_not_its_old() {
    // External-content FTS tables only stay correct if the update triggers fire. If they
    // silently stopped, search would keep returning superseded text -- confidently wrong
    // memory, which is worse than none.
    use ai_kanban::core::note::NotePatch;
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "edited");
    let n = s.create_note(pid, NoteDraft { body: "the guard runs first".into(), ..NoteDraft::new("ordering") }, Actor::Agent).unwrap();
    s.update_note(pid, n.id, NotePatch { body: Some("actually the rewrite runs first".into()), ..Default::default() }).unwrap();

    let stale = s.recall(&q("guard", Some(pid))).unwrap();
    assert!(!stale.hits.iter().any(|h| h.kind == HitKind::Note), "superseded text must leave the index");
    let fresh = s.recall(&q("rewrite", Some(pid))).unwrap();
    assert!(fresh.hits.iter().any(|h| h.kind == HitKind::Note));
}

#[test]
fn a_task_matched_on_its_title_alone_still_gets_a_snippet() {
    // The common shape of a filed side quest is a title and nothing else. Pinning the
    // snippet to the body column returns an empty string for exactly those tasks --
    // "titles, not answers", and invisible in any fixture whose tasks happen to have bodies.
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "titleonly");
    s.create_task(pid, TaskDraft::new("fix the redirect loop")).unwrap();

    let r = s.recall(&q("redirect", Some(pid))).unwrap();
    let hit = r.hits.iter().find(|h| h.kind == HitKind::Task).expect("task hit");
    assert!(hit.snippet.contains(">>redirect<<"), "empty snippet for a title-only match: {:?}", hit.snippet);
}

#[test]
fn a_note_matched_on_its_title_alone_still_gets_a_snippet() {
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "notetitle");
    s.create_note(pid, NoteDraft::new("nginx strips the trailing slash"), Actor::Agent).unwrap();

    let r = s.recall(&q("nginx", Some(pid))).unwrap();
    assert!(r.hits[0].snippet.contains(">>nginx<<"), "{:?}", r.hits[0].snippet);
}

#[test]
fn every_hit_id_means_one_thing_and_task_references_are_separate() {
    // "Output from one tool is valid input to another" only holds if `#N` has a single
    // meaning. An event hit reporting its *task's* id under `id` would make some rendered
    // ids valid task_show input and others point at an unrelated task.
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "idspace");
    let t = s.create_task(pid, TaskDraft::new("auth work")).unwrap();
    s.update_task(pid, t.id, TaskPatch {
        status: Some(Status::Done),
        log: Some("root cause was middleware ordering".into()),
        ..Default::default()
    }).unwrap();
    s.log(pid, Actor::Agent, "middleware ordering decided project-wide").unwrap();

    let r = s.recall(&q("middleware", Some(pid))).unwrap();
    let task_event = r.hits.iter().find(|h| h.kind == HitKind::Event && h.task_id.is_some()).unwrap();
    let project_log = r.hits.iter().find(|h| h.kind == HitKind::Event && h.task_id.is_none()).unwrap();

    assert_eq!(task_event.task_id, Some(t.id), "the task reference is carried explicitly");
    assert_ne!(task_event.id, t.id, "an event's own id is not its task's id");
    assert_eq!(project_log.title, "project log");
}

#[test]
fn notes_outrank_events_regardless_of_raw_fts_score() {
    // bm25 scores come from each table's own corpus statistics, so they are not comparable
    // across tables. Sorting the merged list by raw rank looks principled and silently
    // reorders on a meaningless number.
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "ranking");
    for i in 0..30 {
        s.log(pid, Actor::Agent, &format!("touched the cache layer while doing thing {i}")).unwrap();
    }
    s.create_note(pid, NoteDraft {
        body: "the cache layer keys on the raw URL, so query order changes the key".into(),
        ..NoteDraft::new("cache keying")
    }, Actor::Agent).unwrap();

    let r = s.recall(&q("cache", Some(pid))).unwrap();
    assert_eq!(r.hits[0].kind, HitKind::Note, "a durable claim answers 'what do we know' better than one log line");
}

#[test]
fn a_capped_result_says_how_many_matches_it_left_out() {
    // Recall is capped like everything else, and a capped list that looks complete sends the
    // reader away believing the store holds nothing more. `core/board.rs` already sets the
    // rule -- report what the cap excluded rather than dropping it silently.
    //
    // The number has to be COUNTED, and this fixture is built to prove it. Fifteen matching
    // notes means `recall_notes` returns exactly its own `limit` of 10 and the merged list
    // is truncated from 10 to 10 -- so inferring the omission from what came back gives 0,
    // confidently and wrongly. The cap is applied twice; only a real count sees past it.
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "omitted");
    let total = DEFAULT_LIMIT + 5;
    for i in 0..total {
        s.create_note(pid, NoteDraft {
            body: "the auth middleware rewrites Location headers".into(),
            ..NoteDraft::new(format!("middleware note {i}"))
        }, Actor::Agent).unwrap();
    }

    let r = s.recall(&q("middleware", Some(pid))).unwrap();
    assert_eq!(r.hits.len(), DEFAULT_LIMIT, "the cap still applies");
    assert_eq!(r.omitted, total - DEFAULT_LIMIT, "counted, not inferred from the truncated list");

    // And a result that fits reports nothing left out, or the notice cries wolf on every
    // search and stops being read.
    let r = s.recall(&RecallQuery { text: "middleware", project_id: Some(pid), limit: 200 }).unwrap();
    assert_eq!(r.hits.len(), total);
    assert_eq!(r.omitted, 0);
}

#[test]
fn the_omitted_count_ignores_exactly_what_the_search_ignores() {
    // Recall counts matches to report what its cap left out, and searches them to return
    // them. Those are two queries over the same tables, so their filters have to agree: a
    // count that includes rows the search can never return claims omissions that do not
    // exist, and the reader goes looking for hits that were never withheld.
    //
    // The rows this guards are the ones recall drops on purpose. Creating a task writes a
    // `created` event whose body is the task's own title, and creating a note writes
    // `note_added` the same way -- so a query matching the title matches the entity AND its
    // echo. Everything here fits well inside the limit, so the only way `omitted` comes out
    // non-zero is if the count is looking at rows the search is not.
    let s = Store::open_in_memory().unwrap();
    let pid = project(&s, "parity");
    s.create_task(pid, TaskDraft::new("nginx redirect loop")).unwrap();
    s.create_note(pid, NoteDraft {
        body: "the proxy drops the trailing slash".into(),
        ..NoteDraft::new("nginx redirect notes")
    }, Actor::Agent).unwrap();

    let r = s.recall(&RecallQuery { text: "nginx", project_id: Some(pid), limit: 200 }).unwrap();
    assert_eq!(r.hits.len(), 2, "the task and the note themselves: {:?}",
        r.hits.iter().map(|h| (h.kind, h.title.as_str())).collect::<Vec<_>>());
    assert_eq!(r.omitted, 0, "nothing was withheld, so nothing may be claimed withheld");
}
