use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bytes::Bytes;
use get_if_addrs::{get_if_addrs, IfAddr};
use hkdf::Hkdf;
use num_bigint::{BigUint, ToBigInt};
use p256::elliptic_curve::rand_core::OsRng;
use p256::{PublicKey, SecretKey};
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use ts_rs::TS;

use crate::CUSTOM_DOWNLOAD;

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize, TS)]
#[ts(export)]
#[allow(dead_code)]
pub enum DeviceType {
    Unknown = 0,
    Phone = 1,
    Tablet = 2,
    Laptop = 3,
}

#[allow(dead_code)]
impl DeviceType {
    pub fn from_raw_value(value: u8) -> Self {
        match value {
            0 => DeviceType::Unknown,
            1 => DeviceType::Phone,
            2 => DeviceType::Tablet,
            3 => DeviceType::Laptop,
            _ => DeviceType::Unknown,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct RemoteDeviceInfo {
    pub name: String,
    pub device_type: DeviceType,
}

impl RemoteDeviceInfo {
    pub fn serialize(&self) -> Vec<u8> {
        // 1 byte: Version(3 bits)|Visibility(1 bit)|Device Type(3 bits)|Reserved(1 bit)
        let mut endpoint_info: Vec<u8> = vec![((self.device_type.clone() as u8) << 1) & 0b111];

        // 16 bytes: unknown random bytes
        endpoint_info.extend((0..16).map(|_| rand::rng().random_range(0..=255)));

        // Device name in UTF-8 prefixed with 1-byte length
        let mut name_chars = self.name.as_bytes().to_vec();
        if name_chars.len() > 255 {
            name_chars.truncate(255);
        }
        endpoint_info.push(name_chars.len() as u8);
        endpoint_info.extend(name_chars);

        endpoint_info
    }
}

pub fn gen_mdns_name(endpoint_id: [u8; 4]) -> String {
    let mut name_b = Vec::new();

    let pcp: [u8; 1] = [0x23];
    name_b.extend_from_slice(&pcp);

    name_b.extend_from_slice(&endpoint_id);

    let service_id: [u8; 3] = [0xFC, 0x9F, 0x5E];
    name_b.extend_from_slice(&service_id);

    let unknown_bytes: [u8; 2] = [0x00, 0x00];
    name_b.extend_from_slice(&unknown_bytes);

    URL_SAFE_NO_PAD.encode(&name_b)
}

pub fn gen_mdns_endpoint_info(device_type: u8, device_name: &str) -> String {
    let mut record = Vec::new();

    // 1 byte: Version(3 bits)|Visibility(1 bit)|Device Type(3 bits)|Reserved(1 bits)
    // Device types: unknown=0, phone=1, tablet=2, laptop=3
    record.push(device_type << 1);

    let unknown_bytes = rand::rng().random::<[u8; 16]>();
    record.extend_from_slice(&unknown_bytes);

    let device_name = device_name.as_bytes();
    let length = device_name.len() as u8;
    record.push(length);
    record.extend_from_slice(device_name);

    URL_SAFE_NO_PAD.encode(&record)
}

pub fn parse_mdns_endpoint_info(encoded_str: &str) -> Result<(DeviceType, String), anyhow::Error> {
    let decoded_bytes = URL_SAFE_NO_PAD.decode(encoded_str)?;
    if decoded_bytes.len() < 19 {
        return Err(anyhow!("Invalid data length"));
    }

    let device_type = (decoded_bytes[0] >> 1) & 0x7;
    let name_length = decoded_bytes[17] as usize;
    if 18 + name_length > decoded_bytes.len() {
        return Err(anyhow!("Invalid name length"));
    }

    let device_name_bytes = &decoded_bytes[18..18 + name_length];
    let device_name = String::from_utf8(device_name_bytes.to_vec())?;

    Ok((DeviceType::from_raw_value(device_type), device_name))
}

pub async fn stream_read_exact(
    socket: &mut TcpStream,
    buf: &mut [u8],
) -> Result<(), anyhow::Error> {
    match socket.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn gen_ecdsa_keypair() -> (SecretKey, PublicKey) {
    let secret_key = SecretKey::random(&mut OsRng);
    let public_key = secret_key.public_key();

    (secret_key, public_key)
}

pub fn encode_point(unsigned: Bytes) -> Result<Vec<u8>, anyhow::Error> {
    let big_int = BigUint::from_bytes_be(&unsigned)
        .to_bigint()
        .ok_or_else(|| anyhow!("Failed to convert to bigint"))?;

    Ok(big_int.to_signed_bytes_be())
}

pub fn hkdf_extract_expand(
    salt: &[u8],
    input: &[u8],
    info: &[u8],
    output_len: usize,
) -> Result<Vec<u8>, anyhow::Error> {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), input);
    let mut okm = vec![0u8; output_len];
    hkdf.expand(info, &mut okm)
        .map_err(|e| anyhow!("HKDF expand failed: {}", e))?;
    Ok(okm)
}

pub fn to_four_digit_string(bytes: &Vec<u8>) -> String {
    let k_hash_modulo = 9973;
    let k_hash_base_multiplier = 31;

    let mut hash = 0;
    let mut multiplier = 1;
    for &byte in bytes {
        let byte = byte as i8 as i32;
        hash = (hash + byte * multiplier) % k_hash_modulo;
        multiplier = (multiplier * k_hash_base_multiplier) % k_hash_modulo;
    }

    format!("{:04}", hash.abs())
}

pub fn gen_random(size: usize) -> Vec<u8> {
    let mut data = vec![0; size];
    rand::rng().fill_bytes(&mut data);

    data
}

pub fn get_download_dir() -> PathBuf {
    let cdown = CUSTOM_DOWNLOAD.read();
    match cdown {
        Ok(mg) => {
            if mg.is_some() {
                return mg.as_ref().unwrap().to_path_buf();
            }
        }
        Err(_) => {
            // TODO
        }
    }

    if let Some(user_dirs) = directories::UserDirs::new() {
        if let Some(dd) = user_dirs.download_dir() {
            return dd.to_path_buf();
        }

        return user_dirs.home_dir().to_path_buf();
    }

    Path::new("/").to_path_buf()
}

pub fn is_not_self_ip(ip_address: &Ipv4Addr) -> bool {
    if let Ok(if_addrs) = get_if_addrs() {
        for if_addr in if_addrs {
            if if_addr.ip() == *ip_address {
                return false;
            }
        }
    }

    true
}

/// Parses an IPv4 address as `/proc/net/route` prints it: a 32-bit number in
/// hex, in the host's byte order.
fn parse_route_address(value: &str) -> Option<Ipv4Addr> {
    let raw = u32::from_str_radix(value.trim(), 16).ok()?;
    Some(Ipv4Addr::from(raw.to_le_bytes()))
}

/// The default route: the interface that carries it and the gateway it points
/// at.
///
/// The route table is read from `/proc/net/route`. The columns are the
/// interface name, the destination, the gateway, the flags, the reference
/// count, the use count, the metric, the mask, the MTU, the window and the
/// RTT. A default route is the row whose destination is all zeroes.
fn default_route() -> Option<(String, Ipv4Addr)> {
    let table = std::fs::read_to_string("/proc/net/route").ok()?;
    parse_default_route(&table)
}

/// The default route of a route table in the `/proc/net/route` format.
///
/// A default route with an unspecified gateway is a "default device" route:
/// it says which interface the traffic leaves by, but there is no gateway to
/// pick a subnet from. Such a route is only used when no route with a real
/// gateway exists, because the gateway is what identifies the subnet the
/// peers are on.
fn parse_default_route(table: &str) -> Option<(String, Ipv4Addr)> {
    let mut best: Option<(bool, u64, String, Ipv4Addr)> = None;

    for line in table.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 7 || fields[1] != "00000000" {
            continue;
        }

        let Some(gateway) = parse_route_address(fields[2]) else {
            continue;
        };

        let metric = fields[6].parse::<u64>().unwrap_or(u64::MAX);
        // A route with a gateway outranks one without, whatever the metric
        // says: the gateway is what names the subnet the peers are on.
        let has_gateway = !gateway.is_unspecified();

        let is_better = match &best {
            None => true,
            Some((best_has_gateway, best_metric, _, _)) => {
                has_gateway && !*best_has_gateway
                    || (has_gateway == *best_has_gateway && metric < *best_metric)
            }
        };

        if is_better {
            best = Some((has_gateway, metric, fields[0].to_owned(), gateway));
        }
    }

    best.map(|(_, _, name, gateway)| (name, gateway))
}

/// Whether two addresses are in the same subnet.
fn same_subnet(ip: Ipv4Addr, netmask: Ipv4Addr, other: Ipv4Addr) -> bool {
    let mask = u32::from(netmask);
    (u32::from(ip) & mask) == (u32::from(other) & mask)
}

/// The IPv4 address a peer on the local network can reach.
///
/// The mDNS advertisement must name exactly one address. The daemon opens a
/// socket per host address and answers the queries on every one of them, so
/// an address a peer cannot route to becomes a second, competing answer for
/// the same service.
///
/// The address of the default route's subnet is the one a peer on that network
/// can reach. A Wi-Fi Direct group owner (MTA's `192.168.49.1`), a VPN
/// address and a Docker bridge address all live on this host too; picking by
/// subnet excludes them without a blacklist.
pub fn reachable_ipv4() -> Option<Ipv4Addr> {
    let interfaces = get_if_addrs().ok()?;
    select_reachable_ipv4(&interfaces, default_route().as_ref())
}

/// The address to advertise, out of the host's interfaces and its default
/// route.
fn select_reachable_ipv4(
    interfaces: &[get_if_addrs::Interface],
    route: Option<&(String, Ipv4Addr)>,
) -> Option<Ipv4Addr> {
    if let Some((iface_name, gateway)) = route {
        // The address of the interface's own subnet that holds the gateway.
        for interface in interfaces {
            if interface.name != *iface_name {
                continue;
            }
            let IfAddr::V4(v4) = &interface.addr else {
                continue;
            };
            if !gateway.is_unspecified() && same_subnet(v4.ip, v4.netmask, *gateway) {
                return Some(v4.ip);
            }
        }

        // The gateway is on link (a point-to-point route) or it is in a subnet
        // that this host has no address for: the interface's first address is
        // the best guess left.
        for interface in interfaces {
            if interface.name != *iface_name {
                continue;
            }
            let IfAddr::V4(v4) = &interface.addr else {
                continue;
            };
            if !v4.ip.is_loopback() {
                return Some(v4.ip);
            }
        }
    }

    // No default route, or an unreadable route table: the host's first
    // non-loopback address.
    for interface in interfaces {
        let IfAddr::V4(v4) = &interface.addr else {
            continue;
        };
        if !v4.ip.is_loopback() {
            return Some(v4.ip);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use get_if_addrs::{IfAddr, Ifv4Addr, Interface};

    #[test]
    fn test_gen_and_parse_mdns_info() {
        let device_name = "a_device_name";
        let device_type = DeviceType::Laptop;

        dbg!(&device_type);
        dbg!(device_type.clone() as u8);

        let info = gen_mdns_endpoint_info(device_type.clone() as u8, device_name);
        let parse_info = parse_mdns_endpoint_info(&info).unwrap();

        assert_eq!(parse_info.1, device_name);
        assert_eq!(parse_info.0, device_type);
    }

    /// An interface with one IPv4 address.
    fn interface(name: &str, ip: Ipv4Addr, netmask: Ipv4Addr) -> Interface {
        Interface {
            name: name.to_owned(),
            addr: IfAddr::V4(Ifv4Addr {
                ip,
                netmask,
                broadcast: None,
            }),
        }
    }

    #[test]
    fn the_default_route_is_the_row_with_the_zero_destination() {
        let table = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0
docker0\t00000000\t00000000\t0001\t0\t0\t0\t00000000\t0\t0\t0
";

        let (name, gateway) = parse_default_route(table).unwrap();

        assert_eq!(name, "wlan0");
        // 0101A8C0 little-endian is C0A80101, which is 192.168.1.1.
        assert_eq!(gateway, Ipv4Addr::new(192, 168, 1, 1));
    }

    #[test]
    fn the_address_of_the_gateways_subnet_is_the_one_to_advertise() {
        // The host carries the address of the default route's subnet and the
        // address of a Wi-Fi Direct group owner on the same interface. Only the
        // first one is reachable from the network.
        let interfaces = vec![
            interface(
                "wlan0",
                Ipv4Addr::new(192, 168, 49, 1),
                Ipv4Addr::new(255, 255, 255, 0),
            ),
            interface(
                "wlan0",
                Ipv4Addr::new(192, 168, 1, 5),
                Ipv4Addr::new(255, 255, 255, 0),
            ),
        ];
        let route = ("wlan0".to_owned(), Ipv4Addr::new(192, 168, 1, 1));

        let address = select_reachable_ipv4(&interfaces, Some(&route)).unwrap();

        assert_eq!(address, Ipv4Addr::new(192, 168, 1, 5));
    }

    #[test]
    fn an_address_on_another_subnet_is_never_the_one_to_advertise() {
        let interfaces = vec![interface(
            "wlan0",
            Ipv4Addr::new(10, 2, 0, 111),
            Ipv4Addr::new(255, 255, 255, 0),
        )];
        let route = ("wlan0".to_owned(), Ipv4Addr::new(192, 168, 1, 1));

        // No address of this interface is in the gateway's subnet, so the
        // interface's own address is the best guess left.
        let address = select_reachable_ipv4(&interfaces, Some(&route)).unwrap();

        assert_eq!(address, Ipv4Addr::new(10, 2, 0, 111));
    }

    #[test]
    fn without_a_default_route_the_first_non_loopback_address_is_used() {
        let interfaces = vec![
            interface(
                "lo",
                Ipv4Addr::new(127, 0, 0, 1),
                Ipv4Addr::new(255, 0, 0, 0),
            ),
            interface(
                "eth0",
                Ipv4Addr::new(172, 17, 0, 2),
                Ipv4Addr::new(255, 255, 0, 0),
            ),
        ];

        let address = select_reachable_ipv4(&interfaces, None).unwrap();

        assert_eq!(address, Ipv4Addr::new(172, 17, 0, 2));
    }

    #[test]
    fn the_address_to_advertise_is_the_one_of_the_gateways_subnet() {
        // The check runs against this host's real route table: the address it
        // picks has to be in the gateway's subnet.
        let Some((iface_name, gateway)) = default_route() else {
            // A host without a default route has nothing to check.
            return;
        };

        let Some(address) = reachable_ipv4() else {
            return;
        };

        let interfaces = get_if_addrs().unwrap();
        let interface = interfaces
            .iter()
            .find(|i| i.name == iface_name)
            .expect("the default route's interface exists");

        let IfAddr::V4(v4) = &interface.addr else {
            return;
        };

        if !gateway.is_unspecified() {
            assert!(
                same_subnet(address, v4.netmask, gateway),
                "{address} is not in the subnet of the gateway {gateway}"
            );
        }
    }
}
