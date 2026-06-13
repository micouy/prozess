//! Listening-TCP-port discovery for a process group.
//!
//! The system is scanned once per request (`Discovery::collect`) and then
//! queried per process group, so listing N processes costs one scan, not
//! N. On Linux, where sandboxed kernels often disable netlink `sock_diag`
//! (which `netstat2` needs), discovery falls back to parsing
//! `/proc/net/tcp{,6}` and matching socket inodes to each pid's open fds.

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
            #[cfg(target_os = "linux")]
            {
                if let Some(listeners) = linux_proc::scan() {
                    return Some(listeners);
                }
            }
            eprintln!("port discovery unavailable: {error}");
            None
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_proc {
    use std::collections::HashMap;

    use super::Listener;

    /// Parses `/proc/net/tcp{,6}` for listening sockets and resolves their
    /// owning pids through the inode→fd reverse map. Returns `None` only if
    /// `/proc/net` itself cannot be read (then the caller reports
    /// unavailable); an empty list means "scanned, nothing listening".
    pub fn scan() -> Option<Vec<Listener>> {
        let mut listeners = parse_net_tcp("/proc/net/tcp")?;
        if let Some(v6) = parse_net_tcp("/proc/net/tcp6") {
            listeners.extend(v6);
        }

        let inode_pids = inode_to_pids();

        Some(
            listeners
                .into_iter()
                .map(|listener| Listener {
                    local_addr: listener.local_addr,
                    local_port: listener.local_port,
                    pids: inode_pids.get(&listener.inode).cloned().unwrap_or_default(),
                })
                .collect(),
        )
    }

    struct ProcListener {
        local_addr: String,
        local_port: u16,
        inode: u64,
    }

    // 0A == TCP_LISTEN in the hex state column of /proc/net/tcp.
    const TCP_LISTEN: &str = "0A";

    fn parse_net_tcp(path: &str) -> Option<Vec<ProcListener>> {
        let contents = std::fs::read_to_string(path).ok()?;
        let mut listeners = Vec::new();

        for line in contents.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let local = fields.nth(1)?; // skip sl, take local_address
            let state = fields.next()?;
            // st, then tx/rx queue, tr/when, retrnsmt, uid, timeout, inode
            let inode = fields.nth(5)?;

            if state != TCP_LISTEN {
                continue;
            }
            let (addr, port) = parse_addr(local)?;
            let inode = inode.parse().ok()?;

            listeners.push(ProcListener {
                local_addr: addr,
                local_port: port,
                inode,
            });
        }

        Some(listeners)
    }

    fn parse_addr(field: &str) -> Option<(String, u16)> {
        let (addr_hex, port_hex) = field.split_once(':')?;
        let port = u16::from_str_radix(port_hex, 16).ok()?;

        let addr = match addr_hex.len() {
            8 => {
                let octets = u32::from_str_radix(addr_hex, 16).ok()?.to_le_bytes();
                std::net::Ipv4Addr::from(octets).to_string()
            }
            32 => {
                let mut bytes = [0u8; 16];
                for (index, chunk) in addr_hex.as_bytes().chunks(2).enumerate() {
                    let pair = std::str::from_utf8(chunk).ok()?;
                    bytes[index] = u8::from_str_radix(pair, 16).ok()?;
                }
                // Each 32-bit word is little-endian in /proc.
                for word in bytes.chunks_mut(4) {
                    word.reverse();
                }
                std::net::Ipv6Addr::from(bytes).to_string()
            }
            _ => return None,
        };

        Some((addr, port))
    }

    /// Maps socket inodes to the pids holding them open, by walking
    /// `/proc/<pid>/fd` for `socket:[<inode>]` symlinks.
    fn inode_to_pids() -> HashMap<u64, Vec<u32>> {
        let mut map: HashMap<u64, Vec<u32>> = HashMap::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return map;
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
                continue;
            };

            for fd in fds.flatten() {
                let Ok(target) = std::fs::read_link(fd.path()) else {
                    continue;
                };
                if let Some(inode) = target
                    .to_str()
                    .and_then(|target| target.strip_prefix("socket:["))
                    .and_then(|rest| rest.strip_suffix(']'))
                    .and_then(|inode| inode.parse::<u64>().ok())
                {
                    map.entry(inode).or_default().push(pid);
                }
            }
        }

        map
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_ipv4_listener() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("tcp");
            // 0.0.0.0:8080 (8080 = 0x1F90), LISTEN (0A), inode 4242.
            std::fs::write(
                &path,
                "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000 100 0 0 10 0\n",
            )
            .unwrap();

            let listeners = parse_net_tcp(path.to_str().unwrap()).unwrap();
            assert_eq!(listeners.len(), 1);
            assert_eq!(listeners[0].local_addr, "0.0.0.0");
            assert_eq!(listeners[0].local_port, 8080);
            assert_eq!(listeners[0].inode, 4242);
        }

        #[test]
        fn skips_non_listening_rows() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("tcp");
            // 01 == ESTABLISHED, must be ignored.
            std::fs::write(
                &path,
                "  sl  local_address rem_address   st\n   0: 0100007F:1F90 0100007F:9999 01 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000 100 0 0 10 0\n",
            )
            .unwrap();

            assert!(parse_net_tcp(path.to_str().unwrap()).unwrap().is_empty());
        }

        #[test]
        fn parses_ipv6_loopback() {
            // ::1, word-reversed per /proc layout.
            let field = "00000000000000000000000001000000:1F90";
            let (addr, port) = parse_addr(field).unwrap();
            assert_eq!(addr, "::1");
            assert_eq!(port, 8080);
        }
    }
}
