use bluesea_identity::{PeerAddr, PeerId};
use network::transport::ConnectionMsg;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub enum TcpMsg<MSG> {
    ConnectRequest(PeerId, PeerId, PeerAddr),
    ConnectResponse(Result<(PeerId, PeerAddr), String>),
    Ping(u64),
    Pong(u64),
    Msg(u8, ConnectionMsg<MSG>),
}