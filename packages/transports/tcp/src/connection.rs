use crate::msg::TcpMsg;
use async_bincode::futures::AsyncBincodeStream;
use async_bincode::AsyncDestination;
use async_std::channel::{bounded, unbounded, Receiver, RecvError, Sender};
use async_std::net::{Shutdown, TcpStream};
use async_std::task::JoinHandle;
use bluesea_identity::{ConnId, NodeAddr, NodeId};
use futures_util::io::{ReadHalf, WriteHalf};
use futures_util::{
    select, sink::Sink, AsyncReadExt, AsyncWriteExt, FutureExt, SinkExt, StreamExt,
};
use network::transport::{
    ConnectionEvent, ConnectionMsg, ConnectionReceiver, ConnectionSender, ConnectionStats,
};
use serde::{de::DeserializeOwned, Serialize};
use std::sync::Arc;
use std::time::Duration;
use utils::Timer;

pub type AsyncBincodeStreamU16<MSG> =
    AsyncBincodeStream<TcpStream, TcpMsg<MSG>, TcpMsg<MSG>, AsyncDestination>;

pub const BUFFER_LEN: usize = 16384;

pub async fn send_tcp_stream<MSG: Serialize>(
    writer: &mut AsyncBincodeStreamU16<MSG>,
    msg: TcpMsg<MSG>,
) -> Result<(), ()> {
    match writer.send(msg).await {
        Ok(_) => Ok(()),
        Err(err) => {
            log::error!("[TcpTransport] write buffer error {:?}", err);
            Err(())
        }
    }
}

pub enum OutgoingEvent<MSG> {
    Msg(TcpMsg<MSG>),
    CloseRequest,
    ClosedNotify,
}

pub struct TcpConnectionSender<MSG> {
    remote_node_id: NodeId,
    remote_addr: NodeAddr,
    conn_id: ConnId,
    reliable_sender: Sender<OutgoingEvent<MSG>>,
    unreliable_sender: Sender<OutgoingEvent<MSG>>,
    task: Option<JoinHandle<()>>,
}

impl<MSG> TcpConnectionSender<MSG>
where
    MSG: Serialize + Send + Sync + 'static,
{
    pub fn new(
        node_id: NodeId,
        remote_node_id: NodeId,
        remote_addr: NodeAddr,
        conn_id: ConnId,
        unreliable_queue_size: usize,
        mut socket: AsyncBincodeStreamU16<MSG>,
        timer: Arc<dyn Timer>,
    ) -> (Self, Sender<OutgoingEvent<MSG>>) {
        let (reliable_sender, mut r_rx) = unbounded();
        let (unreliable_sender, mut unr_rx) = bounded(unreliable_queue_size);

        let task = async_std::task::spawn(async move {
            log::info!(
                "[TcpConnectionSender {} => {}] start sending loop",
                node_id,
                remote_node_id
            );
            let mut tick_interval = async_std::stream::interval(Duration::from_millis(5000));
            send_tcp_stream(&mut socket, TcpMsg::<MSG>::Ping(timer.now_ms())).await;

            loop {
                let msg: Result<OutgoingEvent<MSG>, RecvError> = select! {
                    e = r_rx.recv().fuse() => e,
                    e = unr_rx.recv().fuse() => e,
                    e = tick_interval.next().fuse() => {
                        log::debug!("[TcpConnectionSender {} => {}] sending Ping", node_id, remote_node_id);
                        Ok(OutgoingEvent::Msg(TcpMsg::Ping(timer.now_ms())))
                    }
                };

                match msg {
                    Ok(OutgoingEvent::Msg(msg)) => {
                        if let Err(e) = send_tcp_stream(&mut socket, msg).await {}
                    }
                    Ok(OutgoingEvent::CloseRequest) => {
                        if let Err(e) = socket.get_mut().shutdown(Shutdown::Both) {
                            log::error!(
                                "[TcpConnectionSender {} => {}] close sender error {}",
                                node_id,
                                remote_node_id,
                                e
                            );
                        } else {
                            log::info!(
                                "[TcpConnectionSender {} => {}] close sender loop",
                                node_id,
                                remote_node_id
                            );
                        }
                        break;
                    }
                    Ok(OutgoingEvent::ClosedNotify) => {
                        log::info!(
                            "[TcpConnectionSender {} => {}] socket closed",
                            node_id,
                            remote_node_id
                        );
                        break;
                    }
                    Err(err) => {
                        log::error!(
                            "[TcpConnectionSender {} => {}] channel error {:?}",
                            node_id,
                            remote_node_id,
                            err
                        );
                        break;
                    }
                }
            }
            log::info!(
                "[TcpConnectionSender {} => {}] stop sending loop",
                node_id,
                remote_node_id
            );
            ()
        });

        (
            Self {
                remote_addr,
                remote_node_id,
                conn_id,
                reliable_sender: reliable_sender.clone(),
                unreliable_sender,
                task: Some(task),
            },
            reliable_sender,
        )
    }
}

