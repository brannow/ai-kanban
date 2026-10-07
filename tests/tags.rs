//! Task tags.
//!
//! Per note #17, a new field on tasks has several homes and most of them fail SILENTLY with
//! a green suite, because every existing test predates the field. These are the assertions
//! that would otherwise be missing: a patch round trip, export/import, merge, and the FTS
//! index the migration has to rebuild by hand.

use ai_kanban::core::model::*;
use ai_kanban::core::recall::{RecallQuery, DEFAULT_LIMIT};
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::Store;

fn fixture(name: &str) -> (Store, i64) {
    let s = Store::open_in_memory().unwrap();
    let dir = std::env::temp_dir().join(format!("aik-tags-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pid = s.resolve_project(&dir).unwrap().project.id;
    (s, pid)
}

fn tags(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn tags_survive_a_write_and_a_patch_that_changes_nothing_else() {
    // `TaskPatch::is_empty` lists every field by hand, and `update_task` early-returns on
    // it. A field missing from that list makes a patch changing ONLY that field look like
    // no change at all: writes nothing, reports success. That is note #17's home 2, and it
    // has shipped broken here once before.
    let (s, pid) = fixture("patch");
    let t = s.create_task(pid, TaskDraft {
        tags: tags(&["frontend", "in review"]),
        ..TaskDraft::new("the redirect loop")
    }).unwrap();
    assert_eq!(s.task_tags(pid, t.id).unwrap(), tags(&["frontend", "in review"]));

    s.update_task(pid, t.id, TaskPatch {
        tags: Some(tags(&["waiting-on-vendor"])),
        ..Default::default()
    }).unwrap();
    assert_eq!(s.task_tags(pid, t.id).unwrap(), tags(&["waiting-on-vendor"]),
        "a patch touching only tags must not be mistaken for an empty patch");

    // Untouched by a patch about something else -- the update rewrites every column, so a
    // field read back wrong here would silently wipe labels on every status change.
    s.update_task(pid, t.id, TaskPatch {
        status: Some(Status::Doing), ..Default::default()
    }).unwrap();
    assert_eq!(s.task_tags(pid, t.id).unwrap(), tags(&["waiting-on-vendor"]));

    // Cleared explicitly, which the adapters expose as an empty value.
    s.update_task(pid, t.id, TaskPatch { tags: Some(vec![]), ..Default::default() }).unwrap();
    assert!(s.task_tags(pid, t.id).unwrap().is_empty());
}

#[test]
fn a_tag_is_findable_by_typing_it() {
    // tasks_fts is an EXTERNAL-CONTENT table, so adding a column meant dropping it,
    // recreating it with three columns, rewriting all three sync triggers and rebuilding.
    // Every one of those can be got wrong in a way that returns zero rows rather than an
    // error -- an empty FTS index looks exactly like "nothing matched".
    let (s, pid) = fixture("recall");
    s.create_task(pid, TaskDraft {
        tags: tags(&["waiting-on-vendor"]),
        ..TaskDraft::new("chase the SSL certificate")
    }).unwrap();

    let r = s.recall(&RecallQuery { text: "waiting-on-vendor", project_id: Some(pid), limit: DEFAULT_LIMIT }).unwrap();
    assert_eq!(r.hits.len(), 1, "a tag has to be findable, or indexing it bought nothing");
    assert_eq!(r.hits[0].title, "chase the SSL certificate");

    // And the triggers still maintain the index after the column list changed: a stale row
    // here would keep answering for a tag that has been removed.
    let t = s.task_opt(pid, r.hits[0].id).unwrap().unwrap();
    s.update_task(pid, t.id, TaskPatch { tags: Some(vec![]), ..Default::default() }).unwrap();
    let r = s.recall(&RecallQuery { text: "waiting-on-vendor", project_id: Some(pid), limit: DEFAULT_LIMIT }).unwrap();
    assert!(r.hits.is_empty(), "the index must forget a removed tag: {:?}", r.hits);

    // The title still finds it, so the rebuild did not cost the columns that already worked.
    let r = s.recall(&RecallQuery { text: "certificate", project_id: Some(pid), limit: DEFAULT_LIMIT }).unwrap();
    assert_eq!(r.hits.len(), 1);
}

#[test]
fn tags_are_shown_where_the_agent_committed_to_one_task_and_nowhere_cheaper() {
    // The asymmetry is the design, not an oversight. The board listing is the most
    // expensive space in the product and is paid for on every task_add; tags buy an agent
    // nothing there, because carrying no semantics it must honour is exactly what makes
    // them safe to have at all. task_show is the response allowed to cost more.
    let (s, pid) = fixture("render");
    let t = s.create_task(pid, TaskDraft {
        tags: tags(&["frontend"]),
        ..TaskDraft::new("the redirect loop")
    }).unwrap();

    let board = ai_kanban::render::board(&s.board(pid, &BoardQuery::board()).unwrap());
    assert!(!board.contains("frontend"), "not on the board line: {board}");

    let detail = s.task_detail(pid, t.id).unwrap();
    let shown = ai_kanban::render::task_detail(&detail);
    assert!(shown.contains("frontend"), "task_show is where they belong: {shown}");
}

#[test]
fn tags_survive_export_and_import() {
    // Note #17 home 3. The export format is deliberately COMPLETE rather than lossy, and a
    // new column not carried means `backup` -> `import` silently strips it.
    let (s, pid) = fixture("transfer");
    s.create_task(pid, TaskDraft {
        tags: tags(&["frontend", "in review"]),
        ..TaskDraft::new("the redirect loop")
    }).unwrap();
    let dump = s.export(&[]).unwrap();

    let target = Store::open_in_memory().unwrap();
    target.import(&dump).unwrap();
    let tpid = target.all_projects().unwrap()[0].id;
    let imported = target.board(tpid, &BoardQuery::board()).unwrap();
    assert_eq!(imported.tasks.len(), 1);
    assert_eq!(target.task_tags(tpid, imported.tasks[0].id).unwrap(),
        tags(&["frontend", "in review"]));
}

#[test]
fn a_list_is_read_the_same_whether_typed_as_csv_or_json() {
    // The schemas ask for comma separated, and agents send a JSON array anyway. Nine tasks
    // on a real board stored `["broken-in-v10","search","solr"]` verbatim, and then read
    // back as the tags `["broken-in-v10"`, `"search"` and `"solr"]`, so filtering for
    // broken-in-v10 missed them. The same parser reads the store, so those rows heal on read.
    use ai_kanban::core::note::split_list;
    let want = tags(&["broken-in-v10", "search", "solr"]);
    assert_eq!(split_list("broken-in-v10, search,solr"), want);
    assert_eq!(split_list(r#"["broken-in-v10","search","solr"]"#), want);
    assert_eq!(split_list(r#" [ "broken-in-v10", " search", "solr,"] "#), want);
    assert_eq!(split_list(r#"["a,b"]"#), tags(&["a", "b"]), "the store's separator stays a separator");
    assert!(split_list("").is_empty() && split_list("[]").is_empty(), "both still mean clear");
    assert_eq!(split_list("[wip"), tags(&["[wip"]), "not JSON: taken as typed, not dropped");
}
