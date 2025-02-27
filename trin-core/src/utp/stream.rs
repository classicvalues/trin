#![allow(dead_code)]

use crate::portalnet::discovery::Discovery;
use core::convert::TryFrom;
use discv5::enr::NodeId;
use discv5::Enr;
use log::debug;
use rand::Rng;
use std::cmp::{max, min};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

use crate::portalnet::types::messages::ProtocolId;
use crate::utp::packets::{Packet, PacketHeader, PacketType, HEADER_SIZE};
use crate::utp::trin_helpers::{UtpMessageId, UtpStreamState};

// Maximum time (in microseconds) to wait for incoming packets when the send window is full
const PRE_SEND_TIMEOUT: u32 = 500_000;

pub const MAX_DISCV5_PACKET_SIZE: usize = 1280;
pub const MIN_PACKET_SIZE: usize = 150;
const MIN_DISCV5_PACKET_SIZE: usize = 63;
// 100 miliseconds
const CCONTROL_TARGET: usize = 100 * 1000;
const MAX_CWND_INCREASE_BYTES_PER_RTT: usize = 3000;
const MIN_WINDOW_SIZE: usize = 10;

/// Return current time in microseconds since the UNIX epoch.
fn now_microseconds() -> u32 {
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros()
        & 0xffffffff) as u32
}

fn get_time_diff(response_time: u32) -> u32 {
    let current_time = now_microseconds();

    if current_time > response_time {
        current_time - response_time
    } else {
        response_time - current_time
    }
}

pub fn rand() -> u16 {
    rand::thread_rng().gen()
}

#[derive(PartialEq, Clone, Debug)]
pub enum SocketState {
    Uninitialized,
    SynSent,
    SynRecv,
    Connected,
    Disconnected,
}

#[derive(Hash, Eq, PartialEq, Copy, Clone, Debug)]
pub struct ConnectionKey {
    pub node_id: NodeId,
    pub conn_id_recv: u16,
}

impl ConnectionKey {
    fn new(node_id: NodeId, conn_id_recv: u16) -> Self {
        Self {
            node_id,
            conn_id_recv,
        }
    }
}

// Basically the same idea as in the official Bit Torrent library we will store all of the active connections data here
pub struct UtpListener {
    pub discovery: Arc<Discovery>,
    pub utp_connections: HashMap<ConnectionKey, UtpSocket>,
    // We only want to listen/handle packets of connections that were negotiated with
    pub listening: HashMap<u16, UtpMessageId>,
}

impl UtpListener {
    pub fn process_utp_request(&mut self, payload: &[u8], node_id: &NodeId) {
        match Packet::try_from(payload) {
            Ok(packet) => {
                let connection_id = packet.connection_id();

                match packet.get_type() {
                    PacketType::Reset => {
                        let key_fn =
                            |offset| ConnectionKey::new(*node_id, connection_id - 1 + offset);
                        let f = |conn: &&mut UtpSocket| -> bool {
                            conn.sender_connection_id == connection_id
                        };

                        if let Some(conn) = self.utp_connections.get_mut(&key_fn(1)) {
                            conn.state = SocketState::Disconnected;
                        } else if let Some(conn) =
                            self.utp_connections.get_mut(&key_fn(2)).filter(f)
                        {
                            conn.state = SocketState::Disconnected;
                        } else if let Some(conn) =
                            self.utp_connections.get_mut(&key_fn(0)).filter(f)
                        {
                            conn.state = SocketState::Disconnected;
                        }
                    }
                    PacketType::Syn => {
                        if let Some(enr) = self.discovery.discv5.find_enr(node_id) {
                            // If neither of those cases happened handle this is a new request
                            let (tx, _) = mpsc::unbounded_channel::<UtpStreamState>();
                            let mut conn = UtpSocket::new(Arc::clone(&self.discovery), enr, tx);
                            conn.handle_packet(packet);
                            self.utp_connections.insert(
                                ConnectionKey {
                                    node_id: *node_id,
                                    conn_id_recv: conn.receiver_connection_id,
                                },
                                conn,
                            );
                        } else {
                            debug!("Query requested an unknown ENR");
                        }
                    }
                    _ => {
                        if let Some(conn) = self.utp_connections.get_mut(&ConnectionKey {
                            node_id: *node_id,
                            conn_id_recv: connection_id,
                        }) {
                            conn.handle_packet(packet);
                        }
                    }
                }
            }
            Err(e) => {
                debug!("Failed to decode packet: {}", e);
            }
        }
    }

