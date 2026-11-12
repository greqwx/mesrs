// Copyright (c) 2026 Greg

//! terminal interface: app state, key handling, commands and drawing
//!
//! the screen is rebuilt from state only when something changed, so typing
//! stays smooth even when the chat log is long

use std::collections::HashSet;
use std::io::{self, Stdout, Write};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use chrono::Local;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::style::{
    Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, queue};

use crate::protocol::{NetEvent, OutCmd, parse_code};
use crate::shutdown;
use crate::text::{MAX_TEXT, clean_text, cut, fit, sanitize_nick, tail_fit, visible_width, wrap};

/// how long the ui sleeps between checks of the network channel
const POLL: Duration = Duration::from_millis(100);
/// old lines are dropped past this point to keep memory flat
const MAX_LINES: usize = 1000;
/// two clicks on the same row closer than this start a reply
const DOUBLE_CLICK: Duration = Duration::from_millis(450);
/// longest quote sent along with a reply, keeps the packet small
const QUOTE_W: usize = 80;
/// background of the row under the mouse, a couple of shades above black so
/// it reads as a hint rather than a selection
const HOVER_BG: Color = Color::Rgb {
    r: 36,
    g: 36,
    b: 36,
};

struct Command {
    name: &'static str,
    usage: &'static str,
    about: &'static str,
}

/// everything the hint line and /help know about, names stay lowercase
const COMMANDS: [Command; 8] = [
    Command {
        name: "help",
        usage: "/help",
        about: "list commands",
    },
    Command {
        name: "nick",
        usage: "/nick name",
        about: "change your nickname",
    },
    Command {
        name: "who",
        usage: "/who",
        about: "who is in the group",
    },
    Command {
        name: "me",
        usage: "/me action",
        about: "emote under your name",
    },
    Command {
        name: "topic",
        usage: "/topic text",
        about: "set the group topic, empty clears it",
    },
    Command {
        name: "group",
        usage: "/group 654321",
        about: "switch to another group",
    },
    Command {
        name: "clear",
        usage: "/clear",
        about: "clear the chat log",
    },
    Command {
        name: "quit",
        usage: "/quit",
        about: "quit",
    },
];

struct ChatMsg {
    seq: u64,
    time: String,
    text: String,
    /// the line this message answers, already shortened for the wire
    quote: Option<String>,
    style: Style,
}

/// one wrapped screen line, tied to the message it came from
struct LogRow {
    /// monotonic message number, survives old lines being dropped
    seq: u64,
    /// position in the log, used for lookups during the same draw
    idx: usize,
    text: String,
    style: Style,
}

/// one screen row, built from segments so a line can mix styles, plus the
/// flag that paints the hover background across the whole width
struct Row {
    parts: Vec<(String, Style)>,
    hover: bool,
}

impl Row {
    fn plain(text: String, style: Style) -> Self {
        Row {
            parts: vec![(text, style)],
            hover: false,
        }
    }
}

/// who a line belongs to, the whole line gets one colour
#[derive(Clone, Copy)]
enum Style {
    Header,
    Dim,
    Input,
    Own,
    Peer(Color),
    System,
    Error,
}

pub struct App {
    nick: String,
    code: u32,
    lines: Vec<ChatMsg>,
    /// ids of messages already shown, udp can deliver a datagram twice
    seen: HashSet<String>,
    input: String,
    /// wrapped lines scrolled up from the newest message, 0 means bottom
    scroll: usize,
    /// peer id and nickname, the id keeps the colour stable
    peers: Vec<(String, String)>,
    topic: String,
    /// monotonic id handed to every log line
    next_seq: u64,
    /// screen row under the mouse, None while the pointer is elsewhere
    hover: Option<u16>,
    /// message under the mouse, refreshed on every draw
    hover_msg: Option<u64>,
    /// line waiting to be quoted by the next message
    reply: Option<String>,
    /// last left click, for spotting a double click
    last_click: Option<(Instant, u16)>,
    dirty: bool,
}

