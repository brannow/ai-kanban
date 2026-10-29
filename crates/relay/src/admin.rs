//! `ai-kanban-relay ...` -- running the relay and administering it.
//!
//! Admin commands open the database directly instead of going through the API. They are run
//! with `docker exec` inside the container, so whoever can run them already holds the file;
//! an admin API would add a second way in that needs its own protection.

use super::store::{Removal, ServerDb};
use super::pages::{self, prog};
use super::{Options, EMPTY_ROOM_AGE};
use ai_kanban_protocol::{Handle, Ref};
use std::path::PathBuf;

struct Args {
    pos: Vec<String>,
    db: Option<PathBuf>,
    listen: Option<String>,
    snapshot_every: Option<String>,
    snapshot_dir: Option<PathBuf>,
    keep: Option<String>,
    yes: bool,
    debug: bool,
    help: bool,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        pos: vec![], db: None, listen: None,
        snapshot_every: None, snapshot_dir: None, keep: None, yes: false, debug: false, help: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--db" => a.db = Some(PathBuf::from(value("--db")?)),
            "--listen" => a.listen = Some(value("--listen")?),
            "--snapshot-every" => a.snapshot_every = Some(value("--snapshot-every")?),
            "--snapshot-dir" => a.snapshot_dir = Some(PathBuf::from(value("--snapshot-dir")?)),
            "--keep" => a.keep = Some(value("--keep")?),
            "--yes" => a.yes = true,
            "--debug" => a.debug = true,
            // A flag, not a word among the arguments: `key rotate volker --help` must show the
            // page, not rotate a user called "help".
            "-h" | "--help" => a.help = true,
            s if s.starts_with("--") => return Err(format!("unknown flag {s}")),
            _ => a.pos.push(arg),
        }
    }
    Ok(a)
}

impl Args {
    fn arg(&self, n: usize) -> Option<&str> {
        self.pos.get(n).map(String::as_str)
    }
    fn db_path(&self) -> PathBuf {
        self.db.clone().unwrap_or_else(ServerDb::default_path)
    }
    fn open(&self) -> Result<ServerDb, Box<dyn std::error::Error>> {
        Ok(ServerDb::open(&self.db_path())?)
    }
    fn snapshot_dir(&self) -> PathBuf {
        self.snapshot_dir.clone().unwrap_or_else(|| {
            self.db_path().parent().map(|p| p.join("snapshots")).unwrap_or_else(|| PathBuf::from("snapshots"))
        })
    }
}

fn usage(line: &str) -> Box<dyn std::error::Error> {
    let topic = line.split(' ').next().unwrap_or_default();
    format!("usage: {} {line}   (more: {} help {topic})", prog(), prog()).into()
}

/// A command's page, or the overview.
fn help(topic: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    match topic {
        None => print!("{}", pages::overview()),
        Some(t) => match pages::page(t) {
            Some(text) => print!("{text}"),
            None => return Err(format!("no command {t:?}. One of: {}", pages::TOPICS.join(", ")).into()),
        },
    }
    Ok(())
}

/// The error for a word that is not one of a command's own, with that command's page.
fn unknown(topic: &str, word: &str) -> Box<dyn std::error::Error> {
    format!("unknown {topic} command {word:?}\n\n{}", pages::page(topic).unwrap_or_default()).into()
}

/// `30m`, `6h`, `1d`, or plain seconds.
fn duration(raw: &str) -> Result<u64, String> {
    let (num, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let n: u64 = num.parse().map_err(|_| format!("duration {raw:?}: use 30m, 6h or 1d"))?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(format!("duration {raw:?}: use 30m, 6h or 1d")),
    };
    if n == 0 {
        return Err("duration must be more than 0".into());
    }
    Ok(n * mult)
}