    // I am honestly not sure if I should init this with Enr or NodeId since we could use both
    pub fn connect(
        &mut self,
        connection_id: u16,
        node_id: NodeId,
        tx: mpsc::UnboundedSender<UtpStreamState>,
    ) {
        if let Some(enr) = self.discovery.discv5.find_enr(&node_id) {
            let mut conn = UtpSocket::new(Arc::clone(&self.discovery), enr, tx);
            conn.make_connection(connection_id);
            self.utp_connections.insert(
                ConnectionKey {
                    node_id,
                    conn_id_recv: connection_id,
                },
                conn,
            );
        }
    }
}

// Used to be MicroTransportProtocol impl but it is basically just called UtpStream compared to the
// Rust Tcp Lib so I changed it
pub struct UtpSocket {
    /// The wrapped discv5 protocol
    socket: Arc<Discovery>,

    /// Socket state
    pub state: SocketState,

    // Remote peer
    connected_to: Enr,

    /// Sequence number for the next packet
    seq_nr: u16,

    /// Sequence number of the latest acknowledged packet sent by the remote peer
    ack_nr: u16,

    /// Sender connection identifier
    sender_connection_id: u16,

    /// Receiver connection identifier
    pub receiver_connection_id: u16,

    // maximum window size, in bytes
    max_window: u32,

    /// Received but not acknowledged packets
    incoming_buffer: BTreeMap<u16, Packet>,

    /// Packets not yet sent
    unsent_queue: VecDeque<Packet>,

    /// Bytes in flight
    cur_window: u32,

    /// Window size of the remote peer
    remote_wnd_size: u32,

    /// Sent but not yet acknowledged packets
    send_window: HashMap<u16, Packet>,

    /// How many ACKs did the socket receive for packet with sequence number equal to `ack_nr`
    duplicate_ack_count: u8,

    /// Sequence number of the latest packet the remote peer acknowledged
    last_acked: u16,

    /// Round-trip time to remote peer
    rtt: i32,

    /// Variance of the round-trip time to the remote peer
    rtt_variance: i32,

    /// Rolling window of packet delay to remote peer
    base_delays: Vec<u32>,

    /// Rolling window of the difference between sending a packet and receiving its acknowledgement
    current_delays: Vec<u32>,

    /// Current congestion timeout in milliseconds
    congestion_timeout: u64,

    /// Start of the current minute for sampling purposes
    // TODO: Add Timestamp struct here
    last_rollover: u32,

    pub recv_data_stream: Vec<u8>,
    tx: mpsc::UnboundedSender<UtpStreamState>,
}

impl UtpSocket {
    fn new(
        socket: Arc<Discovery>,
        connected_to: Enr,
        tx: mpsc::UnboundedSender<UtpStreamState>,
    ) -> Self {
        Self {
            state: SocketState::Uninitialized,
            seq_nr: 0,
            ack_nr: 0,
            receiver_connection_id: 0,
            sender_connection_id: 0,
            max_window: 0,
            incoming_buffer: Default::default(),
            unsent_queue: Default::default(),
            connected_to,
            socket,
            cur_window: 0,
            remote_wnd_size: 0,
            send_window: Default::default(),
            duplicate_ack_count: 0,
            last_acked: 0,
            rtt: 0,
            rtt_variance: 0,
            base_delays: vec![],
            congestion_timeout: 1000,
            last_rollover: 0,
            current_delays: Vec::with_capacity(8),
            recv_data_stream: vec![],

            // signal when node is connected to write payload
            tx,
        }
    }

