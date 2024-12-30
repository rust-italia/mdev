use std::{
    future::Future,
    io,
    pin::Pin,
    process,
    task::{ready, Context, Poll},
};

use futures_util::{FutureExt, Stream};
use kobject_uevent::UEvent;
use netlink_sys::{
    protocols::NETLINK_KOBJECT_UEVENT, AsyncSocket, AsyncSocketExt, SocketAddr, TokioSocket,
};

type FutureOutput = (TokioSocket, Result<Vec<u8>, io::Error>);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Socket open error: {0}")]
    Open(io::Error),
    #[error("Socket bind error: {0}")]
    Bind(io::Error),
    #[error("Socket receive error: {0}")]
    Receive(io::Error),
    #[error(transparent)]
    NetlinkPacket(kobject_uevent::Error),
}

/// creates a new stream of UEvents
pub fn uevents() -> Result<impl Stream<Item = Result<UEvent, Error>>, Error> {
    let mut socket = TokioSocket::new(NETLINK_KOBJECT_UEVENT).map_err(Error::Open)?;
    let sa = SocketAddr::new(process::id(), 1);
    socket.socket_mut().bind(&sa).map_err(Error::Bind)?;

    Ok(UEventsStream::new(socket))
}

enum UEventsStream {
    Socket(TokioSocket),
    Future(Pin<Box<dyn Future<Output = FutureOutput>>>),
    None,
}

impl UEventsStream {
    pub fn new(socket: TokioSocket) -> Self {
        Self::Socket(socket)
    }

    fn take_socket(&mut self) -> Option<TokioSocket> {
        if matches!(self, Self::Socket(_)) {
            let Self::Socket(socket) = std::mem::replace(self, Self::None) else {
                unreachable!();
            };
            Some(socket)
        } else {
            None
        }
    }
}

impl Stream for UEventsStream {
    type Item = Result<UEvent, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if let Some(mut socket) = this.take_socket() {
            *this = Self::Future(Box::pin(async move {
                let res = socket.recv_from_full().await.map(|(buf, _)| buf);
                (socket, res)
            }));
        }

        if let Self::Future(fut) = this {
            let (socket, res) = ready!(fut.poll_unpin(cx));
            *this = Self::Socket(socket);
            match res {
                Ok(buf) => {
                    if buf.is_empty() {
                        return Poll::Ready(None);
                    } else {
                        return Poll::Ready(Some(
                            UEvent::from_netlink_packet(&buf).map_err(Error::NetlinkPacket),
                        ));
                    }
                }
                Err(e) => {
                    return Poll::Ready(Some(Err(Error::Receive(e))));
                }
            }
        }

        Poll::Pending
    }
}
