// Copyright (c) 2026 Greg

//! udp transport: one socket per group, discovery by periodic hellos
//!
//! chat messages and actions are retransmitted until every peer that was
//! online at send time acknowledges them, hello packets keep the roster fresh
//! and tombstones keep a short network hiccup from looking like a rejoin

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use crate::protocol::{NetEvent, OutCmd, PROTO, Packet, PeerMessage, group_for, packet, random_id};
use crate::shutdown;
use crate::text::{clean_text, display_nick};

/// how often we tell the group we are still here
const HEARTBEAT: Duration = Duration::from_millis(2000);
/// a peer that stays quiet for longer is considered gone, three missed
/// hellos in a row are already too many, tombstones hide the return trip
const PEER_TTL: Duration = Duration::from_secs(8);
/// how long a peer that timed out is remembered as gone, a comeback inside
/// this window is silent and does not look like a fresh join
const TOMB_TTL: Duration = Duration::from_secs(120);
/// socket read timeout, also the worst case delay before shutdown is noticed
const READ_TIMEOUT: Duration = Duration::from_millis(150);
/// pause between retransmissions of an unacknowledged message
const RETRY_EVERY: Duration = Duration::from_millis(300);
/// give up on acknowledgements after this long
const RETRY_FOR: Duration = Duration::from_secs(6);

struct Peer {
    nick: String,
    last_seen: Instant,
}

/// a chat message or action waiting for acknowledgements
struct Pending {
    bytes: Vec<u8>,
    /// peer id and nickname of everyone who still has to confirm delivery
    waiting: Vec<(String, String)>,
    last_sent: Instant,
    deadline: Instant,
    text: String,
}

pub struct Config {
    pub my_id: String,
    pub nick: String,
    pub group: Ipv4Addr,
    pub port: u16,
}

/// binds the group port and joins the multicast group
///
/// reuse_address is needed so a second copy of the client can listen on the
/// same port on one machine, which makes local testing easy. the flag in the
/// result says whether multicast worked, networks that block it fall back to
/// broadcast on the same port
fn open_socket(port: u16, group: Ipv4Addr) -> io::Result<(UdpSocket, bool)> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    sock.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    let socket: UdpSocket = sock.into();
    socket.set_read_timeout(Some(READ_TIMEOUT))?;

    let joined = socket
        .join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)
        .is_ok();
    if !joined {
        socket.set_broadcast(true)?;
    }
    Ok((socket, joined))
}

fn dest_for(multicast: bool, group: Ipv4Addr, port: u16) -> SocketAddr {
    if multicast {
        SocketAddr::from((group, port))
    } else {
        SocketAddr::from((Ipv4Addr::BROADCAST, port))
    }
}

fn send_hello(socket: &UdpSocket, dest: SocketAddr, id: &str, nick: &str, topic: &str) {
    let mut hello = packet(id, nick, "hello", "", "");
    hello.topic = topic.to_string();
    send_packet(socket, dest, &hello);
}

/// sends a chat packet and remembers it until every peer confirms it
///
/// with an empty roster there is nobody to wait for, so the packet is just
/// sent once and forgotten
fn send_reliable(
    socket: &UdpSocket,
    dest: SocketAddr,
    pending: &mut HashMap<String, Pending>,
    peers: &HashMap<String, Peer>,
    pkt: Packet,
    text: &str,
) {
    let Ok(bytes) = serde_json::to_vec(&pkt) else {
        return;
    };
    if bytes.len() > 1400 {
        return;
    }
    let _ = socket.send_to(&bytes, dest);
    if peers.is_empty() {
        return;
    }

    let waiting = peers
        .iter()
        .map(|(id, peer)| (id.clone(), peer.nick.clone()))
        .collect();
    let now = Instant::now();
    pending.insert(
        pkt.id.clone(),
        Pending {
            bytes,
            waiting,
            last_sent: now,
            deadline: now + RETRY_FOR,
            text: text.to_string(),
        },
    );
}

