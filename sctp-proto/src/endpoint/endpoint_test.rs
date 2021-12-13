use super::*;
use crate::association::Event;

use assert_matches::assert_matches;
use lazy_static::lazy_static;
use std::net::Ipv6Addr;
use std::ops::RangeFrom;
use std::sync::Mutex;
use std::{cmp, mem, net::UdpSocket, time::Duration};
use tracing::{info, info_span, trace};

lazy_static! {
    pub static ref SERVER_PORTS: Mutex<RangeFrom<u16>> = Mutex::new(4433..);
    pub static ref CLIENT_PORTS: Mutex<RangeFrom<u16>> = Mutex::new(44433..);
}

fn min_opt<T: Ord>(x: Option<T>, y: Option<T>) -> Option<T> {
    match (x, y) {
        (Some(x), Some(y)) => Some(cmp::min(x, y)),
        (Some(x), _) => Some(x),
        (_, Some(y)) => Some(y),
        _ => None,
    }
}

pub fn client_config() -> ClientConfig {
    ClientConfig::new()
}

pub fn server_config() -> ServerConfig {
    ServerConfig::new()
}

struct TestEndpoint {
    endpoint: Endpoint,
    addr: SocketAddr,
    socket: Option<UdpSocket>,
    timeout: Option<Instant>,
    outbound: VecDeque<Transmit>,
    delayed: VecDeque<Transmit>,
    inbound: VecDeque<(Instant, Option<EcnCodepoint>, Bytes)>,
    accepted: Option<AssociationHandle>,
    connections: HashMap<AssociationHandle, Association>,
    conn_events: HashMap<AssociationHandle, VecDeque<AssociationEvent>>,
}

impl TestEndpoint {
    fn new(endpoint: Endpoint, addr: SocketAddr) -> Self {
        let socket = UdpSocket::bind(addr).expect("failed to bind UDP socket");
        socket
            .set_read_timeout(Some(Duration::new(0, 10_000_000)))
            .unwrap();

        Self {
            endpoint,
            addr,
            socket: Some(socket),
            timeout: None,
            outbound: VecDeque::new(),
            delayed: VecDeque::new(),
            inbound: VecDeque::new(),
            accepted: None,
            connections: HashMap::default(),
            conn_events: HashMap::default(),
        }
    }

    pub fn drive(&mut self, now: Instant, remote: SocketAddr) {
        if let Some(ref socket) = self.socket {
            loop {
                let mut buf = [0; 8192];
                if socket.recv_from(&mut buf).is_err() {
                    break;
                }
            }
        }

        while self.inbound.front().map_or(false, |x| x.0 <= now) {
            let (recv_time, ecn, packet) = self.inbound.pop_front().unwrap();
            if let Some((ch, event)) =
                self.endpoint
                    .handle(recv_time, remote, None, ecn, packet.into())
            {
                match event {
                    DatagramEvent::NewAssociation(conn) => {
                        self.connections.insert(ch, conn);
                        self.accepted = Some(ch);
                    }
                    DatagramEvent::AssociationEvent(event) => {
                        self.conn_events
                            .entry(ch)
                            .or_insert_with(VecDeque::new)
                            .push_back(event);
                    }
                }
            }
        }

        while let Some(x) = self.poll_transmit() {
            self.outbound.push_back(x);
        }

        let mut endpoint_events: Vec<(AssociationHandle, EndpointEvent)> = vec![];
        for (ch, conn) in self.connections.iter_mut() {
            if self.timeout.map_or(false, |x| x <= now) {
                self.timeout = None;
                conn.handle_timeout(now);
            }

            for (_, mut events) in self.conn_events.drain() {
                for event in events.drain(..) {
                    match event.0 {
                        AssociationEventInner::Datagram(transmit) => {
                            conn.handle_transmit(transmit);
                        }
                    }
                }
            }

            while let Some(event) = conn.poll_endpoint_event() {
                endpoint_events.push((*ch, event));
            }

            while let Some(x) = conn.poll_transmit(now) {
                self.outbound.push_back(x);
            }
            self.timeout = conn.poll_timeout();
        }

        for (ch, event) in endpoint_events {
            self.handle_event(ch, event);
        }
    }

