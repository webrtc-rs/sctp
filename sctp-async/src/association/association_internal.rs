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
