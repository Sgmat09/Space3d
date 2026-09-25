//! WebSocket server transport: a hardened `space3d_net::WsServer` (16 KiB inbound cap, bounded
//! queues, text frames rejected, admission hook for bans / `Origin`) turned into a stream of
//! `LinkEvent`s, with one [`WsLinkTx`] per client.

use crate::clock::{SystemUnixClock, UnixClock};
use crate::security::bans::BanList;
use crate::security::netaddr::{Cidr, resolve_client_ip};
use space3d_net::{ClientId, ClientSender, ConnectMeta, Message, NetError, ServerEvent, WsServer, WsServerConfig};
use space3d_replication::LinkId;
use space3d_replication::transport::{CloseCode, Lane, LinkEvent, LinkTx, PeerInfo, TransportCaps, TransportError};
use std::sync::Arc;

/// Largest server → client packet (clients accept 1 MiB; the *inbound* cap is 16 KiB).
pub const MAX_OUTBOUND_PACKET: usize = 1 << 20;

/// Server-side WS transport.
#[derive(Clone, Debug)]
pub struct WsServerTransport {
    ws: WsServer,
    caps: TransportCaps,
    trusted: Arc<Vec<Cidr>>,
}

/// Admission policy applied at the HTTP upgrade (before a socket exists): IP/CIDR bans and the
/// browser `Origin` allowlist. Per-IP concurrency and handshake rate are enforced by the
/// gatekeeper, which sees every open/close.
#[derive(Clone)]
pub struct AdmissionPolicy {
    /// Bans (shared with the gatekeeper / fleet link).
    pub bans: BanList,
    /// Allowed browser origins (empty = any). Non-browser clients send no `Origin` and pass.
    pub allowed_origins: Vec<String>,
    /// Proxies whose `X-Forwarded-For` is trusted.
    pub trusted_proxies: Vec<Cidr>,
    /// Wall clock for ban expiry.
    pub clock: Arc<dyn UnixClock>,
}

impl Default for AdmissionPolicy {
    fn default() -> Self {
        Self {
            bans: BanList::default(),
            allowed_origins: Vec::new(),
            trusted_proxies: Vec::new(),
            clock: Arc::new(SystemUnixClock),
        }
    }
}

impl std::fmt::Debug for AdmissionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionPolicy")
            .field("allowed_origins", &self.allowed_origins)
            .field("trusted_proxies", &self.trusted_proxies)
            .finish_non_exhaustive()
    }
}

impl AdmissionPolicy {
    /// The accept hook: `Err(403)` for banned IPs and disallowed origins.
    pub fn check(&self, meta: &ConnectMeta) -> Result<(), u16> {
        let ip = resolve_client_ip(meta.addr, meta.forwarded_for.as_deref(), &self.trusted_proxies);
        if self.bans.ip(ip, self.clock.now_ms()).is_some() {
            return Err(403);
        }
        if !origin_allowed(&self.allowed_origins, meta.origin.as_deref()) {
            return Err(403);
        }
        Ok(())
    }
}

/// Whether `origin` is in `allowed` (empty list = any; a missing `Origin` = non-browser, allowed).
pub fn origin_allowed(allowed: &[String], origin: Option<&str>) -> bool {
    match origin {
        None => true,
        Some(o) => {
            let o = o.trim_end_matches('/');
            allowed.is_empty() || allowed.iter().any(|a| a.trim_end_matches('/').eq_ignore_ascii_case(o))
        }
    }
}

impl WsServerTransport {
    /// Create with limits (`max_message_size` = the inbound packet cap; text frames rejected).
    /// Links advertise [`MAX_OUTBOUND_PACKET`] as their send cap: that is what clients accept,
    /// and a single large entity record must fit (the 16 KiB cap only applies inbound).
    pub fn new(config: WsServerConfig) -> Self {
        let caps = TransportCaps::stream(MAX_OUTBOUND_PACKET.max(config.max_message_size));
        Self {
            ws: WsServer::with_config(config.accept_text(false)),
            caps,
            trusted: Arc::new(Vec::new()),
        }
    }

    /// Standard game-port transport: `max_message_bytes` inbound cap, `outbound_limit` bytes
    /// per link, `max_connections`, no text frames, plus the [`AdmissionPolicy`] accept hook.
    pub fn hardened(
        max_message_bytes: usize,
        outbound_limit: usize,
        max_connections: usize,
        policy: AdmissionPolicy,
    ) -> Self {
        let trusted = policy.trusted_proxies.clone();
        let config = WsServerConfig::default()
            .max_message_size(max_message_bytes)
            .outbound_limit_bytes(outbound_limit)
            .max_connections(max_connections)
            .inbound_queue(65_536)
            .accept_text(false)
            .accept(move |meta| policy.check(meta));
        Self::new(config).with_trusted_proxies(trusted)
    }

    /// Resolve `X-Forwarded-For` only from these proxies.
    pub fn with_trusted_proxies(mut self, trusted: Vec<Cidr>) -> Self {
        self.trusted = Arc::new(trusted);
        self
    }

    /// The underlying server (mount with `HttpServer::ws(path, transport.ws())`).
    pub fn ws(&self) -> &WsServer {
        &self.ws
    }

    /// `LinkId` of a WS client (`ClientId` → `LinkId(id)`).
    pub fn link_id(client: ClientId) -> LinkId {
        LinkId(client.0)
    }

    /// Await the next event (ingress task only; never from a system). `None` once the server
    /// is gone.
    pub async fn next_event(&self) -> Option<LinkEvent> {
        loop {
            let ev = self.ws.recv().await?;
            if let Some(ev) = self.convert(ev) {
                return Some(ev);
            }
        }
    }

