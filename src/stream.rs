use std::process;

use anyhow::anyhow;

use futures_util::stream::{unfold, Stream};

use kobject_uevent::UEvent;

use netlink_sys::{
    protocols::NETLINK_KOBJECT_UEVENT, AsyncSocket, AsyncSocketExt, SocketAddr, TokioSocket,
};

/// creates a new stream of UEvents
pub fn uevents() -> anyhow::Result<impl Stream<Item = anyhow::Result<UEvent>>> {
    let mut socket = TokioSocket::new(NETLINK_KOBJECT_UEVENT)
        .map_err(|e| anyhow!("Socket open error: {}", e))?;
    let sa = SocketAddr::new(process::id(), 1);
    socket
        .socket_mut()
        .bind(&sa)
        .map_err(|e| anyhow!("Socket bind error: {}", e))?;

    Ok(unfold(
        (socket, bytes::BytesMut::with_capacity(1024 * 8)),
        |(mut socket, mut buf)| async move {
            buf.clear();
            match socket.recv_from(&mut buf).await {
                Ok(_addr) => {
                    if buf.len() == 0 {
                        return None;
                    }
                }
                Err(e) => {
                    return Some((Err(anyhow!("Socket receive error: {}", e)), (socket, buf)));
                }
            };

            Some((UEvent::from_netlink_packet(&buf), (socket, buf)))
        },
    ))
}
