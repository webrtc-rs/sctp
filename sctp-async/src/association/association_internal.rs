impl AssociationInternal {
    pub(crate) async fn close(&mut self) -> Result<()> {
        if self.get_state() != AssociationState::Closed {
            self.set_state(AssociationState::Closed);

            log::debug!("[{}] closing association..", self.name);

            self.close_all_timers().await;

            // awake read/write_loop to exit
            self.close_loop_ch_tx.take();

            for si in self.streams.keys().cloned().collect::<Vec<u16>>() {
                self.unregister_stream(si);
            }

            // Wait for read_loop to end
            //if let Some(read_loop_close_ch) = &mut self.read_loop_close_ch {
            //    let _ = read_loop_close_ch.recv().await;
            //}

            log::debug!("[{}] association closed", self.name);
            log::debug!(
                "[{}] stats nDATAs (in) : {}",
                self.name,
                self.stats.get_num_datas()
            );
            log::debug!(
                "[{}] stats nSACKs (in) : {}",
                self.name,
                self.stats.get_num_sacks()
            );
            log::debug!(
                "[{}] stats nT3Timeouts : {}",
                self.name,
                self.stats.get_num_t3timeouts()
            );
            log::debug!(
                "[{}] stats nAckTimeouts: {}",
                self.name,
                self.stats.get_num_ack_timeouts()
            );
            log::debug!(
                "[{}] stats nFastRetrans: {}",
                self.name,
                self.stats.get_num_fast_retrans()
            );
        }

        Ok(())
    }

    async fn close_all_timers(&mut self) {
        // Close all retransmission & ack timers
        if let Some(t1init) = &self.t1init {
            t1init.stop().await;
        }
        if let Some(t1cookie) = &self.t1cookie {
            t1cookie.stop().await;
        }
        if let Some(t2shutdown) = &self.t2shutdown {
            t2shutdown.stop().await;
        }
        if let Some(t3rtx) = &self.t3rtx {
            t3rtx.stop().await;
        }
        if let Some(treconfig) = &self.treconfig {
            treconfig.stop().await;
        }
        if let Some(ack_timer) = &mut self.ack_timer {
            ack_timer.stop();
        }
    }

    fn awake_write_loop(&self) {
        //log::debug!("[{}] awake_write_loop_ch.notify_one", self.name);
        if let Some(awake_write_loop_ch) = &self.awake_write_loop_ch {
            let _ = awake_write_loop_ch.try_send(());
        }
    }

    /// unregister_stream un-registers a stream from the association
    /// The caller should hold the association write lock.
    fn unregister_stream(&mut self, stream_identifier: u16) {
        let s = self.streams.remove(&stream_identifier);
        if let Some(s) = s {
            s.closed.store(true, Ordering::SeqCst);
            s.read_notifier.notify_waiters();
        }
    }

    fn gather_data_packets_to_retransmit(&mut self, mut raw_packets: Vec<Bytes>) -> Vec<Bytes> {
        for p in &self.get_data_packets_to_retransmit() {
            if let Ok(raw) = p.marshal() {
                raw_packets.push(raw);
            } else {
                log::warn!(
                    "[{}] failed to serialize a DATA packet to be retransmitted",
                    self.name
                );
            }
        }

        raw_packets
    }

    async fn gather_outbound_data_and_reconfig_packets(
        &mut self,
        mut raw_packets: Vec<Bytes>,
    ) -> Vec<Bytes> {
        // Pop unsent data chunks from the pending queue to send as much as
        // cwnd and rwnd allow.
        let (chunks, sis_to_reset) = self.pop_pending_data_chunks_to_send().await;
        if !chunks.is_empty() {
            // Start timer. (noop if already started)
            log::trace!("[{}] T3-rtx timer start (pt1)", self.name);
            if let Some(t3rtx) = &self.t3rtx {
                t3rtx.start(self.rto_mgr.get_rto()).await;
            }
            for p in &self.bundle_data_chunks_into_packets(chunks) {
                if let Ok(raw) = p.marshal() {
                    raw_packets.push(raw);
                } else {
                    log::warn!("[{}] failed to serialize a DATA packet", self.name);
                }
            }
        }

        if !sis_to_reset.is_empty() || self.will_retransmit_reconfig {
            if self.will_retransmit_reconfig {
                self.will_retransmit_reconfig = false;
                log::debug!(
                    "[{}] retransmit {} RECONFIG chunk(s)",
                    self.name,
                    self.reconfigs.len()
                );
                for c in self.reconfigs.values() {
                    let p = self.create_packet(vec![Box::new(c.clone())]);
                    if let Ok(raw) = p.marshal() {
                        raw_packets.push(raw);
                    } else {
                        log::warn!(
                            "[{}] failed to serialize a RECONFIG packet to be retransmitted",
                            self.name,
                        );
                    }
                }
            }

            if !sis_to_reset.is_empty() {
                let rsn = self.generate_next_rsn();
                let tsn = self.my_next_tsn - 1;
                log::debug!(
                    "[{}] sending RECONFIG: rsn={} tsn={} streams={:?}",
                    self.name,
                    rsn,
                    self.my_next_tsn - 1,
                    sis_to_reset
                );

                let c = ChunkReconfig {
                    param_a: Some(Box::new(ParamOutgoingResetRequest {
                        reconfig_request_sequence_number: rsn,
                        sender_last_tsn: tsn,
                        stream_identifiers: sis_to_reset,
                        ..Default::default()
                    })),
                    ..Default::default()
                };
                self.reconfigs.insert(rsn, c.clone()); // store in the map for retransmission

                let p = self.create_packet(vec![Box::new(c)]);
                if let Ok(raw) = p.marshal() {
                    raw_packets.push(raw);
                } else {
                    log::warn!(
                        "[{}] failed to serialize a RECONFIG packet to be transmitted",
                        self.name
                    );
                }
            }

            if !self.reconfigs.is_empty() {
                if let Some(treconfig) = &self.treconfig {
                    treconfig.start(self.rto_mgr.get_rto()).await;
                }
            }
        }

        raw_packets
    }

    fn gather_outbound_fast_retransmission_packets(
        &mut self,
        mut raw_packets: Vec<Bytes>,
    ) -> Vec<Bytes> {
        if self.will_retransmit_fast {
            self.will_retransmit_fast = false;

            let mut to_fast_retrans: Vec<Box<dyn Chunk + Send + Sync>> = vec![];
            let mut fast_retrans_size = COMMON_HEADER_SIZE;

            let mut i = 0;
            loop {
                let tsn = self.cumulative_tsn_ack_point + i + 1;
                if let Some(c) = self.inflight_queue.get_mut(tsn) {
                    if c.acked || c.abandoned() || c.nsent > 1 || c.miss_indicator < 3 {
                        i += 1;
                        continue;
                    }

                    // RFC 4960 Sec 7.2.4 Fast Retransmit on Gap Reports
                    //  3)  Determine how many of the earliest (i.e., lowest TSN) DATA chunks
                    //      marked for retransmission will fit into a single packet, subject
                    //      to constraint of the path MTU of the destination transport
                    //      address to which the packet is being sent.  Call this value K.
                    //      Retransmit those K DATA chunks in a single packet.  When a Fast
                    //      Retransmit is being performed, the sender SHOULD ignore the value
                    //      of cwnd and SHOULD NOT delay retransmission for this single
                    //		packet.

                    let data_chunk_size = DATA_CHUNK_HEADER_SIZE + c.user_data.len() as u32;
                    if self.mtu < fast_retrans_size + data_chunk_size {
                        break;
                    }

                    fast_retrans_size += data_chunk_size;
                    self.stats.inc_fast_retrans();
                    c.nsent += 1;
                } else {
                    break; // end of pending data
                }

                if let Some(c) = self.inflight_queue.get(tsn) {
                    self.check_partial_reliability_status(c);
                    to_fast_retrans.push(Box::new(c.clone()));
                    log::trace!(
                        "[{}] fast-retransmit: tsn={} sent={} htna={}",
                        self.name,
                        c.tsn,
                        c.nsent,
                        self.fast_recover_exit_point
                    );
                }
                i += 1;
            }

            if !to_fast_retrans.is_empty() {
                if let Ok(raw) = self.create_packet(to_fast_retrans).marshal() {
                    raw_packets.push(raw);
                } else {
                    log::warn!(
                        "[{}] failed to serialize a DATA packet to be fast-retransmitted",
                        self.name
                    );
                }
            }
        }

        raw_packets
    }

    async fn gather_outbound_sack_packets(&mut self, mut raw_packets: Vec<Bytes>) -> Vec<Bytes> {
        if self.ack_state == AckState::Immediate {
            self.ack_state = AckState::Idle;
            let sack = self.create_selective_ack_chunk().await;
            log::debug!("[{}] sending SACK: {}", self.name, sack);
            if let Ok(raw) = self.create_packet(vec![Box::new(sack)]).marshal() {
                raw_packets.push(raw);
            } else {
                log::warn!("[{}] failed to serialize a SACK packet", self.name);
            }
        }

        raw_packets
    }

    fn gather_outbound_forward_tsn_packets(&mut self, mut raw_packets: Vec<Bytes>) -> Vec<Bytes> {
        /*log::debug!(
            "[{}] gatherOutboundForwardTSNPackets {}",
            self.name,
            self.will_send_forward_tsn
        );*/
        if self.will_send_forward_tsn {
            self.will_send_forward_tsn = false;
            if sna32gt(
                self.advanced_peer_tsn_ack_point,
                self.cumulative_tsn_ack_point,
            ) {
                let fwd_tsn = self.create_forward_tsn();
                if let Ok(raw) = self.create_packet(vec![Box::new(fwd_tsn)]).marshal() {
                    raw_packets.push(raw);
                } else {
                    log::warn!("[{}] failed to serialize a Forward TSN packet", self.name);
                }
            }
        }

        raw_packets
    }

    async fn gather_outbound_shutdown_packets(
        &mut self,
        mut raw_packets: Vec<Bytes>,
    ) -> (Vec<Bytes>, bool) {
        let mut ok = true;

        if self.will_send_shutdown.load(Ordering::SeqCst) {
            self.will_send_shutdown.store(false, Ordering::SeqCst);

            let shutdown = ChunkShutdown {
                cumulative_tsn_ack: self.cumulative_tsn_ack_point,
            };

            if let Ok(raw) = self.create_packet(vec![Box::new(shutdown)]).marshal() {
                if let Some(t2shutdown) = &self.t2shutdown {
                    t2shutdown.start(self.rto_mgr.get_rto()).await;
                }
                raw_packets.push(raw);
            } else {
                log::warn!("[{}] failed to serialize a Shutdown packet", self.name);
            }
        } else if self.will_send_shutdown_ack {
            self.will_send_shutdown_ack = false;

            let shutdown_ack = ChunkShutdownAck {};

            if let Ok(raw) = self.create_packet(vec![Box::new(shutdown_ack)]).marshal() {
                if let Some(t2shutdown) = &self.t2shutdown {
                    t2shutdown.start(self.rto_mgr.get_rto()).await;
                }
                raw_packets.push(raw);
            } else {
                log::warn!("[{}] failed to serialize a ShutdownAck packet", self.name);
            }
        } else if self.will_send_shutdown_complete {
            self.will_send_shutdown_complete = false;

            let shutdown_complete = ChunkShutdownComplete {};

            if let Ok(raw) = self
                .create_packet(vec![Box::new(shutdown_complete)])
                .marshal()
            {
                raw_packets.push(raw);
                ok = false;
            } else {
                log::warn!(
                    "[{}] failed to serialize a ShutdownComplete packet",
                    self.name
                );
            }
        }

        (raw_packets, ok)
    }

    /// gather_outbound gathers outgoing packets. The returned bool value set to
    /// false means the association should be closed down after the final send.
    pub(crate) async fn gather_outbound(&mut self) -> (Vec<Bytes>, bool) {
        let mut raw_packets = vec![];

        if !self.control_queue.is_empty() {
            for p in self.control_queue.drain(..) {
                if let Ok(raw) = p.marshal() {
                    raw_packets.push(raw);
                } else {
                    log::warn!("[{}] failed to serialize a control packet", self.name);
                    continue;
                }
            }
        }

        let state = self.get_state();
        match state {
            AssociationState::Established => {
                raw_packets = self.gather_data_packets_to_retransmit(raw_packets);
                raw_packets = self
                    .gather_outbound_data_and_reconfig_packets(raw_packets)
                    .await;
                raw_packets = self.gather_outbound_fast_retransmission_packets(raw_packets);
                raw_packets = self.gather_outbound_sack_packets(raw_packets).await;
                raw_packets = self.gather_outbound_forward_tsn_packets(raw_packets);
                (raw_packets, true)
            }
            AssociationState::ShutdownPending
            | AssociationState::ShutdownSent
            | AssociationState::ShutdownReceived => {
                raw_packets = self.gather_data_packets_to_retransmit(raw_packets);
                raw_packets = self.gather_outbound_fast_retransmission_packets(raw_packets);
                raw_packets = self.gather_outbound_sack_packets(raw_packets).await;
                self.gather_outbound_shutdown_packets(raw_packets).await
            }
            AssociationState::ShutdownAckSent => {
                self.gather_outbound_shutdown_packets(raw_packets).await
            }
            _ => (raw_packets, true),
        }
    }

    pub(crate) fn open_stream(
        &mut self,
        stream_identifier: u16,
        default_payload_type: PayloadProtocolIdentifier,
    ) -> Result<Arc<Stream>> {
        if self.streams.contains_key(&stream_identifier) {
            return Err(Error::ErrStreamAlreadyExist);
        }

        if let Some(s) = self.create_stream(stream_identifier, false) {
            s.set_default_payload_type(default_payload_type);
            Ok(Arc::clone(&s))
        } else {
            Err(Error::ErrStreamCreateFailed)
        }
    }

    /// create_forward_tsn generates ForwardTSN chunk.
    /// This method will be be called if use_forward_tsn is set to false.
    fn create_forward_tsn(&self) -> ChunkForwardTsn {
        // RFC 3758 Sec 3.5 C4
        let mut stream_map: HashMap<u16, u16> = HashMap::new(); // to report only once per SI
        let mut i = self.cumulative_tsn_ack_point + 1;
        while sna32lte(i, self.advanced_peer_tsn_ack_point) {
            if let Some(c) = self.inflight_queue.get(i) {
                if let Some(ssn) = stream_map.get(&c.stream_identifier) {
                    if sna16lt(*ssn, c.stream_sequence_number) {
                        // to report only once with greatest SSN
                        stream_map.insert(c.stream_identifier, c.stream_sequence_number);
                    }
                } else {
                    stream_map.insert(c.stream_identifier, c.stream_sequence_number);
                }
            } else {
                break;
            }

            i += 1;
        }

        let mut fwd_tsn = ChunkForwardTsn {
            new_cumulative_tsn: self.advanced_peer_tsn_ack_point,
            streams: vec![],
        };

        let mut stream_str = String::new();
        for (si, ssn) in &stream_map {
            stream_str += format!("(si={} ssn={})", si, ssn).as_str();
            fwd_tsn.streams.push(ChunkForwardTsnStream {
                identifier: *si,
                sequence: *ssn,
            });
        }
        log::trace!(
            "[{}] building fwd_tsn: newCumulativeTSN={} cumTSN={} - {}",
            self.name,
            fwd_tsn.new_cumulative_tsn,
            self.cumulative_tsn_ack_point,
            stream_str
        );

        fwd_tsn
    }

    async fn send_reset_request(&mut self, stream_identifier: u16) -> Result<()> {
        let state = self.get_state();
        if state != AssociationState::Established {
            return Err(Error::ErrResetPacketInStateNotExist);
        }

        // Create DATA chunk which only contains valid stream identifier with
        // nil userData and use it as a EOS from the stream.
        let c = ChunkPayloadData {
            stream_identifier,
            beginning_fragment: true,
            ending_fragment: true,
            user_data: Bytes::new(),
            ..Default::default()
        };

        self.pending_queue.push(c).await;
        self.awake_write_loop();

        Ok(())
    }

    /// Move the chunk peeked with self.pending_queue.peek() to the inflight_queue.
    async fn move_pending_data_chunk_to_inflight_queue(
        &mut self,
        beginning_fragment: bool,
        unordered: bool,
    ) -> Option<ChunkPayloadData> {
        if let Some(mut c) = self.pending_queue.pop(beginning_fragment, unordered).await {
            // Mark all fragements are in-flight now
            if c.ending_fragment {
                c.set_all_inflight();
            }

            // Assign TSN
            c.tsn = self.generate_next_tsn();

            c.since = SystemTime::now(); // use to calculate RTT and also for maxPacketLifeTime
            c.nsent = 1; // being sent for the first time

            self.check_partial_reliability_status(&c);

            log::trace!(
                "[{}] sending ppi={} tsn={} ssn={} sent={} len={} ({},{})",
                self.name,
                c.payload_type as u32,
                c.tsn,
                c.stream_sequence_number,
                c.nsent,
                c.user_data.len(),
                c.beginning_fragment,
                c.ending_fragment
            );

            self.inflight_queue.push_no_check(c.clone());

            Some(c)
        } else {
            log::error!("[{}] failed to pop from pending queue", self.name);
            None
        }
    }

    /// pop_pending_data_chunks_to_send pops chunks from the pending queues as many as
    /// the cwnd and rwnd allows to send.
    async fn pop_pending_data_chunks_to_send(&mut self) -> (Vec<ChunkPayloadData>, Vec<u16>) {
        let mut chunks = vec![];
        let mut sis_to_reset = vec![]; // stream identifiers to reset
        let is_empty = self.pending_queue.len() == 0;
        if !is_empty {
            // RFC 4960 sec 6.1.  Transmission of DATA Chunks
            //   A) At any given time, the data sender MUST NOT transmit new data to
            //      any destination transport address if its peer's rwnd indicates
            //      that the peer has no buffer space (i.e., rwnd is 0; see Section
            //      6.2.1).  However, regardless of the value of rwnd (including if it
            //      is 0), the data sender can always have one DATA chunk in flight to
            //      the receiver if allowed by cwnd (see rule B, below).

            while let Some(c) = self.pending_queue.peek().await {
                let (beginning_fragment, unordered, data_len, stream_identifier) = (
                    c.beginning_fragment,
                    c.unordered,
                    c.user_data.len(),
                    c.stream_identifier,
                );

                if data_len == 0 {
                    sis_to_reset.push(stream_identifier);
                    if self
                        .pending_queue
                        .pop(beginning_fragment, unordered)
                        .await
                        .is_none()
                    {
                        log::error!("failed to pop from pending queue");
                    }
                    continue;
                }

                if self.inflight_queue.get_num_bytes() + data_len > self.cwnd as usize {
                    break; // would exceeds cwnd
                }

                if data_len > self.rwnd as usize {
                    break; // no more rwnd
                }

                self.rwnd -= data_len as u32;

                if let Some(chunk) = self
                    .move_pending_data_chunk_to_inflight_queue(beginning_fragment, unordered)
                    .await
                {
                    chunks.push(chunk);
                }
            }

            // the data sender can always have one DATA chunk in flight to the receiver
            if chunks.is_empty() && self.inflight_queue.is_empty() {
                // Send zero window probe
                if let Some(c) = self.pending_queue.peek().await {
                    let (beginning_fragment, unordered) = (c.beginning_fragment, c.unordered);

                    if let Some(chunk) = self
                        .move_pending_data_chunk_to_inflight_queue(beginning_fragment, unordered)
                        .await
                    {
                        chunks.push(chunk);
                    }
                }
            }
        }

        (chunks, sis_to_reset)
    }

    /// bundle_data_chunks_into_packets packs DATA chunks into packets. It tries to bundle
    /// DATA chunks into a packet so long as the resulting packet size does not exceed
    /// the path MTU.
    fn bundle_data_chunks_into_packets(&self, chunks: Vec<ChunkPayloadData>) -> Vec<Packet> {
        let mut packets = vec![];
        let mut chunks_to_send = vec![];
        let mut bytes_in_packet = COMMON_HEADER_SIZE;

        for c in chunks {
            // RFC 4960 sec 6.1.  Transmission of DATA Chunks
            //   Multiple DATA chunks committed for transmission MAY be bundled in a
            //   single packet.  Furthermore, DATA chunks being retransmitted MAY be
            //   bundled with new DATA chunks, as long as the resulting packet size
            //   does not exceed the path MTU.
            if bytes_in_packet + c.user_data.len() as u32 > self.mtu {
                packets.push(self.create_packet(chunks_to_send));
                chunks_to_send = vec![];
                bytes_in_packet = COMMON_HEADER_SIZE;
            }

            bytes_in_packet += DATA_CHUNK_HEADER_SIZE + c.user_data.len() as u32;
            chunks_to_send.push(Box::new(c));
        }

        if !chunks_to_send.is_empty() {
            packets.push(self.create_packet(chunks_to_send));
        }

        packets
    }

    /// send_payload_data sends the data chunks.
    async fn send_payload_data(&mut self, chunks: Vec<ChunkPayloadData>) -> Result<()> {
        let state = self.get_state();
        if state != AssociationState::Established {
            return Err(Error::ErrPayloadDataStateNotExist);
        }

        // Push the chunks into the pending queue first.
        for c in chunks {
            self.pending_queue.push(c).await;
        }

        self.awake_write_loop();
        Ok(())
    }

    fn check_partial_reliability_status(&self, c: &ChunkPayloadData) {
        if !self.use_forward_tsn {
            return;
        }

        // draft-ietf-rtcweb-data-protocol-09.txt section 6
        //	6.  Procedures
        //		All Data Channel Establishment Protocol messages MUST be sent using
        //		ordered delivery and reliable transmission.
        //
        if c.payload_type == PayloadProtocolIdentifier::Dcep {
            return;
        }

        // PR-SCTP
        if let Some(s) = self.streams.get(&c.stream_identifier) {
            let reliability_type: ReliabilityType =
                s.reliability_type.load(Ordering::SeqCst).into();
            let reliability_value = s.reliability_value.load(Ordering::SeqCst);

            if reliability_type == ReliabilityType::Rexmit {
                if c.nsent >= reliability_value {
                    c.set_abandoned(true);
                    log::trace!(
                        "[{}] marked as abandoned: tsn={} ppi={} (remix: {})",
                        self.name,
                        c.tsn,
                        c.payload_type,
                        c.nsent
                    );
                }
            } else if reliability_type == ReliabilityType::Timed {
                if let Ok(elapsed) = SystemTime::now().duration_since(c.since) {
                    if elapsed.as_millis() as u32 >= reliability_value {
                        c.set_abandoned(true);
                        log::trace!(
                            "[{}] marked as abandoned: tsn={} ppi={} (timed: {:?})",
                            self.name,
                            c.tsn,
                            c.payload_type,
                            elapsed
                        );
                    }
                }
            }
        } else {
            log::error!("[{}] stream {} not found)", self.name, c.stream_identifier);
        }
    }

    /// get_data_packets_to_retransmit is called when T3-rtx is timed out and retransmit outstanding data chunks
    /// that are not acked or abandoned yet.
    fn get_data_packets_to_retransmit(&mut self) -> Vec<Packet> {
        let awnd = std::cmp::min(self.cwnd, self.rwnd);
        let mut chunks = vec![];
        let mut bytes_to_send = 0;
        let mut done = false;
        let mut i = 0;
        while !done {
            let tsn = self.cumulative_tsn_ack_point + i + 1;
            if let Some(c) = self.inflight_queue.get_mut(tsn) {
                if !c.retransmit {
                    i += 1;
                    continue;
                }

                if i == 0 && self.rwnd < c.user_data.len() as u32 {
                    // Send it as a zero window probe
                    done = true;
                } else if bytes_to_send + c.user_data.len() > awnd as usize {
                    break;
                }

                // reset the retransmit flag not to retransmit again before the next
                // t3-rtx timer fires
                c.retransmit = false;
                bytes_to_send += c.user_data.len();

                c.nsent += 1;
            } else {
                break; // end of pending data
            }

            if let Some(c) = self.inflight_queue.get(tsn) {
                self.check_partial_reliability_status(c);

                log::trace!(
                    "[{}] retransmitting tsn={} ssn={} sent={}",
                    self.name,
                    c.tsn,
                    c.stream_sequence_number,
                    c.nsent
                );

                chunks.push(c.clone());
            }
            i += 1;
        }

        self.bundle_data_chunks_into_packets(chunks)
    }

    /// generate_next_tsn returns the my_next_tsn and increases it. The caller should hold the lock.
    fn generate_next_tsn(&mut self) -> u32 {
        let tsn = self.my_next_tsn;
        self.my_next_tsn += 1;
        tsn
    }

    /// generate_next_rsn returns the my_next_rsn and increases it. The caller should hold the lock.
    fn generate_next_rsn(&mut self) -> u32 {
        let rsn = self.my_next_rsn;
        self.my_next_rsn += 1;
        rsn
    }

    async fn create_selective_ack_chunk(&mut self) -> ChunkSelectiveAck {
        ChunkSelectiveAck {
            cumulative_tsn_ack: self.peer_last_tsn,
            advertised_receiver_window_credit: self.get_my_receiver_window_credit().await,
            gap_ack_blocks: self.payload_queue.get_gap_ack_blocks(self.peer_last_tsn),
            duplicate_tsn: self.payload_queue.pop_duplicates(),
        }
    }

    fn pack(p: Packet) -> Vec<Packet> {
        vec![p]
    }

    /// buffered_amount returns total amount (in bytes) of currently buffered user data.
    /// This is used only by testing.
    pub(crate) fn buffered_amount(&self) -> usize {
        self.pending_queue.get_num_bytes() + self.inflight_queue.get_num_bytes()
    }
}

