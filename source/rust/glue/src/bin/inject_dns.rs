use {
    aargvark::{
        vark,
        Aargvark,
    },
    flowcontrol::shed,
    glue::unstable_ip::UnstableIpv6,
    loga::{
        fatal,
        ResultContext,
    },
    network_interface::{
        NetworkInterface,
        NetworkInterfaceConfig,
    },
    nfq::{
        Queue,
        Verdict,
    },
    std::{
        net::Ipv6Addr,
        panic,
        process,
        sync::{
            Arc,
            Mutex,
        },
        thread::{
            sleep,
            spawn,
        },
        time::Duration,
    },
};

#[inline]
fn checksum_roll(sum32: &mut u32, bytes: &[u8]) {
    let mut iter = bytes.chunks_exact(2);
    for x in &mut iter {
        *sum32 += u16::from_ne_bytes(x.try_into().unwrap()) as u32;
    }
    if let Some(remainder) = iter.remainder().first() {
        let pair = [*remainder, 0x00];
        *sum32 += u16::from_ne_bytes(pair) as u32;
    }
}

fn checksum_finish(sum32: u32) -> [u8; 2] {
    let high = (sum32 >> 16) as u16;
    let low = (sum32 & 0xFFFF) as u16;
    return (!(high + low).to_be()).to_be_bytes();
}

fn icmpv6_udp_checksum(source: &[u8]) -> Option<[u8; 2]> {
    // * IPv6 pseudo-header https://datatracker.ietf.org/doc/html/rfc2460#section-8.1
    //
    // * ICMP https://datatracker.ietf.org/doc/html/rfc4443#section-2.3
    //
    //   Pseudo header + whole body
    //
    // * UDP https://datatracker.ietf.org/doc/html/rfc768
    //
    //   Pseudo header + whole body
    let mut sum32 = 0u32;

    // Icmpv6 length (pseudo header)
    checksum_roll(&mut sum32, source.get(4 .. 6)?);

    // Next header (pseudo header)
    sum32 += u16::from_ne_bytes([0x00, *source.get(6)?]) as u32;

    // Source addr (pseudo header), dest addr (pseudo header), payload
    checksum_roll(&mut sum32, source.get(8..)?);

    // Then do some rfc magic
    return Some(checksum_finish(sum32));
}

