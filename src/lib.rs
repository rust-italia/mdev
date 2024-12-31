use std::{
    fmt,
    future::Future,
    ops::Not,
    pin::Pin,
    task::{Context, Poll},
};

use futures_util::ready;
use kobject_uevent::UEvent;
use netlink_sys::{AsyncSocket, SocketAddr, TokioSocket};
use tokio::sync::mpsc;
use tracing_subscriber::{layer::Layered, prelude::*, EnvFilter, Registry};

pub mod rule;
pub mod stream;

#[must_use = "Rebroadcaster must be awaited in order to work"]
pub struct Rebroadcaster {
    receiver: mpsc::Receiver<RebroadcastMessage>,
    socket: TokioSocket,
    socket_addr: SocketAddr,
    buffer: Vec<u8>,
    offset: usize,
}

#[inline]
fn get_rebroadcast_socket_and_socket_addr() -> std::io::Result<(TokioSocket, SocketAddr)> {
    use netlink_sys::constants;

    Ok(if cfg!(test) {
        let socket = TokioSocket::new(constants::NETLINK_USERSOCK)?;
        let socket_addr = SocketAddr::new(std::process::id(), 0);
        (socket, socket_addr)
    } else {
        let socket = TokioSocket::new(constants::NETLINK_KOBJECT_UEVENT)?;
        let socket_addr = SocketAddr::new(0, 0x4);
        (socket, socket_addr)
    })
}

impl Rebroadcaster {
    pub fn new(buffer: usize) -> std::io::Result<(Self, mpsc::Sender<RebroadcastMessage>)> {
        let (socket, socket_addr) = get_rebroadcast_socket_and_socket_addr()?;

        let (sender, receiver) = mpsc::channel(buffer);
        Ok((
            Self {
                receiver,
                socket,
                socket_addr,
                buffer: Vec::new(),
                offset: 0,
            },
            sender,
        ))
    }
}

impl Future for Rebroadcaster {
    type Output = std::io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        use std::io::Write;

        let this = self.get_mut();
        if this.buffer.is_empty().not() {
            ready!(this.send_message(cx))?;
        }
        debug_assert!(this.buffer.is_empty());
        debug_assert_eq!(this.offset, 0);

        loop {
            match ready!(this.receiver.poll_recv(cx)) {
                Some(RebroadcastMessage::Event(event)) => {
                    write!(this.buffer, "{}", DisplayEvent(&event))?;
                    ready!(this.send_message(cx))?;
                }
                Some(RebroadcastMessage::Stop) | None => break Poll::Ready(Ok(())),
            }
        }
    }
}

impl Rebroadcaster {
    fn send_message(&mut self, cx: &mut Context) -> Poll<<Self as Future>::Output> {
        let buffer_len = self.buffer.len();
        while self.offset < buffer_len {
            let bytes_sent = ready!(self.socket.poll_send_to(
                cx,
                &self.buffer[self.offset..],
                &self.socket_addr
            ))?;
            self.offset += bytes_sent;
        }

        self.buffer.clear();
        self.offset = 0;

        Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
pub enum RebroadcastMessage {
    Event(UEvent),
    Stop,
}

#[derive(Debug)]
struct DisplayEvent<'a>(&'a UEvent);

impl fmt::Display for DisplayEvent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut iter = self.0.env.iter();
        if let Some((name, value)) = iter.next() {
            write!(f, "{}={}", name, value)?;
            for (name, value) in iter {
                write!(f, "\0{}={}", name, value)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, process};

    use futures_util::{pin_mut, FutureExt};
    use kobject_uevent::ActionType;
    use netlink_sys::{constants::NETLINK_USERSOCK, AsyncSocketExt};
    use tokio::select;

    use super::*;

    fn create_event() -> UEvent {
        UEvent {
            action: ActionType::Add,
            devpath: PathBuf::from("/dev/path"),
            subsystem: "subsystem".to_string(),
            env: IntoIterator::into_iter([
                ("ACTION", "add"),
                ("DEVPATH", "/dev/path"),
                ("SUBSYSTEM", "subsystem"),
                ("SEQNUM", "1234"),
            ])
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
            seq: 1234,
        }
    }

    #[tokio::test]
    async fn rebroadcaster() {
        let (rebroadcaster, sender) = Rebroadcaster::new(2).unwrap();
        let mut socket = TokioSocket::new(NETLINK_USERSOCK).unwrap();
        let socket_addr = SocketAddr::new(process::id(), 0);
        socket.socket_mut().bind(&socket_addr).unwrap();

        sender
            .send(RebroadcastMessage::Event(create_event()))
            .await
            .unwrap();
        sender.send(RebroadcastMessage::Stop).await.unwrap();

        let recv_fut = async { socket.recv_from_full().await.unwrap().0 }.fuse();
        let rebroadcaster = rebroadcaster.fuse();
        pin_mut!(recv_fut);
        pin_mut!(rebroadcaster);

        let mut received_data = None;
        let mut rebroadcaster_done = false;
        let received_data = loop {
            select! {
                data = &mut recv_fut => {
                    if rebroadcaster_done {
                        break data;
                    } else {
                        received_data = Some(data);
                    }
                }
                result = &mut rebroadcaster => {
                    result.unwrap();
                    match received_data {
                        Some(received_data) => break received_data,
                        None => rebroadcaster_done = true,
                    }
                }
            }
        };

        assert_eq!(
            UEvent::from_netlink_packet(&received_data).unwrap(),
            create_event()
        );
    }
}

pub fn setup_log(verbose: u8) -> Layered<EnvFilter, Registry> {
    let filter_layer = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if verbose < 1 {
            EnvFilter::new("info")
        } else if verbose < 2 {
            EnvFilter::new("warn")
        } else {
            EnvFilter::new("debug")
        }
    });

    tracing_subscriber::registry().with(filter_layer)
}
