//! The real relay on a loopback port, for tests that go through HTTP. Can be stopped and
//! started again on the same port, to test what a client does while it is gone.

#![allow(dead_code)]

use ai_kanban_relay::{router, Relay, ServerDb};
use std::net::SocketAddr;
use std::path::PathBuf;

pub struct HttpRelay {
    pub url: String,
    pub db_path: PathBuf,
    addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl HttpRelay {
    pub fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("server.db");
        {
            let db = ServerDb::open(&db_path).unwrap();
            for u in ["benny", "volker", "carla"] {
                db.add_user(u).unwrap();
            }
        }
        let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let mut r = HttpRelay { url: format!("http://{addr}"), db_path, addr, stop: None, thread: None, _dir: dir };
        r.resume();
        r
    }

    /// An admin handle on the relay's database, like `docker exec ai-kanban-relay ...`.
    pub fn admin(&self) -> ServerDb {
        ServerDb::open(&self.db_path).unwrap()
    }

    pub fn key(&self, user: &str, tag: &str) -> String {
        self.admin().add_key(user, tag, "").unwrap()
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }

    /// Starts serving again on the same address.
    pub fn resume(&mut self) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (addr, path) = (self.addr, self.db_path.clone());
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = loop {
                    match tokio::net::TcpListener::bind(addr).await {
                        Ok(l) => break l,
                        Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                    }
                };
                ready_tx.send(()).unwrap();
                let relay = Relay::new(ServerDb::open(&path).unwrap());
                // Not a graceful shutdown: that waits for open notification streams to end, which a
                // client only notices at its read timeout. Stopping drops every connection at once,
                // like a relay that crashed or was killed.
                tokio::select! {
                    r = axum::serve(listener, router(relay)) => r.unwrap(),
                    _ = rx => {}
                }
            });
        });
        ready_rx.recv().unwrap();
        self.stop = Some(tx);
        self.thread = Some(thread);
    }
}

impl Drop for HttpRelay {
    fn drop(&mut self) {
        self.stop();
    }
}
