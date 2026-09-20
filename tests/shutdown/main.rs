//! SIGTERM with no sessions live must stop the node promptly.
//!
//! Locks in the graceful path: listeners exit on the stop broadcast, the
//! recorder drain is bounded by `shutdown`, and nothing waits out the full
//! deadline when there is nothing to drain. Uses a long `RUSHLS_SHUTDOWN`
//! so a regression hangs the test instead of passing slowly.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// rtmp + srt + http listeners, each logging `listening` once bound.
const EXPECTED_LISTENERS: usize = 3;

// Reaps the child on every failure path instead of orphaning it.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn sigterm_with_no_sessions_exits_without_waiting_for_shutdown_deadline() {
    // Isolated working directory: the binary would otherwise pick up a
    // `rushls.toml` from the crate root, and an explicit empty config plus
    // environment makes the test independent of ambient files.
    let dir = std::env::temp_dir().join(format!("rushls-shutdown-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("test dir");
    std::fs::write(dir.join("empty.toml"), "").expect("empty config");

    let mut child = KillOnDrop(
        // Dash, not underscore: cargo names the variable after the target.
        Command::new(env!("CARGO_BIN_EXE_rushls"))
            // Hermetic against ambient config: the ready marker is INFO level.
            .env("RUST_LOG", "info")
            .env("RUSHLS_CONFIG", dir.join("empty.toml"))
            .env("RUSHLS_RTMP_LISTEN", "127.0.0.1:0")
            .env("RUSHLS_SRT_LISTEN", "127.0.0.1:0")
            .env("RUSHLS_HTTP_LISTEN", "127.0.0.1:0")
            // Long on purpose: before a fix the process would sit here the hour.
            .env("RUSHLS_SHUTDOWN", "1h")
            .current_dir(&dir)
            // rushls logs to stderr.
            .stderr(Stdio::piped())
            .spawn()
            .expect("rushls binary starts"),
    );

    // SIGTERM must land in the serve loop, not during startup: the signal
    // handlers are registered when the shutdown future is first polled.
    let stderr = child.0.stderr.take().expect("piped stderr");
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (line_tx, line_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut bound = 0;
        let mut notified = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = line_tx.send(line.clone());
            if line.contains("listening") {
                bound += 1;
                if bound >= EXPECTED_LISTENERS && !notified {
                    notified = true;
                    let _ = ready_tx.send(());
                }
            }
        }
    });
    ready_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("all listeners bound");
    std::thread::sleep(Duration::from_millis(500));

    // Child is our own spawn and not yet waited on, so the pid is valid.
    let pid = child.0.id() as libc::pid_t;
    // SAFETY: `pid` names a live child of this test process; `kill` only
    // delivers SIGTERM and never touches memory.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.0.try_wait().expect("poll child") {
            Some(status) => {
                for line in line_rx.try_iter() {
                    eprintln!("[child] {line}");
                }
                assert!(status.success(), "clean shutdown, got {status}");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            None if Instant::now() > deadline => {
                panic!("SIGTERM did not stop rushls within 15s (shutdown deadline is 1h)");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}