#[async_trait]
impl AckTimerObserver for AssociationInternal {
    async fn on_ack_timeout(&mut self) {
        log::trace!(
            "[{}] ack timed out (ack_state: {})",
            self.name,
            self.ack_state
        );
        self.stats.inc_ack_timeouts();
        self.ack_state = AckState::Immediate;
        self.awake_write_loop();
    }
}

#[async_trait]
impl RtxTimerObserver for AssociationInternal {
    async fn on_retransmission_timeout(&mut self, id: RtxTimerId, n_rtos: usize) {
        match id {
            RtxTimerId::T1Init => {
                if let Err(err) = self.send_init() {
                    log::debug!(
                        "[{}] failed to retransmit init (n_rtos={}): {:?}",
                        self.name,
                        n_rtos,
                        err
                    );
                }
            }

            RtxTimerId::T1Cookie => {
                if let Err(err) = self.send_cookie_echo() {
                    log::debug!(
                        "[{}] failed to retransmit cookie-echo (n_rtos={}): {:?}",
                        self.name,
                        n_rtos,
                        err
                    );
                }
            }

            RtxTimerId::T2Shutdown => {
                log::debug!(
                    "[{}] retransmission of shutdown timeout (n_rtos={})",
                    self.name,
                    n_rtos
                );
                let state = self.get_state();
                match state {
                    AssociationState::ShutdownSent => {
                        self.will_send_shutdown.store(true, Ordering::SeqCst);
                        self.awake_write_loop();
                    }
                    AssociationState::ShutdownAckSent => {
                        self.will_send_shutdown_ack = true;
                        self.awake_write_loop();
                    }
                    _ => {}
                }
            }

            RtxTimerId::T3RTX => {
                self.stats.inc_t3timeouts();

                // RFC 4960 sec 6.3.3
                //  E1)  For the destination address for which the timer expires, adjust
                //       its ssthresh with rules defined in Section 7.2.3 and set the
                //       cwnd <- MTU.
                // RFC 4960 sec 7.2.3
                //   When the T3-rtx timer expires on an address, SCTP should perform slow
                //   start by:
                //      ssthresh = max(cwnd/2, 4*MTU)
                //      cwnd = 1*MTU

                self.ssthresh = std::cmp::max(self.cwnd / 2, 4 * self.mtu);
                self.cwnd = self.mtu;
                log::trace!(
                    "[{}] updated cwnd={} ssthresh={} inflight={} (RTO)",
                    self.name,
                    self.cwnd,
                    self.ssthresh,
                    self.inflight_queue.get_num_bytes()
                );

                // RFC 3758 sec 3.5
                //  A5) Any time the T3-rtx timer expires, on any destination, the sender
                //  SHOULD try to advance the "Advanced.Peer.Ack.Point" by following
                //  the procedures outlined in C2 - C5.
                if self.use_forward_tsn {
                    // RFC 3758 Sec 3.5 C2
                    let mut i = self.advanced_peer_tsn_ack_point + 1;
                    while let Some(c) = self.inflight_queue.get(i) {
                        if !c.abandoned() {
                            break;
                        }
                        self.advanced_peer_tsn_ack_point = i;
                        i += 1;
                    }

                    // RFC 3758 Sec 3.5 C3
                    if sna32gt(
                        self.advanced_peer_tsn_ack_point,
                        self.cumulative_tsn_ack_point,
                    ) {
                        self.will_send_forward_tsn = true;
                        log::debug!(
                            "[{}] on_retransmission_timeout {}: sna32GT({}, {})",
                            self.name,
                            self.will_send_forward_tsn,
                            self.advanced_peer_tsn_ack_point,
                            self.cumulative_tsn_ack_point
                        );
                    }
                }

                log::debug!(
                    "[{}] T3-rtx timed out: n_rtos={} cwnd={} ssthresh={}",
                    self.name,
                    n_rtos,
                    self.cwnd,
                    self.ssthresh
                );

                self.inflight_queue.mark_all_to_retrasmit();
                self.awake_write_loop();
            }

            RtxTimerId::Reconfig => {
                self.will_retransmit_reconfig = true;
                self.awake_write_loop();
            }
        }
    }

    async fn on_retransmission_failure(&mut self, id: RtxTimerId) {
        match id {
            RtxTimerId::T1Init => {
                log::error!("[{}] retransmission failure: T1-init", self.name);
                if let Some(handshake_completed_ch) = &self.handshake_completed_ch_tx {
                    let _ = handshake_completed_ch
                        .send(Some(Error::ErrHandshakeInitAck))
                        .await;
                }
            }
            RtxTimerId::T1Cookie => {
                log::error!("[{}] retransmission failure: T1-cookie", self.name);
                if let Some(handshake_completed_ch) = &self.handshake_completed_ch_tx {
                    let _ = handshake_completed_ch
                        .send(Some(Error::ErrHandshakeCookieEcho))
                        .await;
                }
            }

            RtxTimerId::T2Shutdown => {
                log::error!("[{}] retransmission failure: T2-shutdown", self.name);
            }

            RtxTimerId::T3RTX => {
                // T3-rtx timer will not fail by design
                // Justifications:
                //  * ICE would fail if the connectivity is lost
                //  * WebRTC spec is not clear how this incident should be reported to ULP
                log::error!("[{}] retransmission failure: T3-rtx (DATA)", self.name);
            }
            _ => {}
        }
    }
}
