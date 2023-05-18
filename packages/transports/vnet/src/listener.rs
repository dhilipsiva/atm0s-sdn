use crate::connection::VnetConnection;
use async_std::channel::Receiver;
use bluesea_identity::PeerId;
use network::transport::OutgoingConnectionError;

pub enum VnetListenerEvent<MSG> {
    Incoming(VnetConnection<MSG>),
    Outgoing(VnetConnection<MSG>),
    OutgoingErr(u32, PeerId, OutgoingConnectionError),
}

pub struct VnetListener<MSG> {
    pub(crate) rx: Receiver<VnetListenerEvent<MSG>>,
}

impl<MSG> VnetListener<MSG> {
    pub async fn recv(&mut self) -> Option<VnetListenerEvent<MSG>> {
        self.rx.recv().await.ok()
    }
}