fn ago(ts: Option<i64>) -> String {
    match ts {
        None => "never".into(),
        Some(t) => {
            let s = (super::store::now() - t).max(0);
            match s {
                0..=59 => format!("{s}s ago"),
                60..=3599 => format!("{}m ago", s / 60),
                3600..=86399 => format!("{}h ago", s / 3600),
                _ => format!("{}d ago", s / 86400),
            }
        }
    }
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let a = args()?;
    if a.help {
        return help(a.arg(0).filter(|t| pages::TOPICS.contains(t)));
    }
    match a.arg(0) {
        None => {
            let listen = a.listen.as_deref().unwrap_or("127.0.0.1:7373");
            let opts = Options {
                listen: listen.parse().map_err(|_| format!("--listen {listen:?}: expected host:port, like 0.0.0.0:7373"))?,
                db: a.db_path(),
                snapshot_every: a.snapshot_every.as_deref().map(duration).transpose()?,
                snapshot_dir: a.snapshot_dir(),
                keep: match &a.keep {
                    Some(k) => k.parse().map_err(|_| format!("--keep {k:?}: expected a number"))?,
                    None => 28,
                },
                // The variable too, so compose can switch it in `environment:` without
                // repeating the image's command.
                debug: a.debug || std::env::var("AI_KANBAN_RELAY_DEBUG").is_ok_and(|v| !v.is_empty() && v != "0"),
            };
            super::serve(opts).await
        }
        Some("help") => help(a.arg(1)),
        Some("key") => key(&a),
        Some("room") => room(&a),
        Some("forget") => forget(&a),
        Some("snapshot") => {
            let p = super::take_snapshot(&a.open()?, &a.snapshot_dir(), usize::MAX)?;
            println!("snapshot written: {}", p.display());
            Ok(())
        }
        Some("health") => health(a.listen.as_deref().unwrap_or("127.0.0.1:7373")),
        Some(other) => Err(format!("unknown command {other:?}\n\n{}", pages::overview()).into()),
    }
}

fn key(a: &Args) -> Result<(), Box<dyn std::error::Error>> {
    // Alone, it is the page -- listing told nobody what else the command does -- and that
    // needs no database.
    if matches!(a.arg(1), None | Some("help")) {
        return help(Some("key"));
    }
    // A label may be several words without quotes: `key add volker laptop`.
    let rest = |from: usize| a.pos.get(from..).map(|w| w.join(" ")).unwrap_or_default();
    let db = a.open()?;
    match a.arg(1) {
        Some("add") => {
            let label = rest(2);
            if label.trim().is_empty() {
                return Err(usage("key add <label>   (who it is for, like: volker laptop)"));
            }
            let (id, key) = db.issue_key(&label)?;
            print!("{}", pages::welcome(id, &label, &key));
        }
        Some("list") => {
            let keys = db.keys()?;
            if keys.is_empty() {
                println!("no keys yet. {} key add <label>", prog());
            } else {
                println!("{:>4}  {:<8} {:<20} {:<24} {}", "id", "tag", "name (theirs)", "label (yours)", "");
            }
            for k in keys {
                let state = match k.revoked_at {
                    Some(_) => "revoked".to_string(),
                    None if k.tag.is_none() => "not used yet".to_string(),
                    None => format!("last used {}", ago(k.last_used_at)),
                };
                let (tag, name) = (k.tag.unwrap_or_else(|| "-".into()), k.name.unwrap_or_else(|| "-".into()));
                println!("{:>4}  {:<8} {:<20} {:<24} {}", k.id, tag, name, k.label, state);
            }
        }
        Some("revoke") => {
            let id = db.key_id(a.arg(2).ok_or_else(|| usage("key revoke <tag|id>"))?)?;
            db.revoke_key(id)?;
            println!("key {id} revoked. Its tag is not given out again.");
        }
        Some("rotate") => {
            let id = db.key_id(a.arg(2).ok_or_else(|| usage("key rotate <tag|id>"))?)?;
            let label = db.keys()?.into_iter().find(|k| k.id == id).map(|k| k.label).unwrap_or_default();
            let (tag, key) = db.rotate_key(id)?;
            print!("{}", pages::rotated(&label, tag.as_deref(), &key));
        }
        Some("label") => {
            let id = db.key_id(a.arg(2).ok_or_else(|| usage("key label <tag|id> <label>"))?)?;
            let label = rest(3);
            db.label_key(id, &label)?;
            println!("key {id} is now labelled \"{}\".", label.trim());
        }
        Some(other) => return Err(unknown("key", other)),
        None => unreachable!("the page, above"),
    }
    Ok(())
}

