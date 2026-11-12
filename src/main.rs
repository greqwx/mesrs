// Copyright (c) 2026 Greg

//! entry point: reads arguments, asks for what is missing and wires the ui to
//! the network thread

mod net;
mod protocol;
mod shutdown;
mod text;
mod ui;

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

use crossterm::{cursor, event, execute, terminal};

use crate::net::Config;
use crate::protocol::{group_for, parse_code, random_id};
use crate::text::{display_nick, sanitize_nick};
use crate::ui::App;

fn main() {
    if let Err(err) = run() {
        eprintln!("mesrs: {err}");
        std::process::exit(1);
    }
}

/// parses arguments, starts the network thread and hands control to the ui
fn run() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    let code = match args.first() {
        Some(raw) => match parse_code(raw.trim()) {
            Some(code) => code,
            None => {
                eprintln!("group code must be exactly 6 digits");
                std::process::exit(2);
            }
        },
        None => match ask_code()? {
            Some(code) => code,
            None => return Ok(()),
        },
    };

    // the nickname can come from the command line or from stdin, a prompt on
    // a closed stdin just leaves the default name in place
    let nick = match args.get(1) {
        Some(raw) => display_nick(raw),
        None => ask_nick()?,
    };

    let (group, port) = group_for(code);
    let my_id = random_id();

    let (net_tx, net_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    // set on the way out, the network thread checks it between packets
    let stop = Arc::new(AtomicBool::new(false));

    // a panic in raw mode would leave the shell without echo and with the
    // alternate screen still active, so the hook restores it first
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous_hook(info);
    }));

    let mut out = io::stdout();
    shutdown::install();
    terminal::enable_raw_mode()?;
    execute!(
        out,
        terminal::EnterAlternateScreen,
        cursor::Show,
        event::EnableMouseCapture
    )?;

    let net_stop = Arc::clone(&stop);
    let config = Config {
        my_id,
        nick: nick.clone(),
        group,
        port,
    };
    let handle = thread::spawn(move || net::net_run(config, net_tx, cmd_rx, net_stop));

    let mut app = App::new(nick, code, port);
    let result = app.ui(&mut out, &net_rx, &cmd_tx);

    // the network thread sends the goodbye packet on its way out, joining it
    // makes sure the packet is out before the process ends
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
    restore_terminal();
    result
}

/// leaves raw mode and the alternate screen, safe to call more than once
fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let mut out = io::stdout();
    let _ = execute!(
        out,
        cursor::Show,
        event::DisableMouseCapture,
        terminal::LeaveAlternateScreen
    );
}

fn print_help() {
    println!("mesrs, a small lan chat by group code");
    println!();
    println!("usage: mesrs <group-code> [nickname]");
    println!();
    println!("  group-code   exactly 6 digits, everyone with the same code talks together");
    println!();
    println!("keys: enter sends, up/down and pgup/pgdn scroll, ctrl+c quits");
}

/// asks until a valid code is typed, None means stdin is closed
fn ask_code() -> io::Result<Option<u32>> {
    loop {
        print!("group code (6 digits): ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if let Some(code) = parse_code(line.trim()) {
            return Ok(Some(code));
        }
        println!("need exactly 6 digits");
    }
}

fn ask_nick() -> io::Result<String> {
    print!("nickname [{}]: ", default_nick());
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let nick = sanitize_nick(&line);
    Ok(if nick.is_empty() {
        default_nick()
    } else {
        nick
    })
}

fn default_nick() -> String {
    let raw = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default();
    let nick = sanitize_nick(&raw);
    if nick.is_empty() {
        "anon".to_string()
    } else {
        nick
    }
}
