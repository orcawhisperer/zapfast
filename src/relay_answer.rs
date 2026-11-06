//! The SDP answer that lets a browser's `RTCPeerConnection` reach a WhatsApp call relay.
//!
//! A relay speaks no SDP: it is a UDP endpoint the server names for a call. But a browser only
//! opens a connection against an answer, so one is written here from what is known about the
//! relay. It is the far end of ICE-lite, so the browser checks and the relay answers, and it is
//! passive, so the browser is the DTLS client, as the desktop's transport is.
//!
//! The form of this answer, and the certificate's fingerprint, follow the browser client at
//! <https://github.com/oxidezap/client> (`crates/session/src/relay`), which is MIT licensed.

use std::net::SocketAddr;

/// The certificate relays present. A browser insists on the far end's fingerprint, which the
/// `<relay>` block does not carry. Seen to be the same across separate calls that reached different
/// relays, so it is the relays' own and not one per call. If it ever changes, calls in a browser
/// stop connecting while every other client carries on.
const RELAY_FINGERPRINT: &str = "F9:CA:0C:98:A3:CC:71:D6:42:CE:5A:E2:53:D2:15:20:\
D3:1B:BA:D8:57:A4:F0:AF:BE:0B:FB:F3:6B:0C:A0:68";

/// The SCTP port over DTLS, fixed by WebRTC rather than by the relay.
const SCTP_PORT: u16 = 5000;

/// The `a=mid` the offer used, which an answer has to repeat.
fn offer_mid(offer: &str) -> &str {
    offer
        .lines()
        .find_map(|line| line.strip_prefix("a=mid:"))
        .map_or("0", str::trim)
}

/// The answer to `offer` describing the relay at `relay`, whose ICE credentials are `ufrag` and
/// `pwd`. The relay is reached by its IPv4 address, as an IPv6 one cannot be written here.
pub fn answer(offer: &str, relay: SocketAddr, ufrag: &str, pwd: &str) -> Result<String, String> {
    let SocketAddr::V4(relay) = relay else {
        return Err("the relay's address is IPv6".to_owned());
    };
    let (ip, port) = (relay.ip(), relay.port());
    let mid = offer_mid(offer);
    Ok(format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {ip}\r\n\
         s=-\r\n\
         t=0 0\r\n\
         a=group:BUNDLE {mid}\r\n\
         m=application {port} UDP/DTLS/SCTP webrtc-datachannel\r\n\
         c=IN IP4 {ip}\r\n\
         a=mid:{mid}\r\n\
         a=ice-lite\r\n\
         a=ice-ufrag:{ufrag}\r\n\
         a=ice-pwd:{pwd}\r\n\
         a=fingerprint:sha-256 {RELAY_FINGERPRINT}\r\n\
         a=setup:passive\r\n\
         a=sctp-port:{SCTP_PORT}\r\n\
         a=max-message-size:262144\r\n\
         a=candidate:1 1 udp 2130706431 {ip} {port} typ host\r\n\
         a=end-of-candidates\r\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> SocketAddr {
        "203.0.113.7:3480".parse().unwrap()
    }

    #[test]
    fn the_fingerprint_is_thirty_two_pairs_of_capital_hex() {
        // The line break in its literal is easy to get wrong, and a browser's refusal reads as a
        // handshake that did not happen rather than as a bad attribute.
        let pairs: Vec<&str> = RELAY_FINGERPRINT.split(':').collect();
        assert_eq!(pairs.len(), 32);
        assert!(pairs.iter().all(|pair| {
            pair.len() == 2
                && pair
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c))
        }));
    }

    #[test]
    fn the_answer_describes_the_relay_and_leaves_the_client_role_to_the_browser() {
        let answer = answer("v=0\r\na=mid:0\r\n", relay(), "UFRAG", "PWD").unwrap();
        for line in [
            "c=IN IP4 203.0.113.7",
            "m=application 3480 UDP/DTLS/SCTP webrtc-datachannel",
            "a=ice-lite",
            "a=ice-ufrag:UFRAG",
            "a=ice-pwd:PWD",
            "a=setup:passive",
            "a=sctp-port:5000",
            "a=candidate:1 1 udp 2130706431 203.0.113.7 3480 typ host",
            "a=end-of-candidates",
        ] {
            assert!(answer.split("\r\n").any(|got| got == line), "{line}");
        }
        assert!(answer.ends_with("\r\n"));
    }

    #[test]
    fn the_answer_repeats_the_offers_mid() {
        let answer = answer("v=0\r\na=mid:data7\r\n", relay(), "u", "p").unwrap();
        assert!(answer.contains("a=mid:data7\r\n"));
        assert!(answer.contains("a=group:BUNDLE data7\r\n"));
    }

    #[test]
    fn an_offer_without_a_mid_still_gets_one() {
        assert!(
            answer("v=0\r\n", relay(), "u", "p")
                .unwrap()
                .contains("a=mid:0\r\n")
        );
    }

    #[test]
    fn an_ipv6_relay_is_refused_by_name() {
        let relay = "[2001:db8::1]:3480".parse().unwrap();
        assert!(answer("", relay, "u", "p").unwrap_err().contains("IPv6"));
    }
}
