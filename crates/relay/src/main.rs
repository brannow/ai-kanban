//! `ai-kanban-relay` -- the server a team shares boards through. `docs/sync.md` is the design.
//!
//! Its own binary rather than a subcommand of `ai-kanban`: the relay runs in a container on a
//! server, the client on every laptop, and neither has any use for the other's code. They share
//! only the wire format (`ai-kanban-protocol`).

#[tokio::main]
async fn main() {
    if let Err(e) = ai_kanban_relay::admin::run().await {
        eprintln!("ai-kanban-relay: {e}");
        std::process::exit(1);
    }
}