fn modify(source: &[u8], lifetime: u32, ip: Ipv6Addr) -> Option<Vec<u8>> {
    let mut ipv6_packet = vec![];
    ipv6_packet.reserve(source.len() + 128);
    ipv6_packet.extend_from_slice(source);

    #[must_use]
    fn splice(packet: &mut Vec<u8>, start: usize, end: Option<usize>, data: &[u8]) -> Option<()> {
        if start > packet.len() {
            return None;
        }
        if let Some(end) = end {
            if end > packet.len() {
                return None;
            }
            packet.splice(start .. end, data.iter().cloned());
        } else {
            packet.splice(start.., data.iter().cloned());
        }
        return Some(());
    }

    #[must_use]
    fn replace_u16(packet: &mut Vec<u8>, start: usize, data: &[u8; 2]) -> Option<()> {
        packet.get_mut(start .. start + 2)?.copy_from_slice(data);
        return Some(());
    }

    // Check that it's RA
    const IPV6_PAYLOAD_START: usize = 40;
    match *ipv6_packet.get(6)? {
        // ICMP
        //
        // * https://datatracker.ietf.org/doc/html/rfc4443
        //
        // RA
        //
        // * https://datatracker.ietf.org/doc/html/rfc4861#section-4.2
        58 => {
            // Confirm it's RA
            let Some(type_) = ipv6_packet.get(IPV6_PAYLOAD_START) else {
                return None;
            };
            if *type_ != 134 {
                return None;
            }

            // Modify RA
            const OPT_RDNSS: u8 = 25;
            const RA_FIXED_HEADER_SIZE: usize = 16;
            const RA_OPTIONS_START: usize = IPV6_PAYLOAD_START + RA_FIXED_HEADER_SIZE;

            // Set other info flag
            *ipv6_packet.get_mut(IPV6_PAYLOAD_START + 5)? |= 0x40;

            // Copy + filter out RDNSS
            let mut at_option_start = RA_OPTIONS_START;
            let mut new_options = vec![];
            new_options.reserve(ipv6_packet.len() - IPV6_PAYLOAD_START);
            loop {
                if at_option_start == ipv6_packet.len() {
                    break;
                }
                let at_option_type = *ipv6_packet.get(at_option_start)?;
                let at_option_length = *ipv6_packet.get(at_option_start + 1)? as usize * 8;
                shed!{
                    'next_option _;
                    if at_option_type == OPT_RDNSS {
                        // Drop RDNSS
                        break 'next_option;
                    }
                    // Keep anything not RDNSS
                    new_options.extend_from_slice(ipv6_packet.get(at_option_start .. at_option_start + at_option_length)?);
                }
                at_option_start += at_option_length;
            }

            // Generate custom RDNSS
            new_options.push(OPT_RDNSS);
            let lifetime_bytes = lifetime.to_be_bytes();
            let ip_bytes = ip.octets();
            new_options.push(((1 + 1 + 2 + lifetime_bytes.len() + ip_bytes.len()) / 8) as u8);
            new_options.extend_from_slice(&[0, 0]);
            new_options.extend(lifetime_bytes);
            new_options.extend(ip_bytes);

            // Replace options
            splice(&mut ipv6_packet, RA_OPTIONS_START, None, &new_options)?;

            // Update ipv6 payload length
            replace_u16(&mut ipv6_packet, 4, &((RA_FIXED_HEADER_SIZE + new_options.len()) as u16).to_be_bytes())?;

            // Recalc checksum
            ipv6_packet.get_mut(IPV6_PAYLOAD_START + 2 .. IPV6_PAYLOAD_START + 4)?.fill(0);
            let new_checksum = icmpv6_udp_checksum(&ipv6_packet)?;
            replace_u16(&mut ipv6_packet, IPV6_PAYLOAD_START + 2, &new_checksum)?;
        },
        // UDP (DHCPv6)
        //
        // * https://datatracker.ietf.org/doc/html/rfc8415
        17 => {
            const UDP_FIXED_HEADER_SIZE: usize = 8;

            // Confirm it's reply
            if *ipv6_packet.get(IPV6_PAYLOAD_START + UDP_FIXED_HEADER_SIZE)? != 7 {
                return None;
            }

            // Copy + filter out options
            const OPT_DNS: &[u8] = &[0x00, 0x17];
            const DHCP_FIXED_HEADER_SIZE: usize = 4;
            const DHCP_OPTIONS_START: usize = IPV6_PAYLOAD_START + UDP_FIXED_HEADER_SIZE + DHCP_FIXED_HEADER_SIZE;
            let mut at_option_start = DHCP_OPTIONS_START;
            let mut new_options = vec![];
            new_options.reserve(ipv6_packet.len() - IPV6_PAYLOAD_START);
            loop {
                if at_option_start == ipv6_packet.len() {
                    break;
                }
                let at_option_type = ipv6_packet.get(at_option_start .. at_option_start + 2)?;
                let at_option_length =
                    u16::from_be_bytes(
                        ipv6_packet.get(at_option_start + 2 .. at_option_start + 2 + 2)?.try_into().unwrap(),
                    ) as
                        usize +
                        4;
                shed!{
                    'next_option _;
                    if at_option_type == OPT_DNS {
                        // Drop RDNSS
                        break 'next_option;
                    }
                    // Keep anything not RDNSS
                    new_options.extend_from_slice(ipv6_packet.get(at_option_start .. at_option_start + at_option_length)?);
                }
                at_option_start += at_option_length;
            }

            // Generate custom DNS option
            new_options.extend_from_slice(OPT_DNS);
            new_options.extend_from_slice(
                // Length (16 bytes, 1 ip)
                &[0x00, 0x10],
            );
            let ip_bytes = ip.octets();
            new_options.extend(ip_bytes);

            // Replace options
            splice(&mut ipv6_packet, DHCP_OPTIONS_START, None, &new_options)?;

            // Update payload length in udp header
            let new_len = UDP_FIXED_HEADER_SIZE + DHCP_FIXED_HEADER_SIZE + new_options.len();
            replace_u16(&mut ipv6_packet, IPV6_PAYLOAD_START + 4, &(new_len as u16).to_be_bytes())?;

            // Update payload length in ipv6 header
            replace_u16(&mut ipv6_packet, 4, &(new_len as u16).to_be_bytes())?;

            // Recalc checksum
            ipv6_packet.get_mut(IPV6_PAYLOAD_START + 6 .. IPV6_PAYLOAD_START + 8)?.fill(0);
            let new_checksum = icmpv6_udp_checksum(&ipv6_packet)?;
            replace_u16(&mut ipv6_packet, IPV6_PAYLOAD_START + 6, &new_checksum)?;
        },
        _ => {
            return None;
        },
    }

    // Done
    return Some(ipv6_packet);
}