    /// Sends data on the socket to the remote peer. On success, returns the number of bytes
    /// written.
    //
    // # Implementation details
    //
    // This method inserts packets into the send buffer and keeps trying to
    // advance the send window until an ACK corresponding to the last packet is
    // received.
    //
    // Note that the buffer passed to `send_to` might exceed the maximum packet
    // size, which will result in the data being split over several packets.
    pub fn send_to(&mut self, buf: &[u8]) -> usize {
        //TODO: CHeck if we need this
        //
        // if self.state == SocketState::Closed {
        //     return Err(SocketError::ConnectionClosed.into());
        // }

        let total_length = buf.len();

        for chunk in buf.chunks(MAX_DISCV5_PACKET_SIZE - MIN_DISCV5_PACKET_SIZE - HEADER_SIZE) {
            let mut header = PacketHeader::new();
            header.set_type(PacketType::Data);
            header.set_connection_id(self.sender_connection_id);
            header.set_timestamp(now_microseconds());
            header.set_seq_nr(self.seq_nr);
            header.set_ack_nr(self.ack_nr);

            let packet = Packet::with_payload(header, chunk);
            self.unsent_queue.push_back(packet);

            // Intentionally wrap around sequence number
            self.seq_nr = self.seq_nr.wrapping_add(1);
        }

        // Send every packet in the queue
        self.send_packets_in_queue();

        total_length
    }

    fn send_packets_in_queue(&mut self) {
        while let Some(packet) = self.unsent_queue.pop_front() {
            self.send_packet(packet.clone());
            self.cur_window += packet.as_ref().len() as u32;
            self.send_window.insert(packet.get_header().seq_nr, packet);
        }
    }

    fn resend_packet(&self, seq_nr: u16) {
        if let Some(packet) = self.send_window.get(&seq_nr) {
            self.send_packet(packet.clone());
        }
    }

    /// Send one packet.
    fn send_packet(&self, packet: Packet) {
        debug!("current window: {}", self.send_window.len());
        let max_inflight = max(
            MAX_DISCV5_PACKET_SIZE,
            min(self.max_window as usize, self.remote_wnd_size as usize),
        );
        let now = now_microseconds();

        // Wait until enough in-flight packets are acknowledged for rate control purposes, but don't
        // wait more than 500 ms (PRE_SEND_TIMEOUT) before sending the packet
        while self.cur_window + packet.as_ref().len() as u32 > max_inflight as u32
            && now_microseconds() - now < PRE_SEND_TIMEOUT
        {
            debug!("self.curr_window: {}", self.cur_window);
            debug!("max_inflight: {}", max_inflight);
            debug!("self.duplicate_ack_count: {}", self.duplicate_ack_count);
            debug!("now_microseconds() - now = {}", now_microseconds() - now)
            // TODO: Why here we dont do for example:
            // let mut buf = [0; BUF_SIZE];
            // self.recv(&mut buf)?;
        }

        debug!(
            "out: now_microseconds() - now = {}",
            now_microseconds() - now
        );

        let enr = self.connected_to.clone();
        let discovery = self.socket.clone();
        let packet_to_send = packet.clone();

        // Handle talkreq/talkresp in the background
        tokio::spawn(async move {
            if let Err(response) = discovery
                .send_talk_req(enr, ProtocolId::Utp, Vec::from(packet_to_send.as_ref()))
                .await
            {
                debug!("Unable to send utp talk req: {response}")
            }
        });
        debug!("sent {:?}", packet);
    }

    fn update_base_delay(&mut self, delay: u32, now: u32) {
        if now - self.last_rollover > 60 * 1000 {
            self.last_rollover = now;
            if self.base_delays.len() == 10 {
                self.base_delays.remove(0);
            }
            self.base_delays.push(delay);
        } else {
            let index = self.base_delays.len() - 1;
            self.base_delays[index] = min(self.base_delays[index], delay);
        }
    }

