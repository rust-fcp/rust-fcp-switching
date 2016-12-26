extern crate hex;
extern crate rand;
extern crate byteorder;
extern crate fcp_cryptoauth;
extern crate fcp_switching;

use byteorder::BigEndian;
use byteorder::ByteOrder;

use std::net::{UdpSocket, SocketAddr, IpAddr, Ipv6Addr};
use std::collections::HashMap;

use fcp_cryptoauth::wrapper::*;

use fcp_switching::switch_packet::{SwitchPacket, PacketType};
use fcp_switching::switch_packet::Payload as SwitchPayload;
use fcp_switching::operation::RoutingDecision;
use fcp_switching::control::ControlPacket;
use fcp_switching::route_packet::RoutePacket;
use fcp_switching::data_packet::DataPacket;
use fcp_switching::data_packet::Payload as DataPayload;

use hex::ToHex;
use rand::Rng;

struct Interface {
    id: u8,
    ca_session: Wrapper<String>,
    addr: SocketAddr,
}

struct Switch {
    sock: UdpSocket,
    interfaces: Vec<Interface>,
    my_pk: PublicKey,
    my_sk: SecretKey,
    inner_conns: HashMap<u32, Wrapper<()>>,
}

impl Switch {
    fn new(sock: UdpSocket, interfaces: Vec<Interface>, my_pk: PublicKey, my_sk: SecretKey) -> Switch {
        Switch {
            sock: sock,
            interfaces: interfaces,
            inner_conns: HashMap::new(),
            my_pk: my_pk,
            my_sk: my_sk
            }
    }

    fn reverse_iface_id(&self, iface_id: u8) -> u8 {
        match iface_id {
            0b000 => 0b000,
            0b001 => 0b100,
            0b010 => 0b010,
            0b011 => 0b110,
            0b100 => 0b001,
            0b101 => 0b101,
            0b110 => 0b011,
            0b111 => 0b111,
            _ => panic!("Iface id greater than 0b111"),
        }
    }

    fn random_send_ping(&mut self, switch_packet: &SwitchPacket) {
        if rand::thread_rng().next_u32() > 0x7fffffff {
            let ping = ControlPacket::Ping { version: 17, opaque_data: vec![1, 2, 3, 4, 5, 6, 7, 8] };
            let mut packet_response = SwitchPacket::new_reply(&switch_packet, &PacketType::Opaque, SwitchPayload::Control(ping)).unwrap();
            self.send(&mut packet_response, 0b001);
            println!("Sending Ping SwitchPacket: {}", packet_response.raw.to_hex());
        }
    }

    fn send(&mut self, packet: &mut SwitchPacket, from_interface: u8) {
        match packet.switch(3, &(self.reverse_iface_id(from_interface) as u64)) {
            RoutingDecision::SelfInterface(_) => {
                self.on_self_interface_switch_packet(packet);
            }
            RoutingDecision::Forward(iface_id) => {
                let mut sent = false;
                for interface in self.interfaces.iter_mut() {
                    if interface.id as u64 == iface_id {
                        sent = true;
                        for packet in interface.ca_session.wrap_message(&packet.raw) {
                            self.sock.send_to(&packet, interface.addr).unwrap();
                        }
                    }
                }
                if !sent {
                    panic!(format!("Iface {} not found for packet: {:?}", iface_id, packet));
                }
            }
        }
    }

    fn on_inner_ca_message(&mut self, switch_packet: &SwitchPacket, handle: u32, ca_message: Vec<u8>) {
        println!("Received CA packet, containing: {}", ca_message.to_hex());
        println!("ie: {}", DataPacket { raw: ca_message });
        if rand::thread_rng().next_u32() > 0x7fffffff {
            let getpeers_message = DataPacket::new(2, &DataPayload::RoutePacket(RoutePacket::GetPeers { encoding_index: 1, encoding_scheme: None, transaction_id: b"blah".to_vec(), version: 17 }));
            println!("Sending getpeers: {}", getpeers_message.raw.to_hex());
            let mut responses = Vec::new();
            {
                let inner_conn = self.inner_conns.get_mut(&handle).unwrap();
                for packet_response in inner_conn.wrap_message_immediately(&getpeers_message.raw) {
                    if BigEndian::read_u32(&packet_response[0..4]) < 4 {
                        responses.push(SwitchPacket::new_reply(&switch_packet, &PacketType::Opaque, SwitchPayload::CryptoAuthHandshake(packet_response)).unwrap());
                    }
                    else {
                        let peer_handle = inner_conn.peer_session_handle().unwrap();
                        responses.push(SwitchPacket::new_reply(&switch_packet, &PacketType::Opaque, SwitchPayload::Other(peer_handle, packet_response)).unwrap());
                    }
                }
            }
            for mut response in responses {
                self.send(&mut response, 0b001);
            }
        }
    }

