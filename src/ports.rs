//! Listening-TCP-port discovery for a process group.
//!
//! The system is scanned once per request (`Discovery::collect`) and then
//! queried per process group, so listing N processes costs one scan, not
//! N.

use netstat2::{AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};

use crate::protocol::PortInfo;

/// A listening socket and the pids that own it, gathered once.
struct Listener {
    local_addr: String,
    local_port: u16,
    pids: Vec<u32>,
}

pub struct Discovery {
    listeners: Option<Vec<Listener>>,
}

impl Discovery {
    /// Scans every listening TCP socket once. `None` listeners means
    /// discovery is genuinely unavailable (rendered as `?`).
    pub fn collect() -> Self {
        Self {
            listeners: scan_listeners(),
        }
    }

    /// Listening ports owned by any pid in `pids`. `None` propagates an
    /// unavailable scan.
    pub fn ports_for(&self, pids: &[u32]) -> Option<Vec<PortInfo>> {
        let listeners = self.listeners.as_ref()?;
        let mut ports = listeners
            .iter()
            .filter_map(|listener| {
                let owners = listener
                    .pids
                    .iter()
                    .copied()
                    .filter(|pid| pids.contains(pid))
                    .collect::<Vec<_>>();
                if owners.is_empty() {
                    return None;
                }

                Some(PortInfo {
                    protocol: "tcp".to_owned(),
                    state: "listen".to_owned(),
                    local_addr: listener.local_addr.clone(),
                    local_port: listener.local_port,
                    pids: owners,
                })
            })
            .collect::<Vec<_>>();

        ports.sort_by(|a, b| (a.local_port, &a.local_addr).cmp(&(b.local_port, &b.local_addr)));
        Some(ports)
    }
}

fn scan_listeners() -> Option<Vec<Listener>> {
    match netstat2::get_sockets_info(
        AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6,
        ProtocolFlags::TCP,
    ) {
        Ok(sockets) => Some(
            sockets
                .into_iter()
                .filter_map(|socket| {
                    let ProtocolSocketInfo::Tcp(tcp) = socket.protocol_socket_info else {
                        return None;
                    };
                    if tcp.state != TcpState::Listen {
                        return None;
                    }

                    Some(Listener {
                        local_addr: tcp.local_addr.to_string(),
                        local_port: tcp.local_port,
                        pids: socket.associated_pids,
                    })
                })
                .collect(),
        ),
        Err(error) => {
            eprintln!("port discovery unavailable: {error}");
            None
        }
    }
}