    fn update_current_delay(&mut self, delay: u32) {
        if !self.current_delays.len() < 8 {
            self.current_delays = self.current_delays[1..].to_owned();
        }
        self.current_delays.push(delay);
    }

    fn filter(current_delay: &[u32]) -> u32 {
        let filt = (current_delay.len() as f64 / 3_f64).ceil() as usize;
        *current_delay[current_delay.len() - filt..]
            .iter()
            .min()
            .unwrap()
    }

    fn make_connection(&mut self, connection_id: u16) {
        if self.state == SocketState::Uninitialized {
            self.state = SocketState::SynSent;
            self.seq_nr = 1;
            self.receiver_connection_id = connection_id;
            self.sender_connection_id = self.receiver_connection_id + 1;

            let mut header = PacketHeader::new();
            header.set_type(PacketType::Syn);
            header.set_connection_id(self.receiver_connection_id);
            header.set_timestamp(now_microseconds());
            header.set_timestamp_difference(0);
            header.set_seq_nr(self.seq_nr + 1);
            header.set_ack_nr(0);

            self.send_packet(Packet::new(header));
        }
    }

    fn build_selective_ack(&self) -> Vec<u8> {
        // must be at least 4 bytes, and in multiples of 4
        let incoming = self.incoming_buffer.range((self.ack_nr + 2)..);
        let len = incoming.clone().count();
        let k = if len % 32 != 0 {
            (len / 32) + 1
        } else {
            len / 32
        };

        let mut sack_bitfield: Vec<u8> = vec![0u8; k * 4];

        for (seq, _) in incoming {
            let v = (seq - self.ack_nr - 2) as usize;
            let (index, offset) = (v / 8, v % 8);
            sack_bitfield[index] |= 1 << offset;
        }
        sack_bitfield
    }

    pub fn send_finalize(&self) {
        let mut header = PacketHeader::new();
        header.set_type(PacketType::Fin);
        header.set_connection_id(self.sender_connection_id);
        header.set_timestamp(now_microseconds());
        header.set_timestamp_difference(0);
        header.set_seq_nr(self.seq_nr + 1);
        header.set_ack_nr(0);

        self.send_packet(Packet::new(header));
    }

    fn handle_packet(&mut self, packet: Packet) {
        debug!(
            "Handle packet: {:?}. Conn state: {:?}",
            packet.get_type(),
            self.state
        );
        self.remote_wnd_size = packet.wnd_size();

        // Only acknowledge this if this follows the last one, else do it when we advance the send
        // window
        if packet.seq_nr().wrapping_sub(self.ack_nr) == 1 {
            self.ack_nr = packet.seq_nr();
        }

        match packet.get_type() {
            PacketType::Data => self.handle_data_packet(packet),
            PacketType::Fin => self.handle_finalize_packet(packet),
            PacketType::State => self.handle_state_packet(packet),
            PacketType::Reset => unreachable!("Reset should never make it here"),
            PacketType::Syn => self.handle_syn_packet(packet),
        }
    }

    fn handle_data_packet(&mut self, packet: Packet) {
        if self.state == SocketState::SynRecv {
            self.state = SocketState::Connected;
        }

        let mut header = PacketHeader::new();
        header.set_type(PacketType::State);
        header.set_connection_id(self.sender_connection_id);
        header.set_timestamp(now_microseconds());
        header.set_timestamp_difference(get_time_diff(packet.timestamp()));
        header.set_seq_nr(self.seq_nr);
        header.set_ack_nr(self.ack_nr);

        let mut reply = Packet::new(header);
        if packet.seq_nr().wrapping_sub(self.ack_nr) > 1 {
            let sack_bitfield = self.build_selective_ack();
            reply.set_selective_ack(sack_bitfield);
        }

        self.send_packet(reply);

        // Add packet to BTreeMap
        self.incoming_buffer.insert(packet.seq_nr(), packet);

        // TODO: use pop_front when it is in a stable release
        if let Some(packet_from_buffer) = self.incoming_buffer.clone().values().next() {
            let packet_seq = packet_from_buffer.seq_nr();
            if !self.incoming_buffer.is_empty()
                && (self.ack_nr == packet_seq || self.ack_nr + 1 == packet_seq)
            {
                self.incoming_buffer.remove(&packet_seq);
                self.ack_nr = packet_seq;

                self.recv_data_stream
                    .append(&mut Vec::from(packet_from_buffer.get_payload()));
            }
        }
    }

