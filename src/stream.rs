use std::process;

use anyhow::anyhow;

use futures_util::stream::{Stream, unfold};

use kobject_uevent::UEvent;

use netlink_sys::{TokioSocket, SocketAddr, protocols::NETLINK_KOBJECT_UEVENT};

/// creates a new stream of UEvents
pub fn uevents() -> anyhow::Result<impl Stream<Item=anyhow::Result<UEvent>>> {
    let mut socket =  TokioSocket::new(NETLINK_KOBJECT_UEVENT)
        .map_err(|e| anyhow!("Socket open error: {}", e))?;
    let sa = SocketAddr::new(process::id(), 1);
    socket.bind(&sa)?;

    Ok(unfold((socket, vec![0; 1024 * 8]), |(mut socket, mut buf)| async move {
        let n = match socket.recv_from(&mut buf).await {
            Ok((n, _addr)) => {
                if n == 0 {
                    return None;
                }
                n
            },
            Err(e) => {
                return Some((Err(anyhow!("Socket receive error: {}", e)), (socket, buf)));
            },
        };

        Some((UEvent::from_netlink_packet(&buf[0..n]), (socket, buf)))
    }))
}
