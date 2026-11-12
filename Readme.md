# Mesrs
Small LAN chat in the terminal.

## Running

Grab the archive for your platform from the releases page, unpack it and run the
binary with a group code:

    mesrs 123456 nickname

From source, with Rust 1.85 or newer:

    cargo build --release --locked
    ./target/release/mesrs 123456 greg


## How it works

A group code maps to an IPv4 multicast group inside `239.255.0.0/16` and a port<br>
starting at 25100, so the million possible codes never collide. Packets are plain<br>
JSON over UDP. If the network blocks multicast, the client falls back to broadcast on the same port.
