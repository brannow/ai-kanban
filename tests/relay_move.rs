//! A team's relay moves to another server, or is set up anew: each member only puts the new
//! URL and key in, in any order, whenever they get to it. Their shared board holds mixed work --
//! tickets each filed, edits each made to the other's -- and all of it has to come through: every
//! ticket once, with its owner and its latest edits, and sync that goes on afterwards. Nobody
//! disconnects or reconnects (docs/sync.md, "Moving to another relay"; #1130, #1132, #1133).
//!
//! New relays here issue keys the way an admin does (`key add`): not bound to a tag until the
//! machine first uses them, so what the relay hands out is what is tested, not a pinned tag.
//!
//! Each test collects everything that is wrong before failing, so one run shows the whole
//! picture instead of the first symptom.

mod common;
use common::relay_http::HttpRelay;

use ai_kanban::core::model::*;
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::Store;
use ai_kanban::remote::rooms::{self, Choice};
use ai_kanban::remote::{check_identity, sync_board, Client};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// `rooms::connect` writes its backup next to the default store: pointed into a temp dir here,
/// never next to the real one.
fn sandbox() {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: the only write to the environment in this binary, made once, and every test
        // calls this before anything reads it.
        unsafe { std::env::set_var("AI_KANBAN_DB", dir.path().join("kanban.db")) };
        dir
    });
}

struct Member {
    name: &'static str,
    store: Mutex<Store>,
    pid: i64,
    key: String,
    client: Client,
    _dir: tempfile::TempDir,
}

/// Saves the machine's remote, as `ai-kanban remote` or the settings do. The tests share one
/// sandbox, so one `remote.json`, and its save goes through a fixed temp file: one at a time.
/// Which test's remote it then holds does not matter to them: each only needs it to be not
/// the old relay it is holding a session to.
fn save_remote(url: &str, key: &str) {
    static SAVING: Mutex<()> = Mutex::new(());
    let _one = SAVING.lock().unwrap_or_else(|e| e.into_inner());
    ai_kanban::remote::RemoteConfig::new(url, key).save().unwrap();
}

/// A key already named on the relay, as a machine that used it under `name` left it.
fn named_key(relay: &HttpRelay, name: &str) -> String {
    let db = relay.admin();
    let (id, k) = db.issue_key(&format!("{name} laptop")).unwrap();
    db.name_key(id, name).unwrap();
    k
}

/// A key as a relay admin issues one: bound to no tag until its machine first uses it.
fn key(relay: &HttpRelay, name: &str) -> String {
    relay.admin().issue_key(&format!("{name} laptop")).unwrap().1
}

/// A member connected to `room` through the real connect path. `private_tasks` are filed on a
/// board of their own first: a store's ids count across all its boards, so a busy member's
/// shared tickets carry higher numbers than a quiet member's -- as on any real machine.
///
/// `room` is one per test: connect backups are named by room and second, and the tests share
/// a backup folder and run at once.
fn member(relay: &HttpRelay, room: &str, name: &'static str, key: String, private_tasks: usize) -> Member {
    sandbox();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("kanban.db")).unwrap();
    let private = store.create_board("private").unwrap().id;
    for i in 0..private_tasks {
        store.create_task(private, TaskDraft::new(format!("{name} private {i}"))).unwrap();
    }
    let pid = store.create_board("app").unwrap().id;
    let client = Client::new(&relay.url, &key, Duration::from_secs(5));
    let m = Member { name, store: Mutex::new(store), pid, key, client, _dir: dir };
    let inspection = rooms::inspect(&m.store, &m.client, room).unwrap();
    let plan = rooms::plan(&m.store, inspection, Choice::Board(m.pid)).unwrap();
    rooms::connect(&m.store, &m.client, plan).unwrap();
    m
}