    fn handle_state_packet(&mut self, packet: Packet) {
        if self.state == SocketState::SynSent {
            self.state = SocketState::Connected;
            self.ack_nr = packet.seq_nr() - 1;

            self.tx.send(UtpStreamState::Connected).unwrap();
        } else {
            if self.last_acked == packet.ack_nr() {
                self.duplicate_ack_count += 1;
            } else {
                self.last_acked = packet.ack_nr();
                self.duplicate_ack_count = 1;
            }
            debug!(
                "Send window first {:?}, Packet ack_nr: {}",
                self.send_window,
                packet.ack_nr()
            );

            // handle timeouts and congestion window
            if let Some(stored_packet) = self.send_window.get(&packet.ack_nr()) {
                let now = now_microseconds();
                let our_delay = now - stored_packet.timestamp();
                self.update_base_delay(our_delay, now);
                self.update_current_delay(our_delay);
                let queuing_delay = UtpSocket::filter(&self.current_delays)
                    - self.base_delays.iter().min().unwrap();

                self.update_congestion_window(&packet, queuing_delay);
                self.update_timeout(our_delay, queuing_delay);
            }

            let mut packet_loss_detected = false;
            let mut already_resent_ack_1 = false;
            for extension in &packet.get_extensions() {
                // The only extension we support is 1 selective acks, some clients support
                // others tho.
                if extension.extension_type == 1 {
                    self.resend_packet(packet.ack_nr() + 1);
                    already_resent_ack_1 = true;
                    packet_loss_detected = true;
                }

                if let Some((last_seq_nr, _)) = self.send_window.iter().last() {
                    // I need to iterate over this starting with least sig byte per byte
                    // then move right after
                    let mut k = 0;
                    for byte in &extension.bitmask {
                        for i in 0..8 {
                            let (bit, seq_nr) = ((byte >> i) & 1, packet.ack_nr() + 2 + k);
                            if bit == 0 && seq_nr < *last_seq_nr {
                                self.resend_packet(seq_nr);
                                packet_loss_detected = true;
                            }
                            k += 1;
                        }
                    }
                }
            }

            if self.duplicate_ack_count == 3 && !already_resent_ack_1 {
                self.resend_packet(packet.ack_nr() + 1);
                packet_loss_detected = true;
            }

            if packet_loss_detected {
                self.max_window /= 2;
            }

            // acknowledge received packet
            if let Some(stored_packet) = self.send_window.get(&packet.ack_nr()) {
                let seq_nr = stored_packet.seq_nr();
                let len = stored_packet.as_ref().len() as u32;
                self.send_window.remove(&seq_nr);

                // Since we want to close the stream when they have recv all of the packets
                // use a channel
                debug!("Send_window: {:?}", self.send_window);
                if self.send_window.is_empty() {
                    self.tx.send(UtpStreamState::Finished).unwrap();
                }
                self.cur_window -= len;
            }
        }
    }