/// runs until the stop flag is set, then says goodbye to the group
pub fn net_run(config: Config, tx: Sender<NetEvent>, rx: Receiver<OutCmd>, stop: Arc<AtomicBool>) {
    let Config {
        my_id,
        nick,
        group,
        port,
    } = config;
    let mut my_nick = nick;
    let mut my_topic = String::new();

    let (mut socket, multicast) = match open_socket(port, group) {
        Ok(value) => value,
        Err(err) => {
            let _ = tx.send(NetEvent::Error(format!(
                "cannot open udp port {port}: {err}"
            )));
            return;
        }
    };

    if !multicast {
        let _ = tx.send(NetEvent::Notice(
            "multicast is unavailable, using broadcast".to_string(),
        ));
    }

    let mut dest = dest_for(multicast, group, port);
    shutdown::remember(&socket, dest, &my_id, &my_nick);
    let mut peers: HashMap<String, Peer> = HashMap::new();
    let mut tombstones: HashMap<String, Instant> = HashMap::new();
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut last_hello: Option<Instant> = None;
    let mut last_sweep = Instant::now();
    let mut buf = [0u8; 2048];

    while !stop.load(Ordering::Relaxed) {
        // drain outgoing commands first, otherwise a chat send could wait for
        // the next incoming packet, which happens up to READ_TIMEOUT later
        loop {
            match rx.try_recv() {
                Ok(OutCmd::Chat { text, quote }) => {
                    let id = random_id();
                    let mut pkt = packet(&my_id, &my_nick, "msg", &id, &text);
                    pkt.quote = quote;
                    send_reliable(&socket, dest, &mut pending, &peers, pkt, &text);
                }
                Ok(OutCmd::Action(text)) => {
                    let id = random_id();
                    let pkt = packet(&my_id, &my_nick, "action", &id, &text);
                    send_reliable(&socket, dest, &mut pending, &peers, pkt, &text);
                }
                Ok(OutCmd::Nick(nick)) => {
                    my_nick = nick;
                    shutdown::remember(&socket, dest, &my_id, &my_nick);
                    send_packet(&socket, dest, &packet(&my_id, &my_nick, "nick", "", ""));
                }
                Ok(OutCmd::Topic(text)) => {
                    my_topic = text;
                    send_packet(
                        &socket,
                        dest,
                        &packet(&my_id, &my_nick, "topic", "", &my_topic),
                    );
                }
                Ok(OutCmd::Join(code)) => {
                    let (new_group, new_port) = group_for(code);
                    match open_socket(new_port, new_group) {
                        Ok((new_socket, new_multicast)) => {
                            // the old socket is dropped here, the new group
                            // starts from an empty roster, topic and queue
                            socket = new_socket;
                            dest = dest_for(new_multicast, new_group, new_port);
                            shutdown::remember(&socket, dest, &my_id, &my_nick);
                            peers.clear();
                            pending.clear();
                            tombstones.clear();
                            my_topic.clear();
                            send_peers(&tx, &peers);
                            send_hello(&socket, dest, &my_id, &my_nick, &my_topic);
                            last_hello = Some(Instant::now());
                            if !new_multicast {
                                let _ = tx.send(NetEvent::Notice(
                                    "multicast is unavailable, using broadcast".to_string(),
                                ));
                            }
                            let _ = tx.send(NetEvent::Joined {
                                code,
                                port: new_port,
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(NetEvent::Error(format!(
                                "cannot join group {code:06}: {err}"
                            )));
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }

        match socket.recv_from(&mut buf) {
            Ok((n, _)) => {
                let pkt: Packet = match serde_json::from_slice(&buf[..n]) {
                    Ok(pkt) => pkt,
                    Err(_) => continue,
                };
                // multicast and broadcast copy our own packets back to us
                if pkt.proto != PROTO || pkt.peer == my_id || pkt.peer.is_empty() {
                    continue;
                }
                let nick = display_nick(&pkt.nick);

                // remove before the roster insert below, a goodbye should not
                // make the peer show up as joined first
                if pkt.kind == "bye" {
                    if let Some(old) = peers.remove(&pkt.peer) {
                        let _ = tx.send(NetEvent::Notice(format!("{} left", old.nick)));
                        send_peers(&tx, &peers);
                    }
                    continue;
                }

                let old_nick = peers.get(&pkt.peer).map(|peer| peer.nick.clone());
                let known = old_nick.is_some();
                // a peer that timed out a moment ago is coming back from a
                // network hiccup, not joining for the first time
                let returned = tombstones.remove(&pkt.peer).is_some();
                peers.insert(
                    pkt.peer.clone(),
                    Peer {
                        nick: nick.clone(),
                        last_seen: Instant::now(),
                    },
                );

                // a packet from an unknown peer is one more reason to announce
                // ourselves, the answer tells them we are here too
                if !known {
                    if !returned {
                        let _ = tx.send(NetEvent::Notice(format!("{nick} joined")));
                    }
                    send_hello(&socket, dest, &my_id, &my_nick, &my_topic);
                }
                // rename through a lost packet is repaired by any later one
                if old_nick.as_deref() != Some(nick.as_str()) {
                    send_peers(&tx, &peers);
                }

                match pkt.kind.as_str() {
                    "msg" | "action" => {
                        // confirm even a duplicate, the sender keeps retrying
                        // until every peer answers and acks can get lost too
                        if !pkt.id.is_empty() {
                            let ack = packet(&my_id, &my_nick, "ack", &pkt.id, "");
                            send_packet(&socket, dest, &ack);
                        }
                        let text = clean_text(&pkt.text);
                        if !text.is_empty() {
                            let _ = tx.send(NetEvent::Message(PeerMessage {
                                peer: pkt.peer.clone(),
                                nick: nick.clone(),
                                id: pkt.id.clone(),
                                text,
                                quote: clean_text(&pkt.quote),
                                action: pkt.kind == "action",
                            }));
                        }
                    }
                    "ack" => {
                        let done = match pending.get_mut(&pkt.id) {
                            Some(msg) => {
                                msg.waiting.retain(|(id, _)| id != &pkt.peer);
                                msg.waiting.is_empty()
                            }
                            None => false,
                        };
                        if done {
                            pending.remove(&pkt.id);
                        }
                    }
                    // the sender says the nickname changed, tell everyone,
                    // otherwise the roster only updates on the next message
                    "nick" => {
                        if let Some(old) = old_nick {
                            if old != nick {
                                let _ = tx.send(NetEvent::Notice(format!("{old} is now {nick}")));
                                send_peers(&tx, &peers);
                            }
                        }
                    }
                    "topic" => {
                        my_topic = pkt.text.clone();
                        let _ = tx.send(NetEvent::Topic(my_topic.clone()));
                        if my_topic.is_empty() {
                            let _ = tx.send(NetEvent::Notice("topic cleared".to_string()));
                        } else {
                            let _ = tx.send(NetEvent::Notice(format!("topic: {my_topic}")));
                        }
                    }
                    // newcomers learn the topic from the first hello they see
                    "hello" => {
                        if my_topic.is_empty() && !pkt.topic.is_empty() {
                            my_topic = pkt.topic.clone();
                            let _ = tx.send(NetEvent::Topic(my_topic.clone()));
                        }
                    }
                    _ => {}
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => {}
        }

        // resend whatever is still unconfirmed, give up when the deadline
        // passes and tell the ui who never answered
        let now = Instant::now();
        let mut expired: Vec<String> = Vec::new();
        for (id, msg) in pending.iter_mut() {
            if now >= msg.deadline {
                expired.push(id.clone());
                continue;
            }
            if msg.last_sent.elapsed() >= RETRY_EVERY {
                let _ = socket.send_to(&msg.bytes, dest);
                msg.last_sent = now;
            }
        }
        for id in expired {
            if let Some(msg) = pending.remove(&id) {
                // peers that already vanished are not worth reporting
                let mut names: Vec<String> = msg
                    .waiting
                    .iter()
                    .filter(|(peer, _)| peers.contains_key(peer))
                    .map(|(_, nick)| nick.clone())
                    .collect();
                names.sort();
                names.dedup();
                if !names.is_empty() {
                    let _ = tx.send(NetEvent::Undelivered {
                        to: names,
                        text: msg.text,
                    });
                }
            }
        }

        // drop peers that stopped talking, the roster is resent only on change
        if last_sweep.elapsed() >= Duration::from_secs(2) {
            last_sweep = Instant::now();
            let now = Instant::now();
            let before = peers.len();
            let mut gone: Vec<String> = Vec::new();
            peers.retain(|id, peer| {
                let alive = now.duration_since(peer.last_seen) < PEER_TTL;
                if !alive {
                    gone.push(id.clone());
                }
                alive
            });
            for id in gone {
                tombstones.insert(id, now);
            }
            tombstones.retain(|_, at| now.duration_since(*at) < TOMB_TTL);
            if peers.len() != before {
                send_peers(&tx, &peers);
            }
        }

        // a lost hello is repaired by the next one a few seconds later
        if last_hello.map_or(true, |at| at.elapsed() >= HEARTBEAT) {
            send_hello(&socket, dest, &my_id, &my_nick, &my_topic);
            last_hello = Some(Instant::now());
        }
    }

    shutdown::forget();
    send_packet(&socket, dest, &packet(&my_id, &my_nick, "bye", "", ""));
}

/// udp gives no delivery guarantee, a lost packet is simply gone
fn send_packet(socket: &UdpSocket, dest: SocketAddr, pkt: &Packet) {
    if let Ok(data) = serde_json::to_vec(pkt) {
        if data.len() <= 1400 {
            let _ = socket.send_to(&data, dest);
        }
    }
}

fn send_peers(tx: &Sender<NetEvent>, peers: &HashMap<String, Peer>) {
    let mut list: Vec<(String, String)> = peers
        .iter()
        .map(|(id, peer)| (id.clone(), peer.nick.clone()))
        .collect();
    list.sort_by(|a, b| a.1.cmp(&b.1));
    let _ = tx.send(NetEvent::Peers(list));
}