/// One full round, as the background sync runs it. How much it pushed, or why it failed.
fn round(m: &Member) -> Result<usize, String> {
    let s = m.store.lock().unwrap();
    check_identity(&s, &m.client).map_err(|e| format!("{}: identity: {e}", m.name))?;
    let done = sync_board(&s, &m.client, m.pid).map_err(|e| format!("{}: sync: {e}", m.name))?;
    Ok(done.pushed)
}

/// All a member does to move: the new URL and key. The next round finds out the rest.
fn switch(m: &mut Member, relay: &HttpRelay, key: String) {
    m.client = Client::new(&relay.url, &key, Duration::from_secs(5));
    m.key = key;
}

/// Rounds until everyone has what the others have; every failed round is recorded.
fn converge(members: &[&Member], wrong: &mut Vec<String>) {
    for _ in 0..3 {
        for m in members {
            if let Err(e) = round(m) {
                wrong.push(e);
            }
        }
    }
}

fn file(m: &Member, title: &str) {
    m.store.lock().unwrap().create_task(m.pid, TaskDraft::new(title)).unwrap();
}

fn edit(m: &Member, title: &str, patch: TaskPatch) {
    let s = m.store.lock().unwrap();
    let t = tasks(&s, m.pid).into_iter().find(|t| t.title == title).unwrap_or_else(|| panic!("{}: no task {title}", m.name));
    s.update_task(m.pid, t.id, TaskPatch { actor: Actor::Agent, ..patch }).unwrap();
}

fn tasks(s: &Store, pid: i64) -> Vec<Task> {
    s.board(pid, &BoardQuery::board().with_limit(1000).with_status(Status::ALL.to_vec())).unwrap().tasks
}

/// A ticket as one member's board shows it. `owner` is a person's name, whoever looks: the
/// viewer's own name for their own tickets, the room's member list for everyone else's.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    owner: String,
    status: Status,
    priority: Priority,
}

/// Title -> every ticket by that title, so a duplicate shows as two.
fn seen(m: &Member) -> BTreeMap<String, Vec<Seen>> {
    let s = m.store.lock().unwrap();
    let h = s.handles(&[]).unwrap();
    let mut out: BTreeMap<String, Vec<Seen>> = BTreeMap::new();
    for t in tasks(&s, m.pid) {
        let owner = match h.tasks.get(&t.id) {
            Some((tag, _)) if s.is_own_tag(tag).unwrap() => m.name.to_string(),
            Some((tag, _)) => h.names.get(tag).cloned().unwrap_or_else(|| format!("tag {tag}")),
            None => "no identity".into(),
        };
        out.entry(t.title).or_default().push(Seen { owner, status: t.status, priority: t.priority });
    }
    out
}

/// Checks that `m` shows exactly `want` and is not paused, recording each difference.
fn expect(m: &Member, want: &BTreeMap<String, Seen>, wrong: &mut Vec<String>) {
    let got = seen(m);
    for (title, w) in want {
        match got.get(title).map(Vec::as_slice) {
            None | Some([]) => wrong.push(format!("{}: {title:?} is missing", m.name)),
            Some([g]) if g == w => {}
            Some([g]) => wrong.push(format!("{}: {title:?} shows {g:?}, should be {w:?}", m.name)),
            Some(many) => wrong.push(format!("{}: {title:?} is there {} times: {many:?}", m.name, many.len())),
        }
    }
    for title in got.keys().filter(|t| !want.contains_key(*t)) {
        wrong.push(format!("{}: {title:?} should not be there", m.name));
    }
    if let Some(why) = m.store.lock().unwrap().paused().unwrap() {
        wrong.push(format!("{}: sync is paused: {why}", m.name));
    }
}

fn row(title: &str, owner: &str, status: Status, priority: Priority) -> (String, Seen) {
    (title.to_string(), Seen { owner: owner.into(), status, priority })
}