    fn update_congestion_window(&mut self, packet: &Packet, queuing_delay: u32) {
        // LEDBAT congestion control
        // todo: I will need to add support for bytes_acked to also consider selective
        // acks when they are implemented
        let mut bytes_acked = 0;
        for (i, stored_packet) in self.send_window.iter() {
            if &packet.ack_nr() <= i {
                bytes_acked += stored_packet.as_ref().len();
            }
        }

        let off_target = (CCONTROL_TARGET as f64 - queuing_delay as f64) / CCONTROL_TARGET as f64;
        let window_factor = (min(bytes_acked, self.max_window as usize)
            / max(self.max_window as usize, bytes_acked)) as f64;
        let scaled_gain =
            (MAX_CWND_INCREASE_BYTES_PER_RTT as f64 * off_target * window_factor) as u32;

        self.max_window += scaled_gain;
    }

    fn update_timeout(&mut self, our_delay: u32, queuing_delay: u32) {
        let packet_rtt = (our_delay - queuing_delay) as i32;
        let delta = self.rtt - packet_rtt;
        self.rtt_variance += (delta - self.rtt_variance) / 4;
        self.rtt += (packet_rtt - self.rtt) / 8;
        self.congestion_timeout = max((self.rtt + self.rtt_variance * 4) as u64, 500);
    }

    fn handle_finalize_packet(&mut self, packet: Packet) {
        if self.state == SocketState::Connected {
            self.state = SocketState::Disconnected;

            let mut header = PacketHeader::new();
            header.set_type(PacketType::State);
            header.set_connection_id(self.sender_connection_id);
            header.set_timestamp(now_microseconds());
            header.set_timestamp_difference(get_time_diff(packet.timestamp()));
            header.set_seq_nr(self.seq_nr);
            header.set_ack_nr(self.ack_nr);

            self.send_packet(Packet::new(header));
        }
    }

    fn handle_syn_packet(&mut self, packet: Packet) {
        self.receiver_connection_id = packet.connection_id() + 1;
        self.sender_connection_id = packet.connection_id();
        self.seq_nr = rand();
        self.ack_nr = packet.seq_nr();
        self.state = SocketState::SynRecv;

        let mut header = PacketHeader::new();
        header.set_type(PacketType::State);
        header.set_connection_id(self.sender_connection_id);
        header.set_timestamp(now_microseconds());
        header.set_timestamp_difference(get_time_diff(packet.timestamp()));
        header.set_seq_nr(self.seq_nr);
        header.set_ack_nr(self.ack_nr);

        self.send_packet(Packet::new(header));
    }
}

#[cfg(test)]
mod tests {
    use crate::utp::packets::VERSION;
    use crate::utp::stream::{Packet, PacketHeader, PacketType};
    use std::convert::TryFrom;

    #[test]
    fn test_decode_packet() {
        let buf = [
            0x21, 0x0, 0xA4, 0x46, 0xA7, 0x3E, 0xF4, 0x40, 0x0, 0x0, 0x27, 0x10, 0x0, 0x0, 0xF0,
            0x0, 0x3C, 0x2C, 0x7E, 0xB5,
        ];

        let packet = Packet::try_from(&buf[..]);
        assert!(packet.is_ok());
        let packet = packet.unwrap();
        assert_eq!(packet.get_type(), PacketType::State);
        assert_eq!(packet.get_version(), VERSION);
        assert_eq!(packet.extension(), 0);
        assert_eq!(packet.connection_id(), 42054);
        assert_eq!(packet.timestamp(), 2805920832);
        assert_eq!(packet.timestamp_difference(), 10000);
        assert_eq!(packet.wnd_size(), 61440);
        assert_eq!(packet.seq_nr(), 15404);
        assert_eq!(packet.ack_nr(), 32437);
        assert!(packet.get_payload().is_empty());
    }

