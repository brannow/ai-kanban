use ai_kanban::core::model::Actor;
use ai_kanban::core::Store;

#[test]
fn a_board_allows_every_profile_until_one_is_refused() {
    let s = Store::open_in_memory().unwrap();
    let a = s.create_board("BMUKN").unwrap();
    let b = s.create_board("Private").unwrap();
    assert!(s.denied_profiles(a.id).unwrap().is_empty());

    s.set_profile_allowed(b.id, "claude-work", false, Actor::User).unwrap();
    assert_eq!(s.denied_profiles(b.id).unwrap(), ["claude-work"]);
    // Set on one board only.
    assert!(s.denied_profiles(a.id).unwrap().is_empty());

    s.set_profile_allowed(b.id, "claude-work", true, Actor::User).unwrap();
    assert!(s.denied_profiles(b.id).unwrap().is_empty());
}

#[test]
fn resending_the_current_setting_records_nothing() {
    let s = Store::open_in_memory().unwrap();
    let b = s.create_board("Private").unwrap();
    s.set_profile_allowed(b.id, "claude-work", false, Actor::User).unwrap();
    let before = s.change_cursor().unwrap();
    s.set_profile_allowed(b.id, "claude-work", false, Actor::User).unwrap();
    s.set_profile_allowed(b.id, "claude", true, Actor::User).unwrap();
    assert_eq!(s.change_cursor().unwrap(), before);
}

#[test]
fn forgetting_a_board_takes_its_settings_with_it() {
    let s = Store::open_in_memory().unwrap();
    let b = s.create_board("Private").unwrap();
    s.set_profile_allowed(b.id, "claude-work", false, Actor::User).unwrap();
    s.forget_board(b.id).unwrap();
    // A new board can reuse the id; it must not inherit the old one's refusals.
    let again = s.create_board("Private").unwrap();
    assert!(s.denied_profiles(again.id).unwrap().is_empty());
}