fn done(wrong: Vec<String>) {
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

/// On the old relay: two members, each with tickets of their own and an edit to one of the
/// other's. Benjamin has a busy store (his shared tickets are #21 and #22), Benny a quiet one,
/// so Benny's next ids run into Benjamin's old numbers. With `benny_files` false Benny only
/// works on Benjamin's tickets and files none.
fn mixed_work(old: &HttpRelay, room: &str, benny_files: bool) -> (Member, Member, BTreeMap<String, Seen>) {
    let benjamin = member(old, room, "benjamin", old.key("benjamin", "b1"), 20);
    let benny = member(old, room, "benny", old.key("benny", "b2"), 0);
    file(&benjamin, "benjamin 1");
    file(&benjamin, "benjamin 2");
    if benny_files {
        file(&benny, "benny 1");
        file(&benny, "benny 2");
    }
    let mut wrong = vec![];
    converge(&[&benjamin, &benny], &mut wrong);
    edit(&benny, "benjamin 1", TaskPatch { status: Some(Status::Doing), ..Default::default() });
    if benny_files {
        edit(&benjamin, "benny 1", TaskPatch { priority: Some(Priority::High), ..Default::default() });
    }
    converge(&[&benjamin, &benny], &mut wrong);
    let mut state: BTreeMap<String, Seen> =
        [row("benjamin 1", "benjamin", Status::Doing, Priority::Normal), row("benjamin 2", "benjamin", Status::Backlog, Priority::Normal)].into();
    if benny_files {
        state.extend([row("benny 1", "benny", Status::Backlog, Priority::High), row("benny 2", "benny", Status::Backlog, Priority::Normal)]);
    }
    // The starting point itself has to be right, or the move is not what is being tested.
    expect(&benjamin, &state, &mut wrong);
    expect(&benny, &state, &mut wrong);
    assert!(wrong.is_empty(), "before the move already:\n{}", wrong.join("\n"));
    (benjamin, benny, state)
}

/// Work filed after the move by both has to reach the other: sync goes on. Benny files enough
/// for his new ids to pass Benjamin's old ticket numbers.
fn work_after_the_move(benjamin: &Member, benny: &Member, state: &mut BTreeMap<String, Seen>, wrong: &mut Vec<String>) {
    file(benjamin, "benjamin after");
    state.extend([row("benjamin after", "benjamin", Status::Backlog, Priority::Normal)]);
    for i in 0..25 {
        let title = format!("benny after {i}");
        file(benny, &title);
        state.extend([row(&title, "benny", Status::Backlog, Priority::Normal)]);
    }
    converge(&[benjamin, benny], wrong);
}

#[test]
fn both_move_and_every_ticket_comes_through_once_with_its_owner_and_edits() {
    let old = HttpRelay::start();
    let (mut benjamin, mut benny, mut state) = mixed_work(&old, "both-move", true);
    let new = HttpRelay::start();
    let mut wrong = vec![];
    switch(&mut benjamin, &new, key(&new, "benjamin"));
    switch(&mut benny, &new, key(&new, "benny"));
    converge(&[&benjamin, &benny], &mut wrong);
    expect(&benjamin, &state, &mut wrong);
    expect(&benny, &state, &mut wrong);
    work_after_the_move(&benjamin, &benny, &mut state, &mut wrong);
    expect(&benjamin, &state, &mut wrong);
    expect(&benny, &state, &mut wrong);
    done(wrong);
}

/// #1132: a fresh relay hands out tags by who names a key first. Benny comes first, and his
/// name alone would give him b1, Benjamin's old tag -- but his machine publishes as b2 and
/// says so with every request.
#[test]
fn the_order_keys_are_first_used_in_does_not_change_whose_tickets_are_whose() {
    let old = HttpRelay::start();
    let (mut benjamin, mut benny, mut state) = mixed_work(&old, "order", true);
    let new = HttpRelay::start();
    let mut wrong = vec![];
    switch(&mut benny, &new, key(&new, "benny"));
    converge(&[&benny], &mut wrong);
    switch(&mut benjamin, &new, key(&new, "benjamin"));
    converge(&[&benjamin, &benny], &mut wrong);
    for (m, tag) in [(&benjamin, "b1"), (&benny, "b2")] {
        let has = m.store.lock().unwrap().origin_tag().unwrap();
        if has.as_deref() != Some(tag) {
            wrong.push(format!("{}: publishes as {has:?} on the new relay, should keep {tag}", m.name));
        }
    }
    expect(&benjamin, &state, &mut wrong);
    expect(&benny, &state, &mut wrong);
    work_after_the_move(&benjamin, &benny, &mut state, &mut wrong);
    expect(&benjamin, &state, &mut wrong);
    expect(&benny, &state, &mut wrong);
    done(wrong);
}

/// #1130: Benjamin is on the new relay and working; Benny switches later. Benny's copy does not
/// have Benjamin's latest edits yet, and must not win over them -- whether Benny filed tickets
/// of his own or only worked on Benjamin's.
#[test]
fn an_edit_made_before_the_other_member_has_moved_survives_their_move() {
    for (room, benny_files) in [("gap-edit", true), ("gap-edit-no-tickets", false)] {
        let old = HttpRelay::start();
        let (mut benjamin, mut benny, mut state) = mixed_work(&old, room, benny_files);
        let new = HttpRelay::start();
        let mut wrong = vec![];
        switch(&mut benjamin, &new, key(&new, "benjamin"));
        converge(&[&benjamin], &mut wrong);
        edit(&benjamin, "benjamin 1", TaskPatch { status: Some(Status::Testing), ..Default::default() });
        edit(&benjamin, "benjamin 2", TaskPatch { priority: Some(Priority::Low), ..Default::default() });
        state.get_mut("benjamin 1").unwrap().status = Status::Testing;
        state.get_mut("benjamin 2").unwrap().priority = Priority::Low;
        if benny_files {
            edit(&benjamin, "benny 2", TaskPatch { status: Some(Status::Doing), ..Default::default() });
            state.get_mut("benny 2").unwrap().status = Status::Doing;
        }
        converge(&[&benjamin], &mut wrong);

        switch(&mut benny, &new, key(&new, "benny"));
        converge(&[&benjamin, &benny], &mut wrong);
        expect(&benjamin, &state, &mut wrong);
        expect(&benny, &state, &mut wrong);
        assert!(wrong.is_empty(), "{room}:\n{}", wrong.join("\n"));
    }
}

/// Benjamin never comes back (left the team, laptop gone). Benny's copy of his tickets is
/// what keeps them in the room: a new member gets all of it, and is not handed Benjamin's tag,
/// though no key on the new relay holds it.
#[test]
fn a_member_who_never_comes_back_still_has_their_work_in_the_room() {
    let old = HttpRelay::start();
    let (_benjamin, mut benny, state) = mixed_work(&old, "never-back", true);
    let new = HttpRelay::start();
    let mut wrong = vec![];
    switch(&mut benny, &new, key(&new, "benny"));
    converge(&[&benny], &mut wrong);

    // Bea names her key before her machine uses it; on a fresh relay "Bea" would be b1.
    let bea_key = {
        let db = new.admin();
        let (id, k) = db.issue_key("bea laptop").unwrap();
        let tag = db.name_key(id, "Bea").unwrap().tag;
        if tag == "b1" || tag == "b2" {
            wrong.push(format!("bea was given {tag}, a tag already on tickets in the room"));
        }
        k
    };
    let bea = member(&new, "never-back", "bea", bea_key, 0);
    converge(&[&benny, &bea], &mut wrong);
    expect(&benny, &state, &mut wrong);
    // Bea never met Benjamin: his tickets are his tag's, not anybody's she knows.
    let mut hers = state.clone();
    for t in ["benjamin 1", "benjamin 2"] {
        hers.get_mut(t).unwrap().owner = "tag b1".into();
    }
    expect(&bea, &hers, &mut wrong);
    done(wrong);
}

/// The relay admin forgot a ticket on the old relay. Benny pulled the forget; Benjamin had not
/// when they moved, and still holds the ticket. It must not come back on the new relay.
#[test]
fn a_ticket_forgotten_on_the_old_relay_stays_forgotten_after_the_move() {
    let old = HttpRelay::start();
    let (mut benjamin, mut benny, mut state) = mixed_work(&old, "forgotten", true);
    let mut wrong = vec![];
    let secret = {
        let s = benny.store.lock().unwrap();
        let h = s.handles(&[]).unwrap();
        let t = tasks(&s, benny.pid).into_iter().find(|t| t.title == "benjamin 2").unwrap();
        let (origin, id) = h.tasks[&t.id].clone();
        ai_kanban::sync::protocol::Ref { kind: "task".into(), origin, id }
    };
    old.admin().forget("forgotten", &secret).unwrap();
    converge(&[&benny], &mut wrong);
    let before = state.clone();
    state.remove("benjamin 2");
    expect(&benny, &state, &mut wrong);
    expect(&benjamin, &before, &mut wrong);

    let new = HttpRelay::start();
    switch(&mut benjamin, &new, key(&new, "benjamin"));
    switch(&mut benny, &new, key(&new, "benny"));
    converge(&[&benjamin, &benny], &mut wrong);
    expect(&benjamin, &state, &mut wrong);
    expect(&benny, &state, &mut wrong);
    done(wrong);
}

/// The relay's database copied to a new host: the same relay at another URL, with the same
/// keys. Nothing is sent again and nothing changes.
#[test]
fn the_same_relay_at_a_new_url_is_not_a_move() {
    let old = HttpRelay::start();
    let (mut benjamin, _benny, state) = mixed_work(&old, "same-relay", true);
    let copy = HttpRelay::from_copy(&old);
    let mut wrong = vec![];
    let same_key = benjamin.key.clone();
    switch(&mut benjamin, &copy, same_key);
    match round(&benjamin) {
        Ok(0) => {}
        Ok(n) => wrong.push(format!("benjamin sent {n} change(s) again to the same relay")),
        Err(e) => wrong.push(e),
    }
    expect(&benjamin, &state, &mut wrong);
    done(wrong);
}

/// The new relay has a room by the team's name already, with another team's board in it.
#[test]
fn another_teams_room_under_the_same_name_is_not_merged_into() {
    let old = HttpRelay::start();
    let (mut benjamin, _benny, _) = mixed_work(&old, "taken", true);
    let new = HttpRelay::start();
    let carla = member(&new, "taken", "carla", named_key(&new, "Carla"), 0);
    file(&carla, "carla's own");
    round(&carla).unwrap();

    let mut wrong = vec![];
    switch(&mut benjamin, &new, key(&new, "benjamin"));
    match round(&benjamin) {
        Err(e) if e.contains("someone else's room") => {}
        other => wrong.push(format!("benjamin: should pause on another team's room, got {other:?}")),
    }
    if benjamin.store.lock().unwrap().paused().unwrap().is_none() {
        wrong.push("benjamin: sync should be paused".into());
    }
    round(&carla).unwrap();
    let theirs: Vec<String> = seen(&carla).into_keys().collect();
    if theirs != ["carla's own"] {
        wrong.push(format!("carla's board got benjamin's work: {theirs:?}"));
    }
    done(wrong);
}

/// On the new relay, someone else's machine already holds the tag Benjamin's publishes under.
/// His key is not bound to any other tag -- that would split his work in two -- and sync
/// pauses, saying what to do. Nothing is sent.
#[test]
fn a_tag_taken_on_the_new_relay_pauses_instead_of_taking_another() {
    let old = HttpRelay::start();
    let (mut benjamin, _benny, _) = mixed_work(&old, "tag-taken", true);
    let new = HttpRelay::start();
    new.key("bob", "b1");
    let mut wrong = vec![];
    switch(&mut benjamin, &new, key(&new, "benjamin"));
    match round(&benjamin) {
        Err(e) if e.contains("belongs to another key") => {}
        other => wrong.push(format!("benjamin: should pause on a taken tag, got {other:?}")),
    }
    if benjamin.store.lock().unwrap().paused().unwrap().is_none() {
        wrong.push("benjamin: sync should be paused".into());
    }
    if benjamin.store.lock().unwrap().origin_tag().unwrap().as_deref() != Some("b1") {
        wrong.push("benjamin: keeps publishing as b1".into());
    }
    if !new.admin().rooms().unwrap().is_empty() {
        wrong.push("nothing reached the new relay, not even a room".into());
    }
    done(wrong);
}

/// Benjamin's last edit reached only the old relay, and Benjamin's machine is gone before it
/// moves. Benny's switch syncs with the old relay one last time first, so the edit comes along
/// and reaches the new relay, and with it everyone who joins there.
#[test]
fn a_change_only_the_old_relay_has_is_drained_before_the_switch() {
    let old = HttpRelay::start();
    let (benjamin, mut benny, mut state) = mixed_work(&old, "drain", true);
    edit(&benjamin, "benjamin 2", TaskPatch { status: Some(Status::Blocked), ..Default::default() });
    let mut wrong = vec![];
    if let Err(e) = round(&benjamin) {
        wrong.push(e);
    }
    drop(benjamin);
    state.get_mut("benjamin 2").unwrap().status = Status::Blocked;

    let new = HttpRelay::start();
    let leaving = ai_kanban::remote::RemoteConfig::new(&old.url, &benny.key);
    let drained = ai_kanban::remote::engine::drain(&benny.store.lock().unwrap(), &leaving, Duration::from_secs(5));
    if drained != 1 {
        wrong.push(format!("drained {drained} board(s), should be 1"));
    }
    switch(&mut benny, &new, key(&new, "benny"));
    converge(&[&benny], &mut wrong);
    let bea = member(&new, "drain", "bea", named_key(&new, "Bea"), 0);
    converge(&[&benny, &bea], &mut wrong);
    expect(&benny, &state, &mut wrong);
    if seen(&bea).get("benjamin 2").and_then(|v| v.first()).map(|s| s.status) != Some(Status::Blocked) {
        wrong.push(format!("bea: benjamin 2 should be blocked: {:?}", seen(&bea).get("benjamin 2")));
    }
    done(wrong);
}

/// A session started before the move still holds the old relay's URL and key. Its next round
/// must not read the old relay as a new move and send everything back there; the saved remote
/// is the new relay, which it switches to at its next round.
#[test]
fn a_session_still_on_the_old_relay_does_not_move_the_board_back() {
    let old = HttpRelay::start();
    let (mut benjamin, _benny, state) = mixed_work(&old, "stale", true);
    let stale = benjamin.client.clone();
    let new = HttpRelay::start();
    let mut wrong = vec![];
    let new_key = key(&new, "benjamin");
    switch(&mut benjamin, &new, new_key.clone());
    converge(&[&benjamin], &mut wrong);
    save_remote(&new.url, &new_key);

    let s = benjamin.store.lock().unwrap();
    let on = s.relay_instance().unwrap();
    match check_identity(&s, &stale) {
        Err(e) if e.to_string().contains("moved away from") => {}
        other => wrong.push(format!("the old session should be refused, got {other:?}")),
    }
    if s.relay_instance().unwrap() != on {
        wrong.push("the store went back to the old relay".into());
    }
    if s.paused().unwrap().is_some() {
        wrong.push("sync paused for everyone, though only the old session is behind".into());
    }
    let unsent = s.sync_status(benjamin.pid).unwrap().unwrap().unsent;
    drop(s);
    if unsent != 0 {
        wrong.push(format!("{unsent} change(s) queued again for the old relay"));
    }
    expect(&benjamin, &state, &mut wrong);
    done(wrong);
}

/// Connect `m` to `room` again the documented way after the room was deleted on the relay:
/// disconnect (keeping everything), then connect.
fn reconnect(m: &Member, room: &str) -> Result<(), String> {
    rooms::uncouple(&m.store, Some(&m.client), m.pid, room, false).map_err(|e| format!("{}: disconnect: {e}", m.name))?;
    let inspection = rooms::inspect(&m.store, &m.client, room).map_err(|e| format!("{}: inspect: {e}", m.name))?;
    let plan = rooms::plan(&m.store, inspection, Choice::Board(m.pid)).map_err(|e| format!("{}: plan: {e}", m.name))?;
    rooms::connect(&m.store, &m.client, plan).map_err(|e| format!("{}: connect: {e}", m.name))?;
    Ok(())
}

/// #1130 where it was found: the room deleted on the relay and opened again, everyone
/// reconnecting by hand. Benny, who only ever worked on Benjamin's tickets, reconnects after
/// Benjamin has edited again: his copy goes out with the clocks it was written at, and cannot
/// undo the newer edits.
#[test]
fn reconnecting_to_a_reopened_room_does_not_undo_what_others_changed_since() {
    let relay = HttpRelay::start();
    let (benjamin, benny, mut state) = mixed_work(&relay, "reopened", false);
    relay.admin().delete_room("reopened").unwrap();
    let mut wrong = vec![];
    if let Err(e) = reconnect(&benjamin, "reopened") {
        wrong.push(e);
    }
    converge(&[&benjamin], &mut wrong);
    edit(&benjamin, "benjamin 1", TaskPatch { status: Some(Status::Testing), ..Default::default() });
    state.get_mut("benjamin 1").unwrap().status = Status::Testing;
    converge(&[&benjamin], &mut wrong);

    if let Err(e) = reconnect(&benny, "reopened") {
        wrong.push(e);
    }
    converge(&[&benjamin, &benny], &mut wrong);
    expect(&benjamin, &state, &mut wrong);
    expect(&benny, &state, &mut wrong);
    done(wrong);
}

/// The quick rounds -- before a claim, after a write -- of a session that verified the old
/// relay before another process moved the store. They must neither push to the old relay nor
/// join the room there; the edit made meanwhile reaches the new relay.
#[test]
fn a_verified_session_on_the_old_relay_sends_nothing_there_after_the_store_moved() {
    let old = HttpRelay::start();
    let (mut benjamin, mut benny, mut state) = mixed_work(&old, "stale-quick", true);
    let stale = benjamin.client.clone();
    let mut wrong = vec![];
    let new = HttpRelay::start();
    let new_key = key(&new, "benjamin");
    switch(&mut benjamin, &new, new_key.clone());
    converge(&[&benjamin], &mut wrong);
    save_remote(&new.url, &new_key);
    let on_old = |r: &HttpRelay| r.admin().rooms().unwrap().iter().find(|x| x.name == "stale-quick").map(|x| x.changes);
    let before = on_old(&old);

    edit(&benjamin, "benny 2", TaskPatch { status: Some(Status::Testing), ..Default::default() });
    state.get_mut("benny 2").unwrap().status = Status::Testing;
    {
        let s = benjamin.store.lock().unwrap();
        let _ = ai_kanban::remote::sync_board_within(&s, &stale, benjamin.pid, Duration::from_millis(2500));
        if let Ok(done) = ai_kanban::remote::sync_board_changes(&s, &stale, benjamin.pid) {
            wrong.push(format!("the old session's round went through: {done:?}"));
        }
        if s.needs_rejoin(benjamin.pid).unwrap() {
            wrong.push("the room was joined somewhere and lost its rejoin".into());
        }
    }
    if on_old(&old) != before {
        wrong.push(format!("the old relay got changes: {before:?} -> {:?}", on_old(&old)));
    }
    converge(&[&benjamin], &mut wrong);
    switch(&mut benny, &new, key(&new, "benny"));
    converge(&[&benjamin, &benny], &mut wrong);
    expect(&benjamin, &state, &mut wrong);
    expect(&benny, &state, &mut wrong);
    done(wrong);
}
