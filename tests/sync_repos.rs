//! Repos on shared tasks. A checkout's path and local id differ on every machine, so a task's
//! repos travel as keys: the normalized git remote, or the name when there is none. Which
//! checkouts a machine has is its own business and must never take a repo off the team's tasks.

mod common;
use common::sync::*;

use ai_kanban::core::model::*;
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::render;
use std::path::{Path, PathBuf};

/// A directory that looks like a git checkout with `url` as its origin. No git needed: core
/// reads `.git/config` itself.
fn checkout(root: &Path, dir: &str, url: Option<&str>) -> PathBuf {
    let p = root.join(dir);
    std::fs::create_dir_all(p.join(".git")).unwrap();
    let config = match url {
        Some(u) => format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {u}\n"),
        None => "[core]\n\tbare = false\n".to_string(),
    };
    std::fs::write(p.join(".git/config"), config).unwrap();
    p
}

fn link(m: &Member, task: i64, repo: i64) {
    m.store.update_task(m.pid, task, TaskPatch { repos: Some(vec![repo]), actor: Actor::Agent, ..Default::default() }).unwrap();
}

fn repo_names(m: &Member, task: i64) -> Vec<String> {
    m.store.task_repos(m.pid, task).unwrap().into_iter().map(|r| r.name).collect()
}

#[test]
fn a_repo_links_to_each_members_own_checkout_of_the_same_remote() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    // Same repository, cloned over SSH on one machine and HTTPS on the other, named differently.
    let br = benny.store.add_repo(benny.pid, &checkout(a.path(), "svc", Some("git@github.com:Acme/svc.git")), None, Actor::User).unwrap();
    let vr = volker.store.add_repo(volker.pid, &checkout(b.path(), "service", Some("https://github.com/acme/svc")), None, Actor::User).unwrap();
    let t = benny.store.create_task(benny.pid, TaskDraft::new("fix svc")).unwrap();
    link(&benny, t.id, br.id);
    converge(&relay, &[&benny, &volker]);

    let vt = task_by_title(&volker, "fix svc");
    assert_eq!(volker.store.task_repos(volker.pid, vt.id).unwrap().into_iter().map(|r| r.id).collect::<Vec<_>>(), [vr.id]);
    assert_eq!(repo_names(&volker, vt.id), ["service"], "his own checkout, under his own name");
    assert_eq!(volker.store.unmapped_repos(vt.id).unwrap(), Vec::<String>::new());
}

#[test]
fn a_repo_with_no_checkout_here_is_kept_shown_and_never_dropped_by_a_local_edit() {
    let a = tempfile::tempdir().unwrap();
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let br = benny.store.add_repo(benny.pid, &checkout(a.path(), "svc", Some("git@github.com:acme/svc.git")), None, Actor::User).unwrap();
    let t = benny.store.create_task(benny.pid, TaskDraft::new("fix svc")).unwrap();
    link(&benny, t.id, br.id);
    converge(&relay, &[&benny, &volker]);

    let vt = task_by_title(&volker, "fix svc");
    assert!(repo_names(&volker, vt.id).is_empty());
    assert_eq!(volker.store.unmapped_repos(vt.id).unwrap(), ["svc"]);
    let shown = render::task_detail(&volker.store.task_detail(volker.pid, vt.id).unwrap());
    assert!(shown.contains("svc  (no checkout on this machine)"), "{shown}");
    let line = render::board(&volker.store.board(volker.pid, &BoardQuery::board()).unwrap());
    assert!(line.contains("[svc]"), "listed, not 'no repo set': {line}");

    // Volker edits the task; Benny's link must survive.
    volker.store.update_task(volker.pid, vt.id, TaskPatch { title: Some("fix svc timeouts".into()), actor: Actor::Agent, ..Default::default() }).unwrap();
    converge(&relay, &[&benny, &volker]);
    assert_eq!(repo_names(&benny, t.id), ["svc"]);
}