    #[test]
    fn test_decode_packet_with_extension() {
        let buf = [
            0x21, 0x1, 0xA4, 0x46, 0xA7, 0x3E, 0xF4, 0x40, 0x0, 0x0, 0x27, 0x10, 0x0, 0x0, 0xF0,
            0x0, 0x3C, 0x2C, 0x7E, 0xB5, 0x0, 0x4, 0xAA, 0x3C, 0x5F, 0x0,
        ];

        let packet = Packet::try_from(&buf[..]);
        assert!(packet.is_ok());
        let packet = packet.unwrap();
        assert_eq!(packet.get_type(), PacketType::State);
        assert_eq!(packet.get_version(), VERSION);
        assert_eq!(packet.extension(), 1);
        assert_eq!(packet.connection_id(), 42054);
        assert_eq!(packet.timestamp(), 2805920832);
        assert_eq!(packet.timestamp_difference(), 10000);
        assert_eq!(packet.wnd_size(), 61440);
        assert_eq!(packet.seq_nr(), 15404);
        assert_eq!(packet.ack_nr(), 32437);
        assert!(packet.get_payload().is_empty());
    }

    #[test]
    fn test_decode_packet_with_invalid_extension() {
        let buf = [
            0x21, 0x1, 0xA4, 0x46, 0xA7, 0x3E, 0xF4, 0x40, 0x0, 0x0, 0x27, 0x10, 0x0, 0x0, 0xF0,
            0x0, 0x3C, 0x2C, 0x7E, 0xB5, 0x0, 0x4, 0xAA, 0x3C, 0x0,
        ];

        let packet = Packet::try_from(&buf[..]);
        assert!(packet.is_err());
    }

    #[test]
    fn test_encode_packet() {
        let mut header = PacketHeader::new();
        header.set_type(PacketType::Data);
        header.set_connection_id(49300);
        header.set_timestamp(2805920832);
        header.set_timestamp_difference(1805367832);
        header.set_wnd_size(61440);
        header.set_seq_nr(12044);
        header.set_ack_nr(12024);

        let packet = Packet::new(header);
        assert_eq!(packet.get_type(), PacketType::Data);
        assert_eq!(packet.get_version(), VERSION);
        assert_eq!(packet.extension(), 0);
        assert_eq!(packet.connection_id(), 49300);
        assert_eq!(packet.timestamp(), 2805920832);
        assert_eq!(packet.timestamp_difference(), 1805367832);
        assert_eq!(packet.wnd_size(), 61440);
        assert_eq!(packet.seq_nr(), 12044);
        assert_eq!(packet.ack_nr(), 12024);
        assert!(packet.get_payload().is_empty());
    }

    #[test]
    fn test_encode_packet_with_payload() {
        let mut header = PacketHeader::new();
        header.set_type(PacketType::Data);
        header.set_connection_id(49300);
        header.set_timestamp(2805920832);
        header.set_timestamp_difference(1805367832);
        header.set_wnd_size(61440);
        header.set_seq_nr(12044);
        header.set_ack_nr(12024);
        let payload = b"Hello world".to_vec();

        let packet = Packet::with_payload(header, &payload[..]);
        assert_eq!(packet.get_type(), PacketType::Data);
        assert_eq!(packet.get_version(), VERSION);
        assert_eq!(packet.extension(), 0);
        assert_eq!(packet.connection_id(), 49300);
        assert_eq!(packet.timestamp(), 2805920832);
        assert_eq!(packet.timestamp_difference(), 1805367832);
        assert_eq!(packet.wnd_size(), 61440);
        assert_eq!(packet.seq_nr(), 12044);
        assert_eq!(packet.ack_nr(), 12024);
        assert_eq!(packet.get_payload(), &payload[..]);
    }

    #[test]
    fn test_empty_packet() {
        let packet = Packet::try_from(&[][..]);
        assert!(packet.is_err());
    }

    #[test]
    fn test_selective_ack() {
        let mut header = PacketHeader::new();
        header.set_type(PacketType::State);
        header.set_connection_id(49300);
        header.set_timestamp(2805920832);
        header.set_timestamp_difference(1805367832);
        header.set_wnd_size(61440);
        header.set_seq_nr(12044);
        header.set_ack_nr(12024);

        let mut reply = Packet::new(header);
        reply.set_selective_ack(vec![0b1001_1101, 0b0000_0000, 0b0101_1010, 0b0000_0001]);

        assert_eq!(reply.get_extensions()[0].bitmask[0], 0b1001_1101);
        assert_eq!(reply.get_extensions()[0].bitmask[1], 0b0000_0000);
        assert_eq!(reply.get_extensions()[0].bitmask[2], 0b0101_1010);
        assert_eq!(reply.get_extensions()[0].bitmask[3], 0b0000_0001);
    }