#[derive(Aargvark)]
struct Args {
    /// Name of address to get ipv6 address from to add to RDNSS
    #[vark(flag = "--interface")]
    interface: String,
    /// How often (seconds) to recheck the interface for a new IP. Defaults to 60s.
    /// This is also used as the RDNSS lifetime.
    recheck_period: Option<u64>,
    /// Which netfilter queue to read from
    #[vark(flag = "--nf-queue")]
    nf_queue: u16,
    /// Mark packets after modification - you must use this in your nftables rule to
    /// prevent re-processing the same packet (feedback loop).
    #[vark(flag = "--nf-mark")]
    nf_mark: u32,
}

fn main() {
    match || -> Result<(), loga::Error> {
        let orig_hook = panic::take_hook();
        panic::set_hook(Box::new(move |panic_info| {
            orig_hook(panic_info);
            process::exit(1);
        }));
        let args = vark::<Args>();
        let recheck_period = args.recheck_period.unwrap_or(60);
        let rdnss_lifetime = recheck_period as u32;
        let mut nf_queue = Queue::open().context("Error opening netfilter queue")?;
        nf_queue.bind(args.nf_queue).context("Error binding netfilter queue")?;
        let ip_rxtx = Arc::new(Mutex::new(None));

        // Wait for initial ip, or get next ip
        spawn({
            let ip_rxtx = ip_rxtx.clone();
            let want_iface = args.interface;
            move || {
                let mut found_first = false;
                loop {
                    let mut found = None;
                    for iface in NetworkInterface::show()
                        .context("Failure listing network interfaces")
                        .unwrap()
                        .iter() {
                        if want_iface != iface.name {
                            continue;
                        }
                        for addr in &iface.addr {
                            let std::net::IpAddr::V6(addr) = addr.ip() else {
                                continue;
                            };
                            if !addr.unstable_is_global() {
                                continue;
                            }
                            found = Some(addr);
                            found_first = true;
                        }
                    }
                    if found.is_none() {
                        eprintln!("Interface not found or no global ipv6 address found on interface.");
                    }
                    *ip_rxtx.lock().unwrap() = Some(found);
                    if !found_first {
                        sleep(Duration::from_secs(5));
                    } else {
                        sleep(Duration::from_secs(recheck_period));
                    }
                }
            }
        });

        // Drop RAs until we get an ip
        eprintln!("Starting, waiting for first packet, then dropping packets until global IP found");
        let (mut nf_queue_msg, mut ip) = loop {
            let mut nf_queue_msg = nf_queue.recv().context("Error reading netfilter queue")?;
            if let Some(Some(ip)) = ip_rxtx.lock().unwrap().take() {
                break (nf_queue_msg, ip);
            }
            nf_queue_msg.set_verdict(Verdict::Drop);
            nf_queue.verdict(nf_queue_msg).context("Error setting netfilter message verdict")?;
        };
        loop {
            eprintln!("Found global IP {}, switching from dropping to rewriting RA packets", ip);

            // Replace/add RDNSS in subsequent RAs (continue with last msg of previous loop).
            // Until we lose the ip again.
            loop {
                // Modify
                match modify(nf_queue_msg.get_payload(), rdnss_lifetime, ip) {
                    Some(ipv6_packet) => {
                        nf_queue_msg.set_payload(ipv6_packet);
                        nf_queue_msg.set_nfmark(args.nf_mark);
                        nf_queue_msg.set_verdict(Verdict::Repeat);
                        nf_queue.verdict(nf_queue_msg).context("Error setting netfilter message verdict")?;
                    },
                    None => {
                        // Bad, not a real RA, or undocumented headers
                        nf_queue_msg.set_verdict(Verdict::Drop);
                        nf_queue.verdict(nf_queue_msg).context("Error setting netfilter message verdict")?;
                    },
                }

                // Wait for next msg
                nf_queue_msg = nf_queue.recv().context("Error reading netfilter queue")?;

                // Check for ips changes
                if let Some(update) = ip_rxtx.lock().unwrap().take() {
                    match update {
                        Some(new_ip) => {
                            ip = new_ip;
                        },
                        None => {
                            break;
                        },
                    }
                }
            }
            eprintln!("Lost IP, switching from modifying RA packets to dropping them");

            // Drop RAs again
            loop {
                nf_queue_msg = nf_queue.recv().context("Error reading netfilter queue")?;
                if let Some(Some(new_ip)) = ip_rxtx.lock().unwrap().take() {
                    ip = new_ip;
                    break;
                }
                nf_queue_msg.set_verdict(Verdict::Drop);
                nf_queue.verdict(nf_queue_msg).context("Error setting netfilter message verdict")?;
            };
        }
    }() {
        Ok(_) => (),
        Err(e) => fatal(e),
    }
}