#[test]
fn a_checkout_added_later_takes_over_the_links_made_before_it() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let br = benny.store.add_repo(benny.pid, &checkout(a.path(), "svc", Some("git@github.com:acme/svc.git")), None, Actor::User).unwrap();
    let t = benny.store.create_task(benny.pid, TaskDraft::new("fix svc")).unwrap();
    link(&benny, t.id, br.id);
    converge(&relay, &[&benny, &volker]);

    let vr = volker.store.add_repo(volker.pid, &checkout(b.path(), "svc-clone", Some("https://github.com/acme/svc.git")), None, Actor::User).unwrap();
    let vt = task_by_title(&volker, "fix svc");
    assert_eq!(volker.store.task_repos(volker.pid, vt.id).unwrap()[0].id, vr.id);
    assert!(volker.store.unmapped_repos(vt.id).unwrap().is_empty());
    assert_eq!(volker.store.flush().unwrap(), 0, "the set did not change, so nothing is published");
}

#[test]
fn taking_a_repo_off_my_board_does_not_take_it_off_the_teams_tasks() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let br = benny.store.add_repo(benny.pid, &checkout(a.path(), "svc", Some("git@github.com:acme/svc.git")), None, Actor::User).unwrap();
    volker.store.add_repo(volker.pid, &checkout(b.path(), "svc", Some("git@github.com:acme/svc.git")), None, Actor::User).unwrap();
    let t = benny.store.create_task(benny.pid, TaskDraft::new("fix svc")).unwrap();
    link(&benny, t.id, br.id);
    converge(&relay, &[&benny, &volker]);

    benny.store.remove_repo(benny.pid, br.id, Actor::User).unwrap();
    converge(&relay, &[&benny, &volker]);
    let vt = task_by_title(&volker, "fix svc");
    assert_eq!(repo_names(&volker, vt.id), ["svc"], "volker's link stands");
    assert_eq!(benny.store.unmapped_repos(t.id).unwrap(), ["svc"], "benny still sees it, as not checked out here");

    // A deliberate change to the task's repos does travel.
    volker.store.update_task(volker.pid, vt.id, TaskPatch { repos: Some(vec![]), actor: Actor::Agent, ..Default::default() }).unwrap();
    converge(&relay, &[&benny, &volker]);
    assert!(benny.store.unmapped_repos(t.id).unwrap().is_empty());
}

#[test]
fn a_checkout_without_a_remote_travels_by_name() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let br = benny.store.add_repo(benny.pid, &checkout(a.path(), "notes", None), None, Actor::User).unwrap();
    volker.store.add_repo(volker.pid, &checkout(b.path(), "notes", None), None, Actor::User).unwrap();
    let t = benny.store.create_task(benny.pid, TaskDraft::new("write it down")).unwrap();
    link(&benny, t.id, br.id);
    converge(&relay, &[&benny, &volker]);
    let vt = task_by_title(&volker, "write it down");
    assert_eq!(repo_names(&volker, vt.id), ["notes"]);
}

#[test]
fn repos_on_a_private_board_never_touch_sync_state() {
    let a = tempfile::tempdir().unwrap();
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    benny.store.flush().unwrap();
    benny.store.outbox_sent(benny.pid, i64::MAX).unwrap();
    let private = benny.store.create_board("private").unwrap().id;
    let r = benny.store.add_repo(private, &checkout(a.path(), "secret", Some("git@example.com:me/secret.git")), None, Actor::User).unwrap();
    let t = benny.store.create_task(private, TaskDraft::new("t")).unwrap();
    benny.store.update_task(private, t.id, TaskPatch { repos: Some(vec![r.id]), actor: Actor::Agent, ..Default::default() }).unwrap();
    benny.store.remove_repo(private, r.id, Actor::User).unwrap();
    assert_eq!(benny.store.flush().unwrap(), 0);
    let unmapped: i64 = benny.store.conn.query_row("SELECT COUNT(*) FROM sync_unmapped_repos", [], |r| r.get(0)).unwrap();
    assert_eq!(unmapped, 0);
}