    pub fn next_wakeup(&self) -> Option<Instant> {
        let next_inbound = self.inbound.front().map(|x| x.0);
        min_opt(self.timeout, next_inbound)
    }

    fn is_idle(&self) -> bool {
        self.connections.values().all(|x| x.is_idle())
    }

    pub fn delay_outbound(&mut self) {
        assert!(self.delayed.is_empty());
        mem::swap(&mut self.delayed, &mut self.outbound);
    }

    pub fn finish_delay(&mut self) {
        self.outbound.extend(self.delayed.drain(..));
    }

    pub fn assert_accept(&mut self) -> AssociationHandle {
        self.accepted.take().expect("server didn't connect")
    }
}

impl ::std::ops::Deref for TestEndpoint {
    type Target = Endpoint;
    fn deref(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl ::std::ops::DerefMut for TestEndpoint {
    fn deref_mut(&mut self) -> &mut Endpoint {
        &mut self.endpoint
    }
}

struct Pair {
    server: TestEndpoint,
    client: TestEndpoint,
    time: Instant,
    latency: Duration, // One-way
}

impl Pair {
    pub fn new(endpoint_config: Arc<EndpointConfig>, server_config: ServerConfig) -> Self {
        let server = Endpoint::new(endpoint_config.clone(), Some(Arc::new(server_config)));
        let client = Endpoint::new(endpoint_config, None);

        Pair::new_from_endpoint(client, server)
    }

    pub fn new_from_endpoint(client: Endpoint, server: Endpoint) -> Self {
        let server_addr = SocketAddr::new(
            Ipv6Addr::LOCALHOST.into(),
            SERVER_PORTS.lock().unwrap().next().unwrap(),
        );
        let client_addr = SocketAddr::new(
            Ipv6Addr::LOCALHOST.into(),
            CLIENT_PORTS.lock().unwrap().next().unwrap(),
        );
        Self {
            server: TestEndpoint::new(server, server_addr),
            client: TestEndpoint::new(client, client_addr),
            time: Instant::now(),
            latency: Duration::new(0, 0),
        }
    }

    /// Returns whether the connection is not idle
    pub fn step(&mut self) -> bool {
        self.drive_client();
        self.drive_server();
        if self.client.is_idle() && self.server.is_idle() {
            return false;
        }

        let client_t = self.client.next_wakeup();
        let server_t = self.server.next_wakeup();
        match min_opt(client_t, server_t) {
            Some(t) if Some(t) == client_t => {
                if t != self.time {
                    self.time = self.time.max(t);
                    trace!("advancing to {:?} for client", self.time);
                }
                true
            }
            Some(t) if Some(t) == server_t => {
                if t != self.time {
                    self.time = self.time.max(t);
                    trace!("advancing to {:?} for server", self.time);
                }
                true
            }
            Some(_) => unreachable!(),
            None => false,
        }
    }

    /// Advance time until both connections are idle
    pub fn drive(&mut self) {
        while self.step() {}
    }

    pub fn drive_client(&mut self) {
        let span = info_span!("client");
        let _guard = span.enter();
        self.client.drive(self.time, self.server.addr);
        for x in self.client.outbound.drain(..) {
            if let Some(ref socket) = self.client.socket {
                if let Payload::RawEncode(contents) = &x.payload {
                    for content in contents {
                        socket.send_to(content, x.remote).unwrap();
                    }
                }
            }
            if self.server.addr == x.remote {
                if let Payload::RawEncode(contents) = x.payload {
                    for content in contents {
                        self.server
                            .inbound
                            .push_back((self.time + self.latency, x.ecn, content));
                    }
                }
            }
        }
    }

    pub fn drive_server(&mut self) {
        let span = info_span!("server");
        let _guard = span.enter();
        self.server.drive(self.time, self.client.addr);
        for x in self.server.outbound.drain(..) {
            if let Some(ref socket) = self.server.socket {
                if let Payload::RawEncode(contents) = &x.payload {
                    for content in contents {
                        socket.send_to(content, x.remote).unwrap();
                    }
                }
            }
            if self.client.addr == x.remote {
                if let Payload::RawEncode(contents) = x.payload {
                    for content in contents {
                        self.client
                            .inbound
                            .push_back((self.time + self.latency, x.ecn, content));
                    }
                }
            }
        }
    }

    pub fn connect(&mut self) -> (AssociationHandle, AssociationHandle) {
        self.connect_with(client_config())
    }

    pub fn connect_with(&mut self, config: ClientConfig) -> (AssociationHandle, AssociationHandle) {
        info!("connecting");
        let client_ch = self.begin_connect(config);
        self.drive();
        let server_ch = self.server.assert_accept();
        self.finish_connect(client_ch, server_ch);
        (client_ch, server_ch)
    }

    /// Just start connecting the client
    pub fn begin_connect(&mut self, config: ClientConfig) -> AssociationHandle {
        let span = info_span!("client");
        let _guard = span.enter();
        let (client_ch, client_conn) = self
            .client
            .connect(config, self.server.addr) //TODO:, "localhost")
            .unwrap();
        self.client.connections.insert(client_ch, client_conn);
        client_ch
    }

    fn finish_connect(&mut self, client_ch: AssociationHandle, server_ch: AssociationHandle) {
        /*TODO:assert_matches!(
            self.client_conn_mut(client_ch).poll_event(),
            Some(Event::HandshakeDataReady)
        );*/
        assert_matches!(
            self.client_conn_mut(client_ch).poll_event(),
            Some(Event::Connected { .. })
        );
        /*TODO:assert_matches!(
            self.server_conn_mut(server_ch).poll_event(),
            Some(Event::HandshakeDataReady)
        );*/
        assert_matches!(
            self.server_conn_mut(server_ch).poll_event(),
            Some(Event::Connected { .. })
        );
    }

    pub fn client_conn_mut(&mut self, ch: AssociationHandle) -> &mut Association {
        self.client.connections.get_mut(&ch).unwrap()
    }

    /*
    pub fn client_streams(&mut self, ch: AssociationHandle) -> Streams<'_> {
        self.client_conn_mut(ch).streams()
    }

    pub fn client_send(&mut self, ch: AssociationHandle, s: StreamId) -> SendStream<'_> {
        self.client_conn_mut(ch).send_stream(s)
    }

    pub fn client_recv(&mut self, ch: AssociationHandle, s: StreamId) -> RecvStream<'_> {
        self.client_conn_mut(ch).recv_stream(s)
    }

    pub fn client_datagrams(&mut self, ch: AssociationHandle) -> Datagrams<'_> {
        self.client_conn_mut(ch).datagrams()
    }*/

    pub fn server_conn_mut(&mut self, ch: AssociationHandle) -> &mut Association {
        self.server.connections.get_mut(&ch).unwrap()
    }

    /*
    pub fn server_streams(&mut self, ch: AssociationHandle) -> Streams<'_> {
        self.server_conn_mut(ch).streams()
    }

    pub fn server_send(&mut self, ch: AssociationHandle, s: StreamId) -> SendStream<'_> {
        self.server_conn_mut(ch).send_stream(s)
    }

    pub fn server_recv(&mut self, ch: AssociationHandle, s: StreamId) -> RecvStream<'_> {
        self.server_conn_mut(ch).recv_stream(s)
    }

    pub fn server_datagrams(&mut self, ch: AssociationHandle) -> Datagrams<'_> {
        self.server_conn_mut(ch).datagrams()
    }*/
}

impl Default for Pair {
    fn default() -> Self {
        Pair::new(Default::default(), server_config())
    }
}
