//! The Windows control channel: the request/reply protocol of [`crate::control`]
//! carried over a per-user named pipe (`\\.\pipe\hushmic-<user>`) instead of a
//! Unix socket, plus the "show yourself" ping a second launch forwards.
//!
//! The grammar, framing and renderers are shared, so a Windows client and the
//! Linux `hushmic` CLI say the same things; only the transport differs. As on
//! Linux the server is serial: one connection at a time, one request line in,
//! one framed reply out.

use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use super::{HOP, RATE};
use crate::control::{
    self, decode_response, parse_request, ControlReq, IO_TIMEOUT, MAX_REQUEST, NO_REPLY,
    REPLY_TIMEOUT,
};

/// The in-process chain's declared algorithmic latency in samples: the pump's
/// [`super::pump::LATENCY_MS`] at [`RATE`], the same 80 ms figure the Linux
/// filter-chain declares, so a status line reads the same on both hosts.
pub const LATENCY_SAMPLES: u32 = 80 * RATE / 1000;

/// The light tier's model, the same file stem Linux's controller picks
/// (`controller::LIGHT_MODEL`), without depending on the PipeWire module.
pub const LIGHT_MODEL: &str = "dpdfnet2_48khz_hr";

/// There is no PipeWire graph state to be "idle" in: the pump runs when the
/// chain runs, so no chain state word is idle on Windows.
pub fn state_is_idle(_state: &str) -> bool {
    false
}

/// `\\.\pipe\hushmic-<sanitized user>`: named pipes are machine-wide, so the
/// user name keeps two signed-in accounts apart (as `$XDG_RUNTIME_DIR` does
/// on Linux).
pub fn pipe_name() -> String {
    format!(r"\\.\pipe\hushmic-{}", user_slug())
}

/// The second launch's "show yourself" ping; separate from the control pipe
/// so it carries no verb and cannot be confused with a request line.
pub fn show_pipe_name() -> String {
    format!(r"\\.\pipe\hushmic-{}-show", user_slug())
}

fn user_slug() -> String {
    let raw = std::env::var("USERNAME").unwrap_or_default();
    let slug: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if slug.is_empty() {
        "user".to_string()
    } else {
        slug
    }
}

/// Serve the control pipe on a background thread: one request line per
/// connection, delivered as `ControlReq` into `tx`, the reply written back to
/// the peer; exits when the main loop's receiver is gone.
pub fn spawn_listener(tx: Sender<ControlReq>) {
    std::thread::spawn(move || loop {
        let Ok(server) = create_instance(&pipe_name()) else {
            eprintln!("[hushmic] cannot create the control pipe {}", pipe_name());
            return;
        };
        if unsafe { ConnectNamedPipe(handle(&server), None) }.is_err() {
            continue; // dropped before it connected: take the next one
        }
        let keep_going = serve_one(&server, &tx);
        unsafe {
            let _ = DisconnectNamedPipe(handle(&server));
        }
        drop(server);
        if !keep_going {
            return;
        }
    });
}

/// Serve the show pipe: every connection is one ping, no reply.
pub fn spawn_show_listener(tx: Sender<()>) {
    std::thread::spawn(move || loop {
        let Ok(server) = create_instance(&show_pipe_name()) else {
            return;
        };
        if unsafe { ConnectNamedPipe(handle(&server), None) }.is_err() {
            continue;
        }
        unsafe {
            let _ = DisconnectNamedPipe(handle(&server));
        }
        drop(server);
        if tx.send(()).is_err() {
            return; // main loop gone
        }
    });
}

/// Tell the running instance to show itself (a flag-less second launch).
/// `false` when nothing is listening.
pub fn request_show() -> bool {
    let Ok(mut client) = connect(&show_pipe_name()) else {
        return false;
    };
    let _ = client.write_all(b"show\n");
    true
}