fn room(a: &Args) -> Result<(), Box<dyn std::error::Error>> {
    // Alone, it is the page -- listing told nobody what else the command does -- and that
    // needs no database.
    if matches!(a.arg(1), None | Some("help")) {
        return help(Some("room"));
    }
    let db = a.open()?;
    match a.arg(1) {
        Some("list") => {
            let rooms = db.rooms()?;
            if rooms.is_empty() {
                println!("no rooms yet. They are created by connecting a board to one.");
            }
            for r in rooms {
                let members: Vec<String> = r.members.iter().map(|m| format!("{} {}", m.tag, m.user)).collect();
                let members = if members.is_empty() { "nobody connected".into() } else { members.join(", ") };
                println!("{:<20} {:>6} changes   {}", r.name, r.changes, members);
            }
        }
        Some("delete") => {
            let name = a.arg(2).ok_or_else(|| usage("room delete <room> [--yes]"))?;
            let Removal { changes, members } = db.room_removal(name)?;
            if !a.yes {
                println!("would delete room {name}: {changes} changes, {members} connected boards lose their room.");
                println!("Members keep their local copies. Dry run -- repeat with --yes.");
                return Ok(());
            }
            db.delete_room(name)?;
            println!("room {name} deleted ({changes} changes).");
        }
        Some("cleanup") => {
            let gone = db.remove_empty_rooms(EMPTY_ROOM_AGE)?;
            if gone.is_empty() {
                println!("no room has been empty for 7 days.");
            } else {
                println!("removed: {}", gone.join(", "));
            }
        }
        Some(other) => return Err(unknown("room", other)),
        None => unreachable!("the page, above"),
    }
    Ok(())
}

fn forget(a: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let line = "forget <room> task|note <id>-<tag> [--yes]";
    let (Some(room), Some(kind), Some(raw)) = (a.arg(1), a.arg(2), a.arg(3)) else { return Err(usage(line)) };
    if kind != "task" && kind != "note" {
        return Err(usage(line));
    }
    let h = Handle::parse(raw).ok_or_else(|| format!("{raw:?} is not a handle like 122-v1"))?;
    let r = Ref { kind: kind.into(), origin: h.origin.clone(), id: h.id };
    let db = a.open()?;
    let removal = db.forget_removal(room, &r)?;
    if !a.yes {
        println!(
            "would forget {kind} {h} in room {room}: {} changes deleted for every member, permanently.",
            removal.changes
        );
        println!("Dry run -- repeat with --yes.");
        return Ok(());
    }
    db.forget(room, &r)?;
    println!("forgot {kind} {h} in room {room} ({} changes). Members apply it on their next sync.", removal.changes);
    Ok(())
}

/// A `HEALTHCHECK` for an image with no shell and no curl: one request with the standard
/// library, so it needs no HTTP client in the binary.
fn health(listen: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{Read, Write};
    // A relay bound to 0.0.0.0 answers on loopback too.
    let addr = listen.replace("0.0.0.0", "127.0.0.1");
    let mut s = std::net::TcpStream::connect(&addr)?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    write!(s, "GET /v1/hello HTTP/1.0\r\nHost: {addr}\r\n\r\n")?;
    let mut buf = String::new();
    s.read_to_string(&mut buf)?;
    if buf.starts_with("HTTP/1.0 200") || buf.starts_with("HTTP/1.1 200") {
        println!("ok");
        Ok(())
    } else {
        Err(format!("relay at {addr} answered: {}", buf.lines().next().unwrap_or("nothing")).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration("30m"), Ok(1800));
        assert_eq!(duration("6h"), Ok(21600));
        assert_eq!(duration("1d"), Ok(86400));
        assert_eq!(duration("90"), Ok(90));
        assert!(duration("0h").is_err());
        assert!(duration("6x").is_err());
    }

    #[test]
    fn tags_and_protocol_agree_on_the_server_tag() {
        use ai_kanban_protocol as protocol;
        assert!(protocol::valid_tag(protocol::SERVER_TAG));
    }
}