    fn on_self_interface_switch_packet(&mut self, switch_packet: &SwitchPacket) {
        match switch_packet.payload() {
            Some(SwitchPayload::Control(ControlPacket::Ping { opaque_data, .. })) => {
                let control_response = ControlPacket::Pong { version: 17, opaque_data: opaque_data };
                let mut packet_response = SwitchPacket::new_reply(switch_packet, &PacketType::Opaque, SwitchPayload::Control(control_response)).unwrap();
                self.send(&mut packet_response, 0b001);

                self.random_send_ping(switch_packet);
            },
            Some(SwitchPayload::Control(ControlPacket::Pong { opaque_data, .. })) => {
                assert_eq!(opaque_data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
                println!("Received pong.");
            },
            Some(SwitchPayload::CryptoAuthHandshake(handshake)) => {
                let mut handle;
                loop {
                    handle = rand::thread_rng().next_u32();
                    if !self.inner_conns.contains_key(&handle) {
                        break
                    }
                };
                let (mut inner_conn, inner_packet) = Wrapper::new_incoming_connection(self.my_pk, self.my_sk.clone(), Credentials::None, None, Some(handle), handshake.clone()).unwrap();
                println!("Received CA handshake, containing: {}", inner_packet.to_hex());
                let inner_packets = match inner_conn.unwrap_message(handshake) {
                    Ok(inner_packets) => inner_packets,
                    Err(e) => panic!("CA error: {:?}", e),
                };
                self.inner_conns.insert(handle, inner_conn);
                for inner_packet in inner_packets {
                    self.on_inner_ca_message(switch_packet, handle, inner_packet)
                }
                self.random_send_ping(switch_packet);
            },
            Some(SwitchPayload::Other(handle, ca_message)) => {
                println!("Received inner CA packet");
                let inner_packets = match self.inner_conns.get_mut(&handle) {
                    Some(inner_conn) => {
                        match inner_conn.unwrap_message(ca_message) {
                            Ok(inner_packets) => inner_packets,
                            Err(e) => panic!("CA error: {:?}", e),
                        }
                    }
                    None => panic!("Received unknown handle.")
                };
                for inner_packet in inner_packets {
                    self.on_inner_ca_message(switch_packet, handle, inner_packet)
                }
            }
            _ => panic!("Can only handle Pings, Pongs, and CA."),
        }
    }

    fn on_outer_ca_message(&mut self, from_addr: SocketAddr, buf: Vec<u8>) {
        let mut messages = None;
        for interface in self.interfaces.iter_mut() {
            if interface.addr == from_addr {
                messages = Some(interface.ca_session.unwrap_message(buf).unwrap());
                break;
            }
        }
        let messages = messages.unwrap();

        for message in messages {
            let mut switch_packet = SwitchPacket { raw: message };
            println!("Received switch packet: {}. Type: {:?}, Label: {}, payload: {:?}", switch_packet.raw.to_hex(), switch_packet.packet_type(), switch_packet.label().to_hex(), switch_packet.payload());
            let decision = switch_packet.switch(3, &0b110);
            match decision {
                RoutingDecision::SelfInterface(_) => {
                    self.on_self_interface_switch_packet(&switch_packet);
                },
                RoutingDecision::Forward(director) => panic!(format!("Can only route to self interface, but switch wanted to forward to director {}.", director)),
            }
        }
    }

    fn loop_(&mut self) {
        loop {
            for interface in self.interfaces.iter_mut() {
                for packet in interface.ca_session.upkeep() {
                    self.sock.send_to(&packet, interface.addr).unwrap();
                }
            }

            let mut buf = vec![0u8; 1024];
            let (nb_bytes, addr) = self.sock.recv_from(&mut buf).unwrap();
            assert!(nb_bytes < 1024);
            buf.truncate(nb_bytes);
            println!("Received packet: {}", buf.to_hex());
            self.on_outer_ca_message(addr, buf);
        }
    }
}

pub fn main() {
    fcp_cryptoauth::init();

    let my_sk = SecretKey::from_hex(b"ac3e53b518e68449692b0b2f2926ef2fdc1eac5b9dbd10a48114263b8c8ed12e").unwrap();
    let my_pk = PublicKey::from_base32(b"2wrpv8p4tjwm532sjxcbqzkp7kdwfwzzbg7g0n5l6g3s8df4kvv0.k").unwrap();
    let their_pk = PublicKey::from_base32(b"2j1xz5k5y1xwz7kcczc4565jurhp8bbz1lqfu9kljw36p3nmb050.k").unwrap();
    // Corresponding secret key: 824736a667d85582747fde7184201b17d0e655a7a3d9e0e3e617e7ca33270da8
    let login = "foo".to_owned().into_bytes();
    let password = "bar".to_owned().into_bytes();
    let credentials = Credentials::LoginPassword {
        login: login,
        password: password,
    };
    let mut allowed_peers = HashMap::new();
    allowed_peers.insert(credentials.clone(), "my peer".to_owned());

    let sock = UdpSocket::bind("[::1]:12345").unwrap();
    let dest = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)), 54321);

    let conn = Wrapper::new_outgoing_connection(
            my_pk, my_sk.clone(), their_pk, credentials, Some(allowed_peers.clone()), "my peer".to_owned(), None);

    let interfaces = vec![Interface { id: 0b011, ca_session: conn, addr: dest }];

    let mut switch = Switch::new(sock, interfaces, my_pk, my_sk);

    switch.loop_();
}
