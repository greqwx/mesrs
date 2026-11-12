// Copyright (c) 2026 Greg

//! text cleanup and width aware helpers used for drawing and for the wire

use unicode_width::UnicodeWidthChar;

/// hard cap for one message, keeps a packet far below the ethernet mtu
pub const MAX_TEXT: usize = 500;
/// byte cap, a string of wide characters must not push a packet over the limit
const MAX_TEXT_BYTES: usize = 900;

/// removes control characters, trims and caps the nickname
pub fn sanitize_nick(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(16)
        .collect()
}

/// falls back to a fixed name when nothing usable is left
pub fn display_nick(raw: &str) -> String {
    let nick = sanitize_nick(raw);
    if nick.is_empty() {
        "anon".to_string()
    } else {
        nick
    }
}

/// drops control characters so escape sequences from the network cannot reach
/// the terminal, then trims and caps the length
pub fn clean_text(raw: &str) -> String {
    let mut out = String::new();
    let mut count = 0;
    for ch in raw.chars() {
        if ch.is_control() {
            continue;
        }
        if count >= MAX_TEXT {
            break;
        }
        if out.len() + ch.len_utf8() > MAX_TEXT_BYTES {
            break;
        }
        out.push(ch);
        count += 1;
    }
    out.trim().to_string()
}

/// terminal columns a string occupies, wide chars count as two
pub fn visible_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// cuts a string to `width` columns and pads it with spaces, used for the bars
pub fn fit(text: &str, width: usize) -> String {
    let mut out = String::with_capacity(width);
    let mut used = 0;
    for ch in text.chars() {
        let w = char_width(ch);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    while used < width {
        out.push(' ');
        used += 1;
    }
    out
}

/// cuts a string to `width` columns, no padding
pub fn cut(text: &str, width: usize) -> String {
    let mut out = String::with_capacity(width);
    let mut used = 0;
    for ch in text.chars() {
        let w = char_width(ch);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// keeps the end of a long input line visible, the start is scrolled off
pub fn tail_fit(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut chars: Vec<char> = Vec::new();
    for ch in text.chars().rev() {
        let w = char_width(ch);
        if used + w > width {
            break;
        }
        chars.push(ch);
        used += w;
    }
    chars.reverse();
    chars.into_iter().collect()
}

/// splits text into lines that fit `width` columns
///
/// words are kept whole when possible, a word longer than the line is cut
/// character by character, and runs of whitespace collapse into one space
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut used = 0;

    for word in text.split_whitespace() {
        let word_w = visible_width(word);
        if word_w <= width {
            let extra = usize::from(!line.is_empty());
            if used + extra + word_w > width {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += word_w;
        } else {
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            let mut chunk = String::new();
            let mut chunk_w = 0;
            for ch in word.chars() {
                let w = char_width(ch);
                if chunk_w + w > width && !chunk.is_empty() {
                    out.push(std::mem::take(&mut chunk));
                    chunk_w = 0;
                }
                chunk.push(ch);
                chunk_w += w;
            }
            line = chunk;
            used = chunk_w;
        }
    }

    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}
