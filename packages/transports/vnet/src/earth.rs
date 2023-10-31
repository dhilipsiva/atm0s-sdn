use crate::connection::{VnetConnectionReceiver, VnetConnectionSender};
use crate::listener::{VnetListener, VnetListenerEvent};
use crate::VNET_PROTOCOL_ID;
use async_std::channel::{unbounded, Sender};
use bluesea_identity::{ConnId, NodeAddr, NodeId};
use network::transport::{AsyncConnectionAcceptor, ConnectionRejectReason, ConnectionStats, OutgoingConnectionError};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub(crate) struct Socket {
    node: NodeId,
    addr: NodeAddr,
    sender: Sender<VnetListenerEvent>,
}

#[derive(Default)]
pub struct VnetEarth {
    pub(crate) conn_id_seed: AtomicU64,
    pub(crate) ports: RwLock<HashMap<u64, Socket>>,
    pub(crate) connections: Arc<RwLock<HashMap<ConnId, (NodeId, NodeId)>>>,
}

impl VnetEarth {
    pub fn create_listener(&self, port: u64, node: NodeId, addr: NodeAddr) -> VnetListener {
        let (tx, rx) = unbounded();
        self.ports.write().insert(port, Socket { node, addr, sender: tx });
        VnetListener { rx }
    }

    pub fn create_outgoing(&self, from_port: u64, to_node: NodeId, to_port: u64) -> Option<ConnId> {
        assert_ne!(from_port, to_port);
        let ports = self.ports.read();
        let from_socket = ports.get(&from_port)?;
        let conn_id_out = ConnId::from_out(VNET_PROTOCOL_ID, self.conn_id_seed.fetch_add(1, Ordering::Relaxed));
        let conn_id_in = ConnId::from_in(VNET_PROTOCOL_ID, self.conn_id_seed.fetch_add(1, Ordering::Relaxed));
        if let Some(to_socket) = ports.get(&to_port) {
            if to_socket.node == to_node {
                let (incoming_acceptor, incoming_acceptor_recv) = AsyncConnectionAcceptor::new();
                let (outgoing_acceptor, outgoing_acceptor_recv) = AsyncConnectionAcceptor::new();
                let from_socket_sender = from_socket.sender.clone();
                let from_socket_node = from_socket.node;
                let from_socket_addr = from_socket.addr.clone();
                let to_socket_sender = to_socket.sender.clone();
                let to_socket_node = to_socket.node;
                let to_socket_addr = to_socket.addr.clone();
                let connections = self.connections.clone();
                self.connections.write().insert(conn_id_out, (from_socket_node, to_socket_node));
                async_std::task::spawn(async move {
                    let (from_tx, from_rx) = unbounded();
                    let (to_tx, to_rx) = unbounded();

                    let outgoing_res = outgoing_acceptor_recv.recv().await;
                    let incoming_res = incoming_acceptor_recv.recv().await;
                    let err = match (outgoing_res, incoming_res) {
                        (Ok(Ok(())), Ok(Ok(()))) => None,
                        (Ok(Err(e)), _) => Some(e),
                        (_, Ok(Err(e))) => Some(e),
                        _ => Some(ConnectionRejectReason::Custom("ChannelError".to_string())),
                    };

                    if let Some(err) = err {
                        from_socket_sender
                            .send_blocking(VnetListenerEvent::OutgoingErr(to_node, conn_id_out, OutgoingConnectionError::BehaviorRejected(err)))
                            .expect("Should send OutgoingErr");
                    } else {
                        from_socket_sender
                            .send_blocking(VnetListenerEvent::Outgoing((
                                Arc::new(VnetConnectionSender {
                                    remote_node_id: to_socket_node,
                                    conn_id: conn_id_out,
                                    remote_addr: to_socket_addr.clone(),
                                    sender: from_tx.clone(),
                                    remote_sender: to_tx.clone(),
                                }),
                                Box::new(VnetConnectionReceiver {
                                    remote_node_id: to_socket_node,
                                    conn_id: conn_id_out,
                                    remote_addr: to_socket_addr,
                                    recv: from_rx,
                                    connections: connections.clone(),
                                    first_stats: Some(ConnectionStats {
                                        rtt_ms: 1,
                                        sending_kbps: 0,
                                        send_est_kbps: 100000,
                                        loss_percent: 0,
                                        over_use: false,
                                    }),
                                }),
                            )))
                            .unwrap();
                        to_socket_sender
                            .send_blocking(VnetListenerEvent::Incoming((
                                Arc::new(VnetConnectionSender {
                                    remote_node_id: from_socket_node,
                                    conn_id: conn_id_in,
                                    remote_addr: from_socket_addr.clone(),
                                    sender: to_tx,
                                    remote_sender: from_tx,
                                }),
                                Box::new(VnetConnectionReceiver {
                                    remote_node_id: from_socket_node,
                                    conn_id: conn_id_in,
                                    remote_addr: from_socket_addr,
                                    recv: to_rx,
                                    connections,
                                    first_stats: Some(ConnectionStats {
                                        rtt_ms: 1,
                                        sending_kbps: 0,
                                        send_est_kbps: 100000,
                                        loss_percent: 0,
                                        over_use: false,
                                    }),
                                }),
                            )))
                            .unwrap();
                    }
                });
                from_socket
                    .sender
                    .send_blocking(VnetListenerEvent::OutgoingRequest(to_node, conn_id_out, outgoing_acceptor))
                    .expect("Should send OutgoingRequest");
                to_socket
                    .sender
                    .send_blocking(VnetListenerEvent::IncomingRequest(from_socket_node, conn_id_in, incoming_acceptor))
                    .expect("Should send IncomingRequest");
            } else {
                from_socket
                    .sender
                    .send_blocking(VnetListenerEvent::OutgoingErr(to_node, conn_id_out, OutgoingConnectionError::AuthenticationError))
                    .expect("Should send OutgoingErr::AuthenticationError");
            }
        } else {
            from_socket
                .sender
                .send_blocking(VnetListenerEvent::OutgoingErr(to_node, conn_id_in, OutgoingConnectionError::DestinationNotFound))
                .expect("Should send OutgoingErr::DestinationNotFound");
        }

        Some(conn_id_out)
    }
}