impl<MSG> ConnectionSender<MSG> for TcpConnectionSender<MSG>
where
    MSG: Send + Sync + 'static,
{
    fn remote_node_id(&self) -> NodeId {
        self.remote_node_id
    }

    fn conn_id(&self) -> ConnId {
        self.conn_id
    }

    fn remote_addr(&self) -> NodeAddr {
        self.remote_addr.clone()
    }

    fn send(&self, service_id: u8, msg: ConnectionMsg<MSG>) {
        match &msg {
            ConnectionMsg::Reliable { .. } => {
                if let Err(e) = self
                    .reliable_sender
                    .send_blocking(OutgoingEvent::Msg(TcpMsg::Msg(service_id, msg)))
                {
                    log::error!("[ConnectionSender] send reliable msg error {:?}", e);
                } else {
                    log::debug!("[ConnectionSender] send reliable msg");
                }
            }
            ConnectionMsg::Unreliable { .. } => {
                if let Err(e) = self
                    .unreliable_sender
                    .try_send(OutgoingEvent::Msg(TcpMsg::Msg(service_id, msg)))
                {
                    log::error!("[ConnectionSender] send unreliable msg error {:?}", e);
                } else {
                    log::debug!("[ConnectionSender] send unreliable msg");
                }
            }
        }
    }

    fn close(&self) {
        if let Err(e) = self
            .unreliable_sender
            .send_blocking(OutgoingEvent::CloseRequest)
        {
            log::error!("[ConnectionSender] send Close request error {:?}", e);
        } else {
            log::info!("[ConnectionSender] sent close request");
        }
    }
}

impl<MSG> Drop for TcpConnectionSender<MSG> {
    fn drop(&mut self) {
        if let Some(mut task) = self.task.take() {
            task.cancel();
        }
    }
}

pub async fn recv_tcp_stream<MSG: DeserializeOwned>(
    reader: &mut AsyncBincodeStreamU16<MSG>,
) -> Result<TcpMsg<MSG>, ()> {
    if let Some(res) = reader.next().await {
        res.map_err(|_| ())
    } else {
        Err(())
    }
}

pub struct TcpConnectionReceiver<MSG> {
    pub(crate) node_id: NodeId,
    pub(crate) remote_node_id: NodeId,
    pub(crate) remote_addr: NodeAddr,
    pub(crate) conn_id: ConnId,
    pub(crate) socket: AsyncBincodeStreamU16<MSG>,
    pub(crate) timer: Arc<dyn Timer>,
    pub(crate) reliable_sender: Sender<OutgoingEvent<MSG>>,
}

#[async_trait::async_trait]
impl<MSG> ConnectionReceiver<MSG> for TcpConnectionReceiver<MSG>
where
    MSG: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn remote_node_id(&self) -> NodeId {
        self.remote_node_id
    }

    fn conn_id(&self) -> ConnId {
        self.conn_id
    }

    fn remote_addr(&self) -> NodeAddr {
        self.remote_addr.clone()
    }

    async fn poll(&mut self) -> Result<ConnectionEvent<MSG>, ()> {
        loop {
            log::debug!(
                "[ConnectionReceiver {} => {}] waiting event",
                self.node_id,
                self.remote_node_id
            );
            match recv_tcp_stream::<MSG>(&mut self.socket).await {
                Ok(msg) => {
                    match msg {
                        TcpMsg::Msg(service_id, msg) => {
                            break Ok(ConnectionEvent::Msg { service_id, msg });
                        }
                        TcpMsg::Ping(sent_ts) => {
                            log::debug!(
                                "[ConnectionReceiver {} => {}] on Ping => reply Pong",
                                self.node_id,
                                self.remote_node_id
                            );
                            self.reliable_sender
                                .send_blocking(OutgoingEvent::Msg(TcpMsg::<MSG>::Pong(sent_ts)));
                        }
                        TcpMsg::Pong(ping_sent_ts) => {
                            //TODO est speed and over_use state
                            log::debug!(
                                "[ConnectionReceiver {} => {}] on Pong",
                                self.node_id,
                                self.remote_node_id
                            );
                            break Ok(ConnectionEvent::Stats(ConnectionStats {
                                rtt_ms: (self.timer.now_ms() - ping_sent_ts) as u16,
                                sending_kbps: 0,
                                send_est_kbps: 0,
                                loss_percent: 0,
                                over_use: false,
                            }));
                        }
                        _ => {
                            log::warn!("[ConnectionReceiver {} => {}] wrong msg type, required TcpMsg::Msg", self.node_id, self.remote_node_id);
                        }
                    }
                }
                Err(e) => {
                    log::info!(
                        "[ConnectionReceiver {} => {}] stream closed",
                        self.node_id,
                        self.remote_node_id
                    );
                    self.reliable_sender
                        .send_blocking(OutgoingEvent::ClosedNotify);
                    break Err(());
                }
            }
        }
    }
}