#[cfg(test)]
mod test {
    use {
        crate::{
            checksum_finish,
            checksum_roll,
            icmpv6_udp_checksum,
            modify,
        },
        std::net::Ipv6Addr,
    };

    const PAYLOAD_RA1: &[u8] = &[
        // IPv6
        0x6b,
        0x80,
        0x00,
        0x00,
        0x00,
        0x20,
        0x3a,
        0xff,
        0xfe,
        0x80,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x4a,
        0x2e,
        0x72,
        0xff,
        0xfe,
        0x63,
        0x7d,
        0x10,
        0xff,
        0x02,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x01,
        // ICMPv6
        0x86,
        0x00,
        // Zero'd checksum
        0x00,
        0x00,
        0x40,
        0xc0,
        0x07,
        0x08,
        0x00,
        0x04,
        0x93,
        0xe0,
        0x00,
        0x00,
        0x27,
        0x10,
        0x01,
        0x01,
        0x48,
        0x2e,
        0x72,
        0x63,
        0x7d,
        0x10,
        0x05,
        0x01,
        0x00,
        0x00,
        0x00,
        0x00,
        0x05,
        0xdc,
    ];
    const PAYLOAD_DHCP1: &[u8] = &[
        // IPv6
        0x6b,
        0x80,
        0x00,
        0x00,
        0x00,
        0x91,
        0x11,
        0x01,
        0xfe,
        0x80,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x40,
        0xff,
        0xfe,
        0x12,
        0x20,
        0x0a,
        0xfe,
        0x80,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0xb2,
        0x6e,
        0xbf,
        0xff,
        0xfe,
        0x39,
        0xbf,
        0x7b,
        // UDP
        0x02,
        0x23,
        0x02,
        0x22,
        0x00,
        0x91,
        // Checksum (zeroed)
        0x00,
        0x00,
        // DHCPv6
        0x07,
        0x56,
        0x20,
        0xfd,
        0x00,
        0x02,
        0x00,
        0x0a,
        0x00,
        0x03,
        0x00,
        0x01,
        0x00,
        0x19,
        0xaa,
        0xbc,
        0xfa,
        0x1b,
        0x00,
        0x01,
        0x00,
        0x0e,
        0x00,
        0x02,
        0x00,
        0x00,
        0xab,
        0x11,
        0xfc,
        0x72,
        0xdb,
        0x43,
        0x6a,
        0xc5,
        0xca,
        0x1f,
        0x00,
        0x17,
        0x00,
        0x20,
        0x24,
        0x04,
        0x01,
        0xa8,
        0x7f,
        0x01,
        0x00,
        0x0b,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x03,
        0x24,
        0x04,
        0x01,
        0xa8,
        0x7f,
        0x01,
        0x00,
        0x0a,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x03,
        0x00,
        0x18,
        0x00,
        0x19,
        0x0a,
        0x66,
        0x6c,
        0x65,
        0x74,
        0x73,
        0x2d,
        0x65,
        0x61,
        0x73,
        0x74,
        0x02,
        0x6a,
        0x70,
        0x00,
        0x05,
        0x69,
        0x70,
        0x74,
        0x76,
        0x66,
        0x02,
        0x6a,
        0x70,
        0x00,
        0x00,
        0x1f,
        0x00,
        0x20,
        0x24,
        0x04,
        0x01,
        0xa8,
        0x11,
        0x02,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x0b,
        0x24,
        0x04,
        0x01,
        0xa8,
        0x11,
        0x02,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x0a,
    ];

