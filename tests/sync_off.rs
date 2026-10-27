//! `AI_KANBAN_SYNC=off` for one run: the board says so instead of silently going stale. Alone in
//! its binary, because the variable is process-wide.

use ai_kanban::core::model::*;
use ai_kanban::core::Store;

#[test]
fn a_run_with_sync_off_says_so_in_the_header_and_starts_no_syncer() {
    // SAFETY: the only test in this binary.
    unsafe {
        std::env::set_var("AI_KANBAN_SYNC", "off");
        std::env::set_var("AI_KANBAN_SERVER_URL", "http://127.0.0.1:9");
        std::env::set_var("AI_KANBAN_SERVER_KEY", "akb_x");
    }
    let s = Store::open_in_memory().unwrap();
    s.set_origin_tag("b1").unwrap();
    let pid = s.create_board("sbb").unwrap().id;
    s.connect_board(pid, "sbahn").unwrap();
    let text = ai_kanban::render::board(&s.board(pid, &BoardQuery::board()).unwrap());
    assert!(text.lines().nth(1).unwrap().contains("sync off for this run"), "{text}");
    assert!(ai_kanban::remote::daemon::Syncer::start().is_none(), "nothing touches the network");
}
