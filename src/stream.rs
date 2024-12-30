use std::process;

use anyhow::anyhow;
use async_stream::try_stream;
use futures_util::Stream;
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

    Ok(try_stream! {
        loop {
            let (buf, _sock) = socket.recv_from_full().await?;
            yield UEvent::from_netlink_packet(&buf)?;
        }
    })
}