    #[test]
    fn test_checksum_roll_ex1() {
        let mut sum32 = 0u32;

        // Wikipedia, checksum set to 0 first
        checksum_roll(
            &mut sum32,
            &[
                0x45,
                0x00,
                0x00,
                0x73,
                0x00,
                0x00,
                0x40,
                0x00,
                0x40,
                0x11,
                0x00,
                0x00,
                0xc0,
                0xa8,
                0x00,
                0x01,
                0xc0,
                0xa8,
                0x00,
                0xc7,
            ],
        );
        assert_eq!(checksum_finish(sum32), [0xb8, 0x61]);
    }

    #[test]
    fn test_checksum_roll_ex2() {
        let mut sum32 = 0u32;

        // RFC 1071 example 1
        checksum_roll(&mut sum32, &[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]);
        assert_eq!(checksum_finish(sum32), [!0xdd, !0xf2]);
    }

    #[test]
    fn test_checksum_roll_ex3() {
        let mut sum32 = 0u32;

        // RFC 1071 example 2a
        checksum_roll(&mut sum32, &[0x00, 0x01, 0xf2]);
        assert_eq!(checksum_finish(sum32), [!0xf2, !0x01]);
    }

    #[test]
    fn test_checksum_roll_ex4() {
        let mut sum32 = 0u32;

        // RFC 1071 example 2b but shifted by 1
        checksum_roll(&mut sum32, &[0x03, 0xf4, 0xf5, 0xf6, 0xf7]);
        assert_eq!(checksum_finish(sum32), [!0xf0, !0xeb]);
    }

    #[test]
    fn test_checksum_ex1() {
        assert_eq!(icmpv6_udp_checksum(PAYLOAD_RA1).unwrap(), [0xfd, 0x40]);
    }

    #[test]
    fn test_checksum_ex2() {
        const PAYLOAD: &[u8] = &[
            0x00,
            0x00,
            0x00,
            0x00,
            // Payload len 32
            0x00,
            0x20,
            // Next header 58
            0x3a,
            0x00,
            // Source
            0xfe,
            0x80,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x88,
            0xc5,
            0x75,
            0x41,
            0xaa,
            0x0c,
            0x58,
            0xee,
            // Dest
            0xff,
            0x02,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            // Icmpv6 type
            0x88,
            // Code
            0x00,
            // Checksum
            0x00,
            0x00,
            // Body
            0x20,
            0x00,
            0x00,
            0x00,
            0xfe,
            0x80,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x88,
            0xc5,
            0x75,
            0x41,
            0xaa,
            0x0c,
            0x58,
            0xee,
            0x02,
            0x01,
            0x38,
            0xea,
            0xa7,
            0x89,
            0xbe,
            0x59,
        ];
        assert_eq!(icmpv6_udp_checksum(PAYLOAD).unwrap(), [0xb8, 0xcc]);
    }