    // https://github.com/ethereum/portal-network-specs/pull/127
    // konrad uTP test vectors

    #[test]
    fn test_syn_packet() {
        let mut header = PacketHeader::new();
        header.set_type(PacketType::Syn);
        header.set_connection_id(10049);
        header.set_timestamp(3384187322);
        header.set_timestamp_difference(0);
        header.set_wnd_size(1048576);
        header.set_seq_nr(11884);
        header.set_ack_nr(0);

        let packet = Packet::new(header);
        assert_eq!(
            hex::encode(packet.as_ref()),
            "41002741c9b699ba00000000001000002e6c0000"
        );
    }

    #[test]
    fn test_act_packet_no_extension() {
        let mut header = PacketHeader::new();
        header.set_type(PacketType::State);
        header.set_connection_id(10049);
        header.set_timestamp(6195294);
        header.set_timestamp_difference(916973699);
        header.set_wnd_size(1048576);
        header.set_seq_nr(16807);
        header.set_ack_nr(11885);

        let packet = Packet::new(header);
        assert_eq!(
            hex::encode(packet.as_ref()),
            "21002741005e885e36a7e8830010000041a72e6d"
        );
    }

    #[test]
    fn test_act_packet_with_selective_ack_extension() {
        let mut header = PacketHeader::new();
        header.set_type(PacketType::State);
        header.set_connection_id(10049);
        header.set_timestamp(6195294);
        header.set_timestamp_difference(916973699);
        header.set_wnd_size(1048576);
        header.set_seq_nr(16807);
        header.set_ack_nr(11885);

        let mut reply = Packet::new(header);
        reply.set_selective_ack(vec![1, 0, 0, 128]);

        assert_eq!(
            hex::encode(reply.as_ref()),
            "21012741005e885e36a7e8830010000041a72e6d000401000080"
        );
    }

    #[test]
    fn test_data_packet() {
        let mut header = PacketHeader::new();
        header.set_type(PacketType::Data);
        header.set_connection_id(26237);
        header.set_timestamp(252492495);
        header.set_timestamp_difference(242289855);
        header.set_wnd_size(1048576);
        header.set_seq_nr(8334);
        header.set_ack_nr(16806);

        let packet = Packet::with_payload(header, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            hex::encode(packet.as_ref()),
            "0100667d0f0cbacf0e710cbf00100000208e41a600010203040506070809"
        );
    }

    #[test]
    fn test_fin_packet() {
        let mut header = PacketHeader::new();
        header.set_type(PacketType::Fin);
        header.set_connection_id(19003);
        header.set_timestamp(515227279);
        header.set_timestamp_difference(511481041);
        header.set_wnd_size(1048576);
        header.set_seq_nr(41050);
        header.set_ack_nr(16806);

        let packet = Packet::new(header);
        assert_eq!(
            hex::encode(packet.as_ref()),
            "11004a3b1eb5be8f1e7c94d100100000a05a41a6"
        );
    }

    #[test]
    fn test_reset_packet() {
        let mut header = PacketHeader::new();
        header.set_type(PacketType::Reset);
        header.set_connection_id(62285);
        header.set_timestamp(751226811);
        header.set_timestamp_difference(0);
        header.set_wnd_size(0);
        header.set_seq_nr(55413);
        header.set_ack_nr(16807);

        let packet = Packet::new(header);
        assert_eq!(
            hex::encode(packet.as_ref()),
            "3100f34d2cc6cfbb0000000000000000d87541a7"
        );
    }
}
