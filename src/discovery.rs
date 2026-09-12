//! UDP broadcast discovery for LAN peers.
//! Sender broadcasts `FLLE_DISCOVER_v1`, receivers reply `FLLE_HERE_v1 <host> <port> <version>`.

use anyhow::Result;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;

use crate::protocol::{DISCOVERY_MAGIC, DISCOVERY_PORT, DISCOVERY_REPLY_PREFIX};

pub async fn discovery_responder(control_port: u16) -> Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)).await?;
    sock.set_broadcast(true)?;
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "pc".to_string());
    let mut buf = [0u8; 1024];
    loop {
        let (n, peer) = match sock.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(_) => continue,
        };
        if &buf[..n] != DISCOVERY_MAGIC {
            continue;
        }
        let reply = format!(
            "{}{} {} {}",
            String::from_utf8_lossy(DISCOVERY_REPLY_PREFIX),
            hostname,
            control_port,
            crate::protocol::VERSION
        );
        let _ = sock.send_to(reply.as_bytes(), peer).await;
    }
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub addr: SocketAddr,
    pub name: String,
    pub port: u16,
}

pub async fn discover(timeout: Duration) -> Result<Vec<Peer>> {
    let sock = UdpSocket::bind(("0.0.0.0", 0)).await?;
    sock.set_broadcast(true)?;
    // Enable recv timeout via tokio::time::timeout loop.
    let bcast: SocketAddr = format!("255.255.255.255:{}", DISCOVERY_PORT).parse()?;
    // Send a few times to survive packet loss.
    for _ in 0..3 {
        let _ = sock.send_to(DISCOVERY_MAGIC, bcast).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let mut peers = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = [0u8; 1024];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let res = tokio::time::timeout(remaining, sock.recv_from(&mut buf)).await;
        let (n, addr) = match res {
            Ok(Ok(x)) => x,
            _ => break,
        };
        let msg = &buf[..n];
        if !msg.starts_with(DISCOVERY_REPLY_PREFIX) {
            continue;
        }
        let rest = String::from_utf8_lossy(&msg[DISCOVERY_REPLY_PREFIX.len()..]).into_owned();
        let mut it = rest.split_whitespace();
        let name = it.next().unwrap_or("?").to_string();
        let port: u16 = it.next().and_then(|p| p.parse().ok()).unwrap_or(crate::protocol::DEFAULT_PORT);
        let ip = addr.ip();
        peers.push(Peer {
            addr: SocketAddr::new(ip, port),
            name,
            port,
        });
    }
    peers.sort_by_key(|p| p.addr.to_string());
    peers.dedup_by_key(|p| p.addr);
    // Don't list ourselves: our own responder also hears the broadcast.
    let mine = local_ips();
    peers.retain(|p| !mine.contains(&p.addr.ip()));
    Ok(peers)
}

pub fn local_ips() -> Vec<IpAddr> {
    match local_ip_address::list_afinet_netifas() {
        Ok(list) => {
            let mut v: Vec<IpAddr> = list.into_iter().map(|(_, ip)| ip).collect();
            v.sort_by_key(|ip| ip.to_string());
            v.dedup();
            v
        }
        Err(_) => vec![],
    }
}
