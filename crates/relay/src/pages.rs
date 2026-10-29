//! Everything the admin commands say at length: the help pages, and the message to send a
//! person after their key was made or replaced.
//!
//! In one place, and in one form, so the overview, a command's page and what a command prints
//! cannot drift apart. Each names commands the way the admin typed them: `ai-kanban-relay`, or
//! whatever a wrapper says it is called (`AI_KANBAN_RELAY_AS`, set by deploy/relay/kanban).
//! The relay cannot know its public URL behind a proxy, so the wrapper passes that too
//! (`AI_KANBAN_RELAY_URL`).

/// How the admin runs these commands.
pub fn prog() -> String {
    std::env::var("AI_KANBAN_RELAY_AS").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "ai-kanban-relay".into())
}

fn public_url() -> String {
    std::env::var("AI_KANBAN_RELAY_URL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "<the relay's URL>".into())
}

const RULE: &str = "----------------------------------------------------------------------";

pub const TOPICS: &[&str] = &["key", "room", "forget", "snapshot", "health"];

/// The overview: every command, one line each.
pub fn overview() -> String {
    let p = prog();
    // Starting the relay is not something a wrapper's user does by hand.
    let run = if p == "ai-kanban-relay" {
        "\
Run it:
    ai-kanban-relay [--listen ADDR] [--snapshot-every DUR] [--keep N] [--snapshot-dir DIR] [--debug]
        ADDR defaults to 127.0.0.1:7373; the container passes 0.0.0.0:7373.
        DUR is like 30m, 6h or 1d. Snapshots go to <db dir>/snapshots by default.
        --debug (or AI_KANBAN_RELAY_DEBUG=1) logs every request, response and stream event,
        bodies included. For watching what clients do, not for a relay left running.
    Every command takes --db <file> (default: $AI_KANBAN_SERVER_DB, else /data/server.db).
    It never falls back to the personal store.

"
    } else {
        ""
    };
    format!(
        "\
ai-kanban relay -- administration

{run}Keys -- one per machine; people name themselves:
    {p} key add <label>              a key for someone, with a message to send them
    {p} key list
    {p} key rotate <tag|id>          replace a lost or leaked key
    {p} key revoke <tag|id>
    {p} key label <tag|id> <label>

Rooms:
    {p} room list
    {p} room delete <room> [--yes]
    {p} room cleanup
    {p} forget <room> task|note <id>-<tag> [--yes]

Upkeep:
    {p} snapshot
    {p} health

Each in detail, with examples:  {p} help <{topics}>
                           or:  {p} <command> --help
",
        topics = TOPICS.join("|"),
    )
}

/// One command's page, or none for a name that is not a command.
pub fn page(topic: &str) -> Option<String> {
    let p = prog();
    let text = match topic {
        "key" => format!(
            "\
{p} key -- one key per machine

The relay hands out keys; people name themselves. A key is given out with a label -- your
note of who has it, which the team never sees -- and gets its tag the first time it is used:
from the name its user picks, or a generated one like brave-otter. They can change the name
any time; the tag stays, and with it the handles of everything they published (#12-v1).
Names may repeat: the tag tells two people apart. A name is what someone says they are
called; the tag and your label are what you can rely on.

Every machine needs a key of its own: task numbers are counted per machine, and two
machines on one key would hand out the same ones.

    {p} key add <label>
        A new key, labelled with who it is for. Prints a message to send them.

    {p} key list
        Every key: id, tag, the name its user chose, your label, and when it was last used.

    {p} key rotate <tag|id>
        For a lost or leaked key: a new key with the same tag, name and label, and the old
        one stops working at once. Prints a message to send them.

    {p} key revoke <tag|id>
        Stop a key for good. Its tag is not given out again.

    {p} key label <tag|id> <label>
        Change your label for a key.

A key not used yet has no tag; name it by its id, from key list.

Examples:
    {p} key add volker laptop
    {p} key rotate v1
    {p} key revoke 3
"
        ),
        "room" => format!(
            "\
{p} room -- the shared boards

A room is created when the first board connects to it, and holds every change its members
published. Each member keeps their own copy, so deleting a room ends the sharing, not the
boards.

    {p} room list
        Every room, how many changes it holds, and who is connected.

    {p} room delete <room> [--yes]
        Delete the room and everything in it. Without --yes it only says what would go.

    {p} room cleanup
        Remove the rooms nobody has been connected to for 7 days. The relay does this by
        itself every hour; this is for now.

Examples:
    {p} room delete sbahn           a dry run: what would go
    {p} room delete sbahn --yes
"
        ),
        "forget" => format!(
            "\
{p} forget -- remove one task or note from a room for good

For what must not stay shared, like a pasted secret. Deletes every change about it from the
room, and it disappears from every member's board on their next sync. A member who was
offline cannot send it back.

    {p} forget <room> task|note <id>-<tag> [--yes]
        Without --yes it only says what would go. The id is the one the board shows: #122-v1
        is task 122-v1.

Example:
    {p} forget sbahn task 122-v1 --yes
"
        ),
        "snapshot" => format!(
            "\
{p} snapshot -- a copy of the relay's database, now

    {p} snapshot [--snapshot-dir DIR]
        Writes a consistent copy as server-<time>.db into the snapshots folder next to the
        database (data/snapshots beside the compose file). Safe while the relay runs.

The relay also takes them by itself, every --snapshot-every, keeping the newest --keep.
"
        ),
        "health" => format!(
            "\
{p} health -- whether the relay answers

    {p} health [--listen ADDR]
        Prints ok and exits 0 if the relay answers on ADDR (default 127.0.0.1:7373). The
        container's health check runs this.
"
        ),
        _ => return None,
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_in_the_overview_has_a_page_and_every_page_is_in_the_overview() {
        let all = overview();
        for t in TOPICS {
            assert!(page(t).is_some_and(|p| p.starts_with(&format!("ai-kanban-relay {t} -- "))), "{t}");
            assert!(all.contains(&format!("ai-kanban-relay {t}")), "{t} missing from the overview");
        }
        assert!(page("nope").is_none());
    }
}

/// Said after `key add`: the message for whoever the key is for, ready to send.
pub fn welcome(id: i64, label: &str, key: &str) -> String {
    let (p, url) = (prog(), public_url());
    format!(
        "\
Key {id} issued for \"{label}\" (the label is only for you).

Send this to them. The key is shown only this once; lost, it can only be replaced
({p} key rotate {id}).
{RULE}
ai-kanban: team relay

1. In Claude Code, install the plugin:
     /plugin marketplace add brannow/ai-kanban
     /plugin install ai-kanban@ai-kanban

2. Enter in its settings (asked on install, or /plugin -> ai-kanban -> Configure):
     Team relay URL   {url}
     Team relay key   {key}
     Your name        how the team sees you. Optional: left empty you get one like
                      brave-otter, and you can change it any time.

   One key per machine. For another machine, ask for a new one.

3. Ask Claude to connect a board to a room (\"connect this board to room <name>\").
   Everyone who connects a board to the same room shares it.
{RULE}
"
    )
}

/// Said after `key rotate`. The machine keeps its own remote over the plugin's settings
/// (src/remote/config.rs), so the new key has to be set on the machine, and the settings
/// changed as well, or every session reports the two disagreeing.
pub fn rotated(label: &str, tag: Option<&str>, key: &str) -> String {
    let url = public_url();
    let which = tag.map(|t| format!(" (tag {t})")).unwrap_or_default();
    format!(
        "\
New key for \"{label}\"{which}. The old key stopped working just now; tag and name stay.

Send this to them. The key is shown only this once.
{RULE}
ai-kanban: your team relay key was replaced

On the machine that used it{which}:

1. Ask Claude to run this (or run it in a terminal, after /ai-kanban:install-cli):
     ai-kanban remote {url} --key {key}

2. If the old key is in the plugin's settings (/plugin -> ai-kanban -> Configure),
   put the new one there too.

Nothing is lost: what was done meanwhile is sent once the new key is in place.
{RULE}
"
    )
}
