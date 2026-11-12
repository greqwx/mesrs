// Copyright (c) 2026 Greg

//! handles the terminal being closed under the process
//!
//! windows reports it as a console control event, unix sends sighup to the
//! foreground group. both paths send one bye packet and ask the ui to stop

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::protocol::packet;

/// set by an os handler, the ui polls it between redraws
static QUIT: AtomicBool = AtomicBool::new(false);

struct ByeSender {
    socket: UdpSocket,
    dest: SocketAddr,
    peer_id: String,
    nick: String,
}

static SENDER: OnceLock<Mutex<Option<ByeSender>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<ByeSender>> {
    SENDER.get_or_init(|| Mutex::new(None))
}

pub fn requested() -> bool {
    QUIT.load(Ordering::Relaxed)
}

/// registers the os specific handlers, failures are not fatal, a missing
/// goodbye only means the other side drops us by timeout
pub fn install() {
    #[cfg(windows)]
    {
        // SAFETY: the handler only locks a mutex, sends a datagram and touches
        // an atomic, all of which are safe on the console control thread
        unsafe {
            SetConsoleCtrlHandler(Some(console_handler), 1);
        }
    }

    #[cfg(unix)]
    {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
        use signal_hook::iterator::Signals;

        let Ok(mut signals) = Signals::new([SIGHUP, SIGTERM, SIGINT]) else {
            return;
        };
        std::thread::spawn(move || {
            if signals.forever().next().is_some() {
                send_bye();
                QUIT.store(true, Ordering::Relaxed);
            }
        });
    }
}

/// the network thread calls this on start, on nick change and after a switch
pub fn remember(socket: &UdpSocket, dest: SocketAddr, peer_id: &str, nick: &str) {
    let Ok(copy) = socket.try_clone() else {
        return;
    };
    *slot().lock().unwrap_or_else(|err| err.into_inner()) = Some(ByeSender {
        socket: copy,
        dest,
        peer_id: peer_id.to_string(),
        nick: nick.to_string(),
    });
}

/// drops the socket copy, the network thread sends its own goodbye
pub fn forget() {
    *slot().lock().unwrap_or_else(|err| err.into_inner()) = None;
}

/// best effort, the other side also drops silent peers by timeout
fn send_bye() {
    let Ok(guard) = slot().lock() else {
        return;
    };
    let Some(state) = guard.as_ref() else {
        return;
    };
    let bye = packet(&state.peer_id, &state.nick, "bye", "", "");
    if let Ok(bytes) = serde_json::to_vec(&bye) {
        let _ = state.socket.send_to(&bytes, state.dest);
    }
}

#[cfg(windows)]
const CTRL_C_EVENT: u32 = 0;
#[cfg(windows)]
const CTRL_BREAK_EVENT: u32 = 1;
#[cfg(windows)]
const CTRL_CLOSE_EVENT: u32 = 2;
#[cfg(windows)]
const CTRL_LOGOFF_EVENT: u32 = 5;
#[cfg(windows)]
const CTRL_SHUTDOWN_EVENT: u32 = 6;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

/// closing the window does not reach the ui at all, so the goodbye has to go
/// out from here before the system tears the process down
#[cfg(windows)]
unsafe extern "system" fn console_handler(ctrl_type: u32) -> i32 {
    match ctrl_type {
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
            send_bye();
            QUIT.store(true, Ordering::Relaxed);
            1
        }
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            QUIT.store(true, Ordering::Relaxed);
            1
        }
        _ => 0,
    }
}