    #[test]
    fn test_modify_ra_ex1() {
        let got = modify(PAYLOAD_RA1, 30, Ipv6Addr::new(1, 2, 3, 4, 5, 6, 7, 8)).unwrap();
        let mut want = vec![
            // IPv6
            0x6b,
            0x80,
            0x00,
            0x00,
            // Length
            0x00,
            0x38,
            0x3a,
            0xff,
            0xfe,
            0x80,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x4a,
            0x2e,
            0x72,
            0xff,
            0xfe,
            0x63,
            0x7d,
            0x10,
            0xff,
            0x02,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            // ICMPv6
            0x86,
            0x00,
            // New checksum
            0xe3,
            0xe3,
            0x40,
            0xc0,
            0x07,
            0x08,
            0x00,
            0x04,
            0x93,
            0xe0,
            0x00,
            0x00,
            0x27,
            0x10,
            0x01,
            0x01,
            0x48,
            0x2e,
            0x72,
            0x63,
            0x7d,
            0x10,
            0x05,
            0x01,
            0x00,
            0x00,
            0x00,
            0x00,
            0x05,
            0xdc,
            // # Extra rdnss start
            //
            // Type
            25,
            // Length
            (1 + 1 + 2 + 4 + 16) / 8,
            // Reserved
            0,
            0,
            // Lifetime
            0,
            0,
            0,
            30,
            // IP
            0,
            1,
            0,
            2,
            0,
            3,
            0,
            4,
            0,
            5,
            0,
            6,
            0,
            7,
            0,
            8
        ];
        if want.len() < got.len() {
            want.resize(got.len(), 0);
        }
        for (i, (got, want)) in Iterator::zip(got.iter(), want.iter()).enumerate() {
            let got = *got;
            let want = *want;
            println!("{:03}: {:x} {} {:x}", i, got, if got == want {
                "=="
            } else {
                "!="
            }, want);
        }
        assert_eq!(got, want);
    }

    #[test]
    fn test_modify_dhcp_ex1() {
        let got = modify(PAYLOAD_DHCP1, 30, Ipv6Addr::new(1, 2, 3, 4, 5, 6, 7, 8)).unwrap();
        let mut want = vec![
            // IPv6
            0x6b,
            0x80,
            0x00,
            0x00,
            // Length
            0x00,
            0x81,
            0x11,
            0x01,
            0xfe,
            0x80,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x40,
            0xff,
            0xfe,
            0x12,
            0x20,
            0x0a,
            0xfe,
            0x80,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0xb2,
            0x6e,
            0xbf,
            0xff,
            0xfe,
            0x39,
            0xbf,
            0x7b,
            // UDP
            0x02,
            0x23,
            0x02,
            0x22,
            // UDP length
            0x00,
            0x81,
            // Checksum
            0xe6,
            0xc9,
            // DHCPv6
            0x07,
            0x56,
            0x20,
            0xfd,
            0x00,
            0x02,
            0x00,
            0x0a,
            0x00,
            0x03,
            0x00,
            0x01,
            0x00,
            0x19,
            0xaa,
            0xbc,
            0xfa,
            0x1b,
            0x00,
            0x01,
            0x00,
            0x0e,
            0x00,
            0x02,
            0x00,
            0x00,
            0xab,
            0x11,
            0xfc,
            0x72,
            0xdb,
            0x43,
            0x6a,
            0xc5,
            0xca,
            0x1f,
            0x00,
            0x18,
            0x00,
            0x19,
            0x0a,
            0x66,
            0x6c,
            0x65,
            0x74,
            0x73,
            0x2d,
            0x65,
            0x61,
            0x73,
            0x74,
            0x02,
            0x6a,
            0x70,
            0x00,
            0x05,
            0x69,
            0x70,
            0x74,
            0x76,
            0x66,
            0x02,
            0x6a,
            0x70,
            0x00,
            0x00,
            0x1f,
            0x00,
            0x20,
            0x24,
            0x04,
            0x01,
            0xa8,
            0x11,
            0x02,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x0b,
            0x24,
            0x04,
            0x01,
            0xa8,
            0x11,
            0x02,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x0a,
            // DNS option
            0x00,
            0x17,
            // Length 16
            0,
            16,
            // IP
            0,
            1,
            0,
            2,
            0,
            3,
            0,
            4,
            0,
            5,
            0,
            6,
            0,
            7,
            0,
            8
        ];
        if want.len() < got.len() {
            want.resize(got.len(), 0);
        }
        for (i, (got, want)) in Iterator::zip(got.iter(), want.iter()).enumerate() {
            let got = *got;
            let want = *want;
            println!("{:03}: {:x} {} {:x}", i, got, if got == want {
                "=="
            } else {
                "!="
            }, want);
        }
        assert_eq!(got, want);
    }
}