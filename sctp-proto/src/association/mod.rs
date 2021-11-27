use crate::config::{ServerConfig, TransportConfig};
use crate::shared::AssociationId;
use crate::util::AssociationIdGenerator;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

mod streams;
mod timer;

pub struct Association {}

impl Association {
    pub(crate) fn new(
        server_config: Option<Arc<ServerConfig>>,
        config: Arc<TransportConfig>,
        init_cid: AssociationId,
        loc_cid: AssociationId,
        rem_cid: AssociationId,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        //crypto: Box<dyn crypto::Session>,
        cid_gen: &dyn AssociationIdGenerator,
        now: Instant,
        //version: u32,
    ) -> Self {
        Association {}
    }
}
