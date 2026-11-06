//! How a browser's `RTCPeerConnection` reaches a WhatsApp call relay: the SDP answer that opens it,
//! and what goes out over it when it falls behind.
//!
//! A relay speaks no SDP: it is a UDP endpoint the server names for a call. But a browser only
//! opens a connection against an answer, so one is written here from what is known about the
//! relay. It is the far end of ICE-lite, so the browser checks and the relay answers, and it is
//! passive, so the browser is the DTLS client, as the desktop's transport is.
//!
//! The form of this answer, and the certificate's fingerprint, follow the browser client at
//! <https://github.com/oxidezap/client> (`crates/session/src/relay`), which is MIT licensed.

use std::net::SocketAddr;

use whatsapp_rust::wacore::voip::demux::{RelayPacketKind, classify_relay_packet};
use whatsapp_rust::wacore::voip::rtp::{RTP_PAYLOAD_TYPE_H264, is_whatsapp_opus_rtp_payload};

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

/// How much unsent media the channel may hold before more video is dropped. At a call's rate this
/// is a fraction of a second: past it the video is older than anything worth sending.
const VIDEO_CEILING: u32 = 64 * 1024;

/// How much it may hold before anything at all is dropped. A voice cannot be what filled it and is
/// what a call can least lose, so it goes on past the ceiling above. Past this one the channel is
/// not congested but stuck.
const HARD_CEILING: u32 = 8 * VIDEO_CEILING;

/// Where the video going out is between access units. The verdict is taken at a unit's first packet
/// and holds to its last: a keyframe is itself larger than the ceiling, and one with a hole in it
/// is worse than none, as is everything that refers to it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VideoUnit {
    Between,
    Sending,
    Dropping,
}

/// What became of the video unit a packet belongs to.
#[derive(Debug, PartialEq)]
pub struct Verdict {
    pub next: VideoUnit,
    /// This packet began a unit, and the whole of it is to be dropped.
    pub dropped_unit: bool,
}

impl VideoUnit {
    /// Whether a packet goes out with `buffered` bytes waiting, and where that leaves the video.
    pub fn admit(self, packet: &[u8], buffered: u32) -> (bool, Verdict) {
        let stays = Verdict {
            next: self,
            dropped_unit: false,
        };
        if buffered > HARD_CEILING {
            // A unit part sent stays as it is: the rest of it will not arrive to end it, but the
            // next one's verdict is the one that matters, and this channel is not coming back.
            return (false, stays);
        }
        if classify_relay_packet(packet) != RelayPacketKind::Rtp {
            return (true, stays);
        }
        let (marker, payload_type) = (packet[1] & 0x80 != 0, packet[1] & 0x7f);
        if is_whatsapp_opus_rtp_payload(payload_type) {
            return (true, stays);
        }
        if payload_type != RTP_PAYLOAD_TYPE_H264 {
            return (buffered <= VIDEO_CEILING, stays);
        }
        let now = match self {
            Self::Between if buffered <= VIDEO_CEILING => Self::Sending,
            Self::Between => Self::Dropping,
            going => going,
        };
        (
            now == Self::Sending,
            Verdict {
                next: if marker { Self::Between } else { now },
                dropped_unit: self == Self::Between && now == Self::Dropping,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An RTP header of a payload type, with its marker or without.
    fn rtp(payload_type: u8, marker: bool) -> Vec<u8> {
        let mut packet = vec![0x80, payload_type | if marker { 0x80 } else { 0 }];
        packet.resize(16, 0);
        packet
    }

    const OVER: u32 = VIDEO_CEILING + 1;
    const OPUS: u8 = whatsapp_rust::wacore::voip::rtp::RTP_PAYLOAD_TYPE_OPUS;

    #[test]
    fn a_full_channel_drops_all_of_a_video_frame_and_counts_it_once() {
        let unit = VideoUnit::Between;
        let (first, verdict) = unit.admit(&rtp(RTP_PAYLOAD_TYPE_H264, false), OVER);
        assert!(!first);
        assert!(verdict.dropped_unit);
        // The channel has drained by now, but the frame already has a hole in it.
        let (middle, next) = verdict.next.admit(&rtp(RTP_PAYLOAD_TYPE_H264, false), 0);
        assert!(!middle);
        assert!(!next.dropped_unit);
        let (last, next) = next.next.admit(&rtp(RTP_PAYLOAD_TYPE_H264, true), 0);
        assert!(!last);
        assert_eq!(next.next, VideoUnit::Between);
        // And the next frame goes out.
        assert!(next.next.admit(&rtp(RTP_PAYLOAD_TYPE_H264, true), 0).0);
    }

    #[test]
    fn a_frame_begun_is_finished_even_as_it_fills_the_channel() {
        let (first, verdict) = VideoUnit::Between.admit(&rtp(RTP_PAYLOAD_TYPE_H264, false), 0);
        assert!(first);
        let (middle, verdict) = verdict.next.admit(&rtp(RTP_PAYLOAD_TYPE_H264, false), OVER);
        assert!(middle, "the bytes before it are spent either way");
        assert!(!verdict.dropped_unit);
        let (last, verdict) = verdict.next.admit(&rtp(RTP_PAYLOAD_TYPE_H264, true), OVER);
        assert!(last);
        assert_eq!(verdict.next, VideoUnit::Between);
    }

    #[test]
    fn a_voice_goes_on_past_the_ceiling_that_stops_video() {
        assert!(VideoUnit::Between.admit(&rtp(OPUS, false), OVER).0);
        assert!(VideoUnit::Dropping.admit(&rtp(OPUS, false), OVER).0);
        assert!(
            !VideoUnit::Between
                .admit(&rtp(RTP_PAYLOAD_TYPE_H264, true), OVER)
                .0
        );
    }

    #[test]
    fn a_voice_between_a_frames_packets_leaves_the_frames_verdict_alone() {
        let (_, verdict) = VideoUnit::Sending.admit(&rtp(OPUS, true), 0);
        assert_eq!(verdict.next, VideoUnit::Sending);
    }

    #[test]
    fn nothing_goes_out_on_a_stuck_channel() {
        let stuck = HARD_CEILING + 1;
        assert!(!VideoUnit::Between.admit(&rtp(OPUS, false), stuck).0);
        assert!(
            !VideoUnit::Sending
                .admit(&rtp(RTP_PAYLOAD_TYPE_H264, false), stuck)
                .0
        );
        // STUN, which keeps the relay's allocation alive.
        assert!(!VideoUnit::Between.admit(&[0x00; 20], stuck).0);
    }

    #[test]
    fn stun_goes_out_while_video_is_held_back() {
        assert!(VideoUnit::Dropping.admit(&[0x00; 20], OVER).0);
    }

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