/// Run one CLI command against the control pipe. Same contract as the Unix
/// `control::client_run`: `(exit code, output)`, usage errors exit 1 without
/// touching the pipe, no listener is exit 2, `NO_REPLY` exit 1.
pub fn client_run(words: &[String]) -> (i32, String) {
    let refs: Vec<&str> = words.iter().map(|s| s.as_str()).collect();
    if let Err(usage) = parse_request(&refs) {
        return (1, usage);
    }
    let not_running = || {
        (
            2,
            "hushmic is not running (start it with hushmic --tray or hushmic --headless)"
                .to_string(),
        )
    };
    let Ok(mut pipe) = connect(&pipe_name()) else {
        return not_running();
    };
    if pipe.write_all(format!("{}\n", refs.join(" ")).as_bytes()).is_err() {
        return not_running();
    }
    let mut raw = Vec::new();
    let mut buf = [0u8; 512];
    let deadline = Instant::now() + REPLY_TIMEOUT;
    loop {
        if !wait_readable(&pipe, deadline) {
            return (1, NO_REPLY.to_string());
        }
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break, // the server closes after the reply
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
    if raw.is_empty() {
        return (1, NO_REPLY.to_string());
    }
    let (ok, payload) = decode_response(&String::from_utf8_lossy(&raw));
    (if ok { 0 } else { 1 }, payload)
}

/// One connection: read the request line, let the main loop answer, write the
/// reply. `false` = the main loop is gone (tray shutting down).
fn serve_one(mut server: &File, tx: &Sender<ControlReq>) -> bool {
    let deadline = Instant::now() + IO_TIMEOUT;
    if !wait_readable(server, deadline) {
        return true; // connected and stalled: drop it, serve the next
    }
    let mut raw = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match server.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.contains(&b'\n') || raw.len() > MAX_REQUEST {
                    break;
                }
            }
        }
        if !wait_readable(server, deadline) {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let line = text.lines().next().unwrap_or("").trim().to_string();
    if line.is_empty() {
        return true;
    }
    let (rtx, rrx) = channel();
    let (wtx, wrx) = channel();
    if tx
        .send(ControlReq {
            line,
            reply: rtx,
            written: wrx,
        })
        .is_err()
    {
        return false;
    }
    let reply = rrx
        .recv_timeout(REPLY_TIMEOUT)
        .unwrap_or_else(|_| control::encode_err("HushMic did not answer in time"));
    let _ = server.write_all(reply.as_bytes());
    let _ = server.flush();
    let _ = wtx.send(());
    true
}

/// One server instance of `name`, owned by the returned [`File`].
fn create_instance(name: &str) -> Result<File, ()> {
    let wide = wide(name);
    let h = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            MAX_REQUEST as u32,
            MAX_REQUEST as u32,
            0,
            None,
        )
    };
    if h.is_invalid() {
        Err(())
    } else {
        Ok(owned(h))
    }
}

/// Connect as a client. No retry: with unlimited instances a live server
/// always accepts, so a failure means nothing is listening.
fn connect(name: &str) -> Result<File, ()> {
    let wide = wide(name);
    unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            windows::Win32::Foundation::GENERIC_READ.0
                | windows::Win32::Foundation::GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map(owned)
    .map_err(|_| ())
}

fn owned(h: HANDLE) -> File {
    // SAFETY: `h` is a fresh, valid handle we own and hand over exactly once.
    File::from(unsafe { OwnedHandle::from_raw_handle(h.0) })
}

fn handle(f: &File) -> HANDLE {
    HANDLE(f.as_raw_handle())
}

/// Named pipes have no read timeout, so wait for bytes with a 2 ms poll
/// instead of blocking: a peer that connects and stalls still gets dropped at
/// `until`, as the Unix listener's `set_read_timeout` does. A broken pipe
/// counts as readable, so the following `read` sees the EOF.
fn wait_readable(f: &File, until: Instant) -> bool {
    loop {
        let mut avail = 0u32;
        let peek = unsafe {
            PeekNamedPipe(handle(f), None, 0, None, Some(&mut avail), None)
        };
        if peek.is_err() || avail > 0 {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// The pump's hop is the unit both the status renderer and the chain speak;
// keeping the import used documents the 48 kHz assumption above.
const _: () = assert!(LATENCY_SAMPLES == 3840 && HOP == 480);