    /// Non-blocking variant of [`Self::next_event`].
    pub fn try_event(&self) -> Option<LinkEvent> {
        loop {
            let ev = self.ws.try_recv()?;
            if let Some(ev) = self.convert(ev) {
                return Some(ev);
            }
        }
    }

    fn convert(&self, ev: ServerEvent) -> Option<LinkEvent> {
        match ev {
            ServerEvent::Connected { client, addr } => {
                let meta = self.ws.client_meta(client);
                let forwarded = meta.as_ref().and_then(|m| m.forwarded_for.clone());
                let peer = PeerInfo {
                    addr: Some(addr),
                    client_ip: Some(resolve_client_ip(addr, forwarded.as_deref(), &self.trusted)),
                    origin: meta.as_ref().and_then(|m| m.origin.clone()),
                    user_agent: meta.and_then(|m| m.user_agent),
                    transport: "ws",
                };
                Some(LinkEvent::Opened {
                    link: Self::link_id(client),
                    peer,
                    tx: Arc::new(WsLinkTx {
                        sender: self.ws.client_sender(client),
                        client,
                        caps: self.caps,
                    }),
                })
            }
            ServerEvent::Message { client, message } => match message {
                Message::Binary(data) => Some(LinkEvent::Packet {
                    link: Self::link_id(client),
                    lane: Lane::Reliable,
                    data,
                }),
                // `accept_text = false` already closes text senders in the socket task.
                Message::Text(_) => {
                    self.ws.disconnect_with(client, 1003, "text frames not accepted");
                    None
                }
            },
            ServerEvent::Disconnected { client, reason } => Some(LinkEvent::Closed {
                link: Self::link_id(client),
                reason,
            }),
        }
    }
}

/// Sender for one WS client. Holds the client's own queue handle ([`ClientSender`]), so room
/// ticks on shard threads never take the `WsServer`-wide client lock to send, check the
/// backlog or check liveness.
#[derive(Debug)]
pub struct WsLinkTx {
    /// `None`: the client was already gone when its link was announced.
    sender: Option<ClientSender>,
    client: ClientId,
    caps: TransportCaps,
}

impl WsLinkTx {
    /// The WS client behind this link.
    pub fn client(&self) -> ClientId {
        self.client
    }
}

impl LinkTx for WsLinkTx {
    fn caps(&self) -> TransportCaps {
        self.caps
    }

    fn send(&self, _lane: Lane, packet: Vec<u8>) -> Result<(), TransportError> {
        if packet.len() > MAX_OUTBOUND_PACKET {
            return Err(TransportError::TooLarge {
                len: packet.len(),
                max: MAX_OUTBOUND_PACKET,
            });
        }
        let Some(sender) = &self.sender else {
            return Err(TransportError::Closed);
        };
        match sender.send(Message::Binary(packet)) {
            Ok(()) => Ok(()),
            // Over the outbound byte cap: the socket task closes the client with 1008.
            Err(NetError::Closed) if sender.is_connected() => Err(TransportError::Backpressure),
            Err(_) => Err(TransportError::Closed),
        }
    }

    fn backlog_bytes(&self) -> usize {
        self.sender
            .as_ref()
            .filter(|s| s.is_connected())
            .map_or(0, ClientSender::backlog_bytes)
    }

    fn close(&self, code: CloseCode, reason: &str) {
        if let Some(s) = &self.sender {
            s.close_with(code.0, reason);
        }
    }

    fn is_open(&self) -> bool {
        self.sender.as_ref().is_some_and(ClientSender::is_connected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::bans::{Ban, BanTarget};

    #[test]
    fn origin_allowlist() {
        let allowed = vec!["https://play.example.com".to_owned()];
        assert!(origin_allowed(&allowed, None), "native clients send no Origin");
        assert!(origin_allowed(&allowed, Some("https://play.example.com")));
        assert!(origin_allowed(&allowed, Some("https://PLAY.example.com/")));
        assert!(!origin_allowed(&allowed, Some("https://evil.example")));
        assert!(origin_allowed(&[], Some("https://anything")));
    }

    #[test]
    fn admission_rejects_banned_ips_and_origins() {
        let bans = BanList::default();
        bans.upsert(Ban {
            id: 1,
            target: BanTarget::Ip(Cidr::parse("203.0.113.0/24").unwrap()),
            until_ms: None,
            reason: "abuse".into(),
        });
        let policy = AdmissionPolicy {
            bans,
            allowed_origins: vec!["https://ok.example".into()],
            trusted_proxies: vec![Cidr::parse("10.0.0.0/8").unwrap()],
            clock: Arc::new(crate::clock::ManualUnixClock::at(0)),
        };
        let meta = |addr: &str, origin: Option<&str>, xff: Option<&str>| ConnectMeta {
            addr: addr.parse().unwrap(),
            origin: origin.map(str::to_owned),
            forwarded_for: xff.map(str::to_owned),
            user_agent: None,
            authorization: None,
        };
        assert_eq!(policy.check(&meta("203.0.113.5:1", None, None)), Err(403));
        assert_eq!(policy.check(&meta("198.51.100.5:1", None, None)), Ok(()));
        assert_eq!(
            policy.check(&meta("10.0.0.2:1", None, Some("203.0.113.9"))),
            Err(403),
            "banned client behind a trusted proxy"
        );
        assert_eq!(
            policy.check(&meta("198.51.100.5:1", Some("https://evil.example"), None)),
            Err(403)
        );
        assert_eq!(
            policy.check(&meta("198.51.100.5:1", Some("https://ok.example"), None)),
            Ok(())
        );
    }
}