impl App {
    pub fn new(nick: String, code: u32, port: u16) -> Self {
        let mut app = App {
            nick,
            code,
            lines: Vec::new(),
            seen: HashSet::new(),
            input: String::new(),
            scroll: 0,
            peers: Vec::new(),
            topic: String::new(),
            next_seq: 1,
            hover: None,
            hover_msg: None,
            reply: None,
            last_click: None,
            dirty: true,
        };
        app.push_system(format!(
            "group {:06} on udp port {}, share the code with people on your network",
            code, port
        ));
        app.push_system(
            "enter sends, /help shows commands, double click a message replies, ctrl+c quits"
                .to_string(),
        );
        app
    }

    fn push_msg(&mut self, text: String, style: Style) {
        self.push_line(text, None, style)
    }

    fn push_line(&mut self, text: String, quote: Option<String>, style: Style) {
        self.lines.push(ChatMsg {
            seq: self.next_seq,
            time: Local::now().format("%H:%M").to_string(),
            text,
            quote,
            style,
        });
        self.next_seq += 1;
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..MAX_LINES / 4);
        }
        self.dirty = true;
    }

    fn push_system(&mut self, text: String) {
        self.push_msg(text, Style::System);
    }

    fn peer_nicks(&self) -> Vec<&str> {
        self.peers.iter().map(|(_, nick)| nick.as_str()).collect()
    }

    /// turns events from the network thread into chat lines and roster changes
    fn handle_net(&mut self, ev: NetEvent) {
        match ev {
            NetEvent::Message(msg) => {
                if !self.seen.insert(msg.id) {
                    return;
                }
                if self.seen.len() > 5000 {
                    self.seen.clear();
                }
                let line = if msg.action {
                    format!("* {} {}", msg.nick, msg.text)
                } else {
                    format!("{}: {}", msg.nick, msg.text)
                };
                let quote = Some(msg.quote).filter(|q| !q.is_empty());
                self.push_line(line, quote, Style::Peer(peer_color(&msg.peer)));
            }
            NetEvent::Peers(list) => {
                self.peers = list;
                self.dirty = true;
            }
            NetEvent::Topic(topic) => {
                self.topic = topic;
                self.dirty = true;
            }
            NetEvent::Joined { code, port } => {
                self.code = code;
                self.lines.clear();
                self.peers.clear();
                self.topic.clear();
                self.reply = None;
                self.scroll = 0;
                self.push_system(format!("joined group {code:06} on udp port {port}"));
            }
            NetEvent::Notice(text) => self.push_system(text),
            NetEvent::Undelivered { to, text } => {
                self.push_msg(
                    format!("not delivered to {}: {text}", to.join(", ")),
                    Style::Error,
                );
            }
            NetEvent::Error(text) => {
                self.push_msg(format!("error: {text}"), Style::Error);
            }
        }
    }

    /// runs one slash command, returns true when the app should quit
    fn handle_command(&mut self, line: &str, tx: &Sender<OutCmd>) -> bool {
        let rest = line.trim_start_matches('/');
        let (name, args) = match rest.find(char::is_whitespace) {
            Some(at) => (&rest[..at], rest[at..].trim()),
            None => (rest, ""),
        };

        match name.to_lowercase().as_str() {
            "help" => {
                self.push_system("commands:".to_string());
                for cmd in &COMMANDS {
                    self.push_system(format!(" {}  {}", cmd.usage, cmd.about));
                }
            }
            "nick" => {
                let nick = sanitize_nick(args);
                if nick.is_empty() {
                    self.push_system("usage: /nick name".to_string());
                    return false;
                }
                let _ = tx.send(OutCmd::Nick(nick.clone()));
                self.nick = nick.clone();
                // other clients print a rename line from the nick packet
                self.push_system(format!("you are now {nick}"));
            }
            "who" => {
                if self.peers.is_empty() {
                    self.push_system("nobody else here yet".to_string());
                } else {
                    self.push_system(format!("{} online", self.peers.len()));
                    let peers = self.peers.clone();
                    for (id, nick) in peers {
                        self.push_msg(format!("online  {nick}"), Style::Peer(peer_color(&id)));
                    }
                }
            }
            "me" => {
                let text = clean_text(args);
                if text.is_empty() {
                    self.push_system("usage: /me action".to_string());
                    return false;
                }
                self.reply = None;
                let _ = tx.send(OutCmd::Action(text.clone()));
                self.push_msg(format!("* {} {}", self.nick, text), Style::Own);
                self.scroll = 0;
            }
            "topic" => {
                let text = clean_text(args);
                let _ = tx.send(OutCmd::Topic(text.clone()));
                self.topic = text.clone();
                if text.is_empty() {
                    self.push_system("topic cleared".to_string());
                } else {
                    self.push_system(format!("topic: {text}"));
                }
            }
            "group" => {
                let Some(code) = parse_code(args) else {
                    self.push_system("usage: /group 654321".to_string());
                    return false;
                };
                if code == self.code {
                    self.push_system("already in this group".to_string());
                    return false;
                }
                let _ = tx.send(OutCmd::Join(code));
                self.push_system(format!("joining group {code:06}"));
            }
            "clear" => {
                self.lines.clear();
                self.reply = None;
                self.dirty = true;
            }
            "quit" => return true,
            _ => {
                self.push_system(format!("unknown command /{name}, try /help"));
            }
        }
        false
    }

    /// applies one key press, returns true when the app should quit
    fn handle_key(&mut self, key: KeyEvent, tx: &Sender<OutCmd>) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('q') => return true,
                KeyCode::Char('u') => {
                    self.input.clear();
                    self.dirty = true;
                }
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Char(c) => {
                if self.input.chars().count() < MAX_TEXT {
                    self.input.push(c);
                    self.dirty = true;
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.dirty = true;
            }
            KeyCode::Tab => {
                self.complete_command();
            }
            KeyCode::Enter => {
                let raw = self.input.clone();
                let text = clean_text(&raw);
                self.input.clear();
                self.dirty = true;
                if text.is_empty() {
                    return false;
                }
                if text.starts_with('/') {
                    let quit = self.handle_command(&text, tx);
                    self.scroll = 0;
                    return quit;
                }
                let quote = self.reply.take();
                let _ = tx.send(OutCmd::Chat {
                    text: text.clone(),
                    quote: quote.clone().unwrap_or_default(),
                });
                // show it right away, the network thread sends in parallel
                self.push_line(format!("{}: {}", self.nick, text), quote, Style::Own);
                self.scroll = 0;
            }
            // scroll offsets are counted from the newest line, up goes back
            KeyCode::Up => {
                self.scroll += 1;
                self.dirty = true;
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_sub(1);
                self.dirty = true;
            }
            KeyCode::PageUp => {
                self.scroll += 10;
                self.dirty = true;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                self.dirty = true;
            }
            KeyCode::Esc => {
                // esc drops the pending quote first, the input is the next thing
                if self.reply.take().is_none() {
                    self.input.clear();
                }
                self.dirty = true;
            }
            _ => {}
        }
        false
    }

    /// fills the input with the only matching command, or with the shared
    /// prefix of several matches, and keeps the hint line in sync
    fn complete_command(&mut self) {
        let matches = command_matches(&self.input);
        let Some(first) = matches.first() else {
            return;
        };
        let typed = self.input.trim_start_matches('/').to_lowercase();

        let filled = if matches.len() == 1 {
            Some(format!("/{} ", first.name))
        } else {
            let mut common = first.name.to_string();
            for cmd in &matches[1..] {
                while !cmd.name.starts_with(&common) {
                    common.pop();
                }
            }
            if common.len() > typed.len() {
                Some(format!("/{common}"))
            } else {
                Some(format!("/{} ", first.name))
            }
        };

        if let Some(text) = filled {
            self.input = text;
            self.dirty = true;
        }
    }

    /// takes the message under the pointer as the quote for the next one
    fn start_reply(&mut self) {
        let Some(seq) = self.hover_msg else {
            return;
        };
        let Some(msg) = self.lines.iter().find(|line| line.seq == seq) else {
            return;
        };
        if !matches!(msg.style, Style::Peer(_) | Style::Own) {
            return;
        }
        self.reply = Some(cut(&msg.text, QUOTE_W));
        self.dirty = true;
    }

    /// main loop, returns when the user quits or the terminal breaks
    pub fn ui(
        &mut self,
        out: &mut Stdout,
        rx: &Receiver<NetEvent>,
        tx: &Sender<OutCmd>,
    ) -> io::Result<()> {
        loop {
            // set by the handler that catches a closing terminal window
            if shutdown::requested() {
                return Ok(());
            }

            loop {
                match rx.try_recv() {
                    Ok(ev) => self.handle_net(ev),
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                }
            }

            if self.dirty {
                self.draw(out)?;
                self.dirty = false;
            }

            if !event::poll(POLL)? {
                continue;
            }
            match event::read()? {
                // windows reports key releases too, only presses matter
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if self.handle_key(key, tx) {
                        return Ok(());
                    }
                }
                Event::Resize(_, _) => self.dirty = true,
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Moved => {
                        if self.hover != Some(mouse.row) {
                            self.hover = Some(mouse.row);
                            self.dirty = true;
                        }
                    }
                    // the wheel would otherwise do nothing, mouse capture turns
                    // off the terminal's own alternate screen scrolling
                    MouseEventKind::ScrollUp => {
                        self.scroll += 3;
                        self.dirty = true;
                    }
                    MouseEventKind::ScrollDown => {
                        self.scroll = self.scroll.saturating_sub(3);
                        self.dirty = true;
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        // a click without a fresh Moved still points at the
                        // row under the pointer, the next draw resolves it
                        self.hover = Some(mouse.row);
                        self.dirty = true;
                        let now = Instant::now();
                        let double = self.last_click.is_some_and(|(at, row)| {
                            row == mouse.row && now.duration_since(at) <= DOUBLE_CLICK
                        });
                        self.last_click = if double { None } else { Some((now, mouse.row)) };
                        if double {
                            self.start_reply();
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    /// draws the five screen bands: title, roster, chat, hint, input
    fn draw(&mut self, out: &mut Stdout) -> io::Result<()> {
        let (cols, rows) = terminal::size()?;
        let width = (cols as usize).max(24);
        let height = (rows as usize).max(6);
        let chat_h = height - 4;
        // one column is lost to the left indent, one stays as a right margin
        let content_w = width - 2;

        let mut frame: Vec<Row> = Vec::with_capacity(height);

        let mut header = format!(
            " mesrs  #{:06}  you: {}  {} online",
            self.code,
            self.nick,
            self.peers.len()
        );
        if self.scroll > 0 {
            header.push_str(&format!("  up {}", self.scroll));
        }
        frame.push(Row::plain(fit(&header, width), Style::Header));

        let online = if self.peers.is_empty() {
            "waiting for someone to join".to_string()
        } else {
            format!("online: {}", self.peer_nicks().join(", "))
        };
        let peer_line = if self.topic.is_empty() {
            format!(" {online}")
        } else {
            format!(" topic: {}  |  {online}", self.topic)
        };
        frame.push(Row::plain(fit(&peer_line, width), Style::Dim));

        // wrapping happens for the whole log on every redraw, the log is
        // capped at MAX_LINES so this stays cheap
        let mut chat: Vec<LogRow> = Vec::new();
        for (idx, msg) in self.lines.iter().enumerate() {
            if let Some(quote) = &msg.quote {
                for line in wrap(quote, content_w.saturating_sub(3)) {
                    chat.push(LogRow {
                        seq: msg.seq,
                        idx,
                        text: format!(" │ {line}"),
                        style: Style::Dim,
                    });
                }
            }
            for chunk in wrap(&msg.text, content_w) {
                chat.push(LogRow {
                    seq: msg.seq,
                    idx,
                    text: format!(" {chunk}"),
                    style: msg.style,
                });
            }
        }

        let max_scroll = chat.len().saturating_sub(chat_h);
        let scroll = self.scroll.min(max_scroll);
        self.scroll = scroll;
        let end = chat.len().saturating_sub(scroll);
        let start = end.saturating_sub(chat_h);
        let visible = &chat[start..end];

        // blank rows sit on top of the log so messages stay above the hint
        // line, which puts the first message at chat_top
        let blanks = chat_h.saturating_sub(visible.len());
        let chat_top = 2 + blanks;
        let hover_row = self
            .hover
            .map(usize::from)
            .filter(|row| *row >= chat_top && *row < chat_top + visible.len());
        let hovered = hover_row.map(|row| visible[row - chat_top].seq);
        self.hover_msg = hovered;

        for _ in 0..blanks {
            frame.push(Row::plain(String::new(), Style::Dim));
        }
        for (offset, row) in visible.iter().enumerate() {
            let msg = &self.lines[row.idx];
            if Some(row.seq) != hovered {
                frame.push(Row::plain(row.text.clone(), row.style));
                continue;
            }

            // a highlighted row is padded to the edge, and on the row under
            // the pointer the stamp takes the last columns; the text is cut
            // there instead of pushing the stamp onto a row of its own
            let on_pointer = Some(chat_top + offset) == hover_row;
            let stamp = format!(" {}", msg.time);
            let keep = if on_pointer {
                width.saturating_sub(visible_width(&stamp))
            } else {
                width
            };
            let body = cut(&row.text, keep);
            let pad = keep.saturating_sub(visible_width(&body));
            let mut parts = vec![(body, row.style), (" ".repeat(pad), Style::Dim)];
            if on_pointer {
                parts.push((stamp, Style::Dim));
            }
            frame.push(Row { parts, hover: true });
        }

        let hint = if self.input.starts_with('/') {
            hint_for(&self.input).unwrap_or_default()
        } else if let Some(quote) = &self.reply {
            format!(" replying to {quote}  (esc cancels)")
        } else {
            String::new()
        };
        frame.push(Row::plain(fit(&hint, width), Style::Dim));

        let tail = tail_fit(&self.input, width.saturating_sub(3));
        frame.push(Row::plain(format!("> {tail}"), Style::Input));

        queue!(out, cursor::MoveTo(0, 0))?;
        for (y, row) in frame.iter().enumerate() {
            queue!(
                out,
                cursor::MoveTo(0, y as u16),
                terminal::Clear(ClearType::CurrentLine)
            )?;
            if row.hover {
                queue!(out, SetBackgroundColor(HOVER_BG))?;
            }
            for (text, style) in &row.parts {
                match style {
                    Style::Header => queue!(out, SetAttribute(Attribute::Reverse))?,
                    Style::Dim | Style::System => queue!(out, SetAttribute(Attribute::Dim))?,
                    Style::Own => queue!(out, SetForegroundColor(Color::Green))?,
                    Style::Peer(color) => queue!(out, SetForegroundColor(*color))?,
                    Style::Error => queue!(out, SetForegroundColor(Color::Red))?,
                    Style::Input => {}
                }
                // reset per segment keeps attributes from leaking, but the
                // background is set for the whole row and must survive
                queue!(
                    out,
                    Print(text.as_str()),
                    SetAttribute(Attribute::Reset),
                    SetForegroundColor(Color::Reset)
                )?;
            }
            if row.hover {
                queue!(out, SetBackgroundColor(Color::Reset))?;
            }
        }

        let cursor_x = 2 + visible_width(&tail);
        queue!(
            out,
            cursor::MoveTo(
                cursor_x.min(width.saturating_sub(1)) as u16,
                (height - 1) as u16
            )
        )?;
        out.flush()
    }
}

/// commands whose name starts with the typed prefix, no matches once the
/// first argument has been typed
fn command_matches(input: &str) -> Vec<&'static Command> {
    let Some(rest) = input.trim_start().strip_prefix('/') else {
        return Vec::new();
    };
    if rest.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let prefix = rest.to_lowercase();
    COMMANDS
        .iter()
        .filter(|cmd| cmd.name.starts_with(&prefix))
        .collect()
}

/// text of the hint line above the input, None when nothing matches
fn hint_for(input: &str) -> Option<String> {
    let matches = command_matches(input);
    match matches.len() {
        0 => None,
        1 => {
            let cmd = matches[0];
            Some(format!(" {}  {}", cmd.usage, cmd.about))
        }
        _ => {
            let names: Vec<String> = matches.iter().map(|cmd| format!("/{}", cmd.name)).collect();
            Some(format!(" {}  tab completes", names.join("  ")))
        }
    }
}

/// picks a stable colour per peer id, own messages use green separately
fn peer_color(peer: &str) -> Color {
    const PALETTE: [Color; 5] = [
        Color::Cyan,
        Color::Magenta,
        Color::Yellow,
        Color::Blue,
        Color::Rgb {
            r: 255,
            g: 170,
            b: 90,
        },
    ];
    let mut hash: u32 = 2_166_136_261;
    for byte in peer.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    PALETTE[hash as usize % PALETTE.len()]
}
