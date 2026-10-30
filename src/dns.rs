use std::net::{Ipv4Addr, UdpSocket};
use std::thread;

use anyhow::Result;

/// Stack for the DNS thread; it only handles tiny packets in a fixed buffer.
const STACK_SIZE: usize = 4096;
/// Standard DNS messages over UDP are at most 512 bytes.
const PACKET_SIZE: usize = 512;
/// Short TTL so a phone doesn't cling to the answer after leaving the network.
const TTL_SECONDS: u32 = 30;

/// Answers every DNS query with `ip`, which is what makes this a captive portal: a phone
/// joined to the access point can resolve any hostname (including the connectivity-check
/// hosts it probes on joining) and gets this device, instead of failing with no DNS
/// server and falling back to mobile data.
pub fn start(ip: Ipv4Addr) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:53")?;

    thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let mut buf = [0u8; PACKET_SIZE];
            loop {
                let Ok((len, from)) = socket.recv_from(&mut buf) else {
                    continue;
                };
                if let Some(reply_len) = build_reply(&mut buf, len, ip) {
                    let _ = socket.send_to(&buf[..reply_len], from);
                }
            }
        })?;

    log::info!("DNS catch-all up - every name resolves to {ip}");
    Ok(())
}

/// Turns the query in `buf[..len]` into a reply in place, returning the reply length.
/// A-record questions get `ip`; any other type (AAAA, HTTPS, ...) gets an empty, successful
/// answer so clients move on to IPv4 rather than waiting for a timeout.
fn build_reply(buf: &mut [u8; PACKET_SIZE], len: usize, ip: Ipv4Addr) -> Option<usize> {
    // 12-byte header, and only plain single-question queries (not responses).
    if len < 12 || buf[2] & 0x80 != 0 || buf[4] != 0 || buf[5] != 1 {
        return None;
    }

    // Walk the question's name labels to find where the type/class fields start.
    let mut pos = 12;
    loop {
        let label = *buf.get(pos)? as usize;
        pos += 1;
        if label == 0 {
            break;
        }
        pos += label;
    }
    let question_end = pos + 4;
    if question_end > len {
        return None;
    }
    // pos..pos+2 is QTYPE (A = 1), pos+2..pos+4 is QCLASS (IN = 1).
    let is_a_record = buf[pos..pos + 2] == [0, 1] && buf[pos + 2..pos + 4] == [0, 1];

    // Header: response, recursion available, no error; keep the query's id and RD bit.
    buf[2] = 0x80 | (buf[2] & 0x01);
    buf[3] = 0x80;
    buf[6] = 0;
    buf[8..12].fill(0);

    if !is_a_record {
        buf[7] = 0;
        return Some(question_end);
    }

    buf[7] = 1;
    let answer = &mut buf[question_end..question_end + 16];
    answer[0..2].copy_from_slice(&[0xC0, 0x0C]); // pointer back to the question's name
    answer[2..4].copy_from_slice(&[0, 1]); // TYPE A
    answer[4..6].copy_from_slice(&[0, 1]); // CLASS IN
    answer[6..10].copy_from_slice(&TTL_SECONDS.to_be_bytes());
    answer[10..12].copy_from_slice(&[0, 4]); // RDLENGTH
    answer[12..16].copy_from_slice(&ip.octets());
    Some(question_end + 16)
}
