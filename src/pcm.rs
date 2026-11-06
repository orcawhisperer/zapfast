//! Small sound conversions that do not depend on where the sound comes from.

/// Brings a stream of samples at one rate to fixed frames of 16-bit samples at another,
/// as a call's engine wants them. Going down, each sample out is the average of those it
/// covers, which is enough for speech and keeps what is above the new rate from folding
/// back into what can be heard.
pub struct Framer {
    /// Samples in for each sample out.
    step: f64,
    /// How far through the sample out this is, in samples in.
    covered: f64,
    sum: f32,
    count: u32,
    frame: Vec<i16>,
    size: usize,
}

impl Framer {
    pub fn new(from: u32, to: u32, size: usize) -> Self {
        Self {
            step: f64::from(from.max(1)) / f64::from(to.max(1)),
            covered: 0.0,
            sum: 0.0,
            count: 0,
            frame: Vec::with_capacity(size),
            size,
        }
    }

    /// Takes in `samples`, and hands `emit` each frame they complete.
    pub fn push(&mut self, samples: &[f32], mut emit: impl FnMut(Vec<i16>)) {
        for &sample in samples {
            self.sum += sample;
            self.count += 1;
            self.covered += 1.0;
            while self.covered >= self.step {
                self.covered -= self.step;
                let mean = self.sum / self.count.max(1) as f32;
                self.sum = 0.0;
                self.count = 0;
                self.frame.push(to_i16(mean));
                if self.frame.len() == self.size {
                    emit(std::mem::replace(
                        &mut self.frame,
                        Vec::with_capacity(self.size),
                    ));
                }
            }
        }
    }
}

fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
}

/// Which packets have been played, by their RTP sequence numbers. A data channel with no
/// retransmissions or ordering hands packets over as they came, so some never come and a few come
/// behind others.
#[derive(Debug, Default)]
pub struct PacketOrder {
    next: Option<u16>,
}

impl PacketOrder {
    /// A jump this far either way is not the network: the stream has started again.
    const RESTART: i16 = 1_000;

    /// Whether a packet still has a place: `None` if that place has gone by, else how many packets
    /// were skipped to reach it.
    pub fn admit(&mut self, sequence: u16) -> Option<u16> {
        let Some(next) = self.next else {
            self.next = Some(sequence.wrapping_add(1));
            return Some(0);
        };
        let ahead = sequence.wrapping_sub(next) as i16;
        if (-Self::RESTART..0).contains(&ahead) {
            return None;
        }
        self.next = Some(sequence.wrapping_add(1));
        Some(if ahead > 0 && ahead < Self::RESTART {
            ahead as u16
        } else {
            0
        })
    }
}

/// What plays in place of a lost packet: the last sound backwards, fading out. Backwards it begins
/// from the very sample the sound broke off on, at the same pitch and level, so it neither clicks
/// nor leaves the hole a silence would. `last` becomes what was just made, for a second loss in a row.
pub fn conceal_into(last: &mut Vec<i16>, filled: &mut Vec<i16>) {
    let length = last.len().max(1) as f32;
    filled.clear();
    filled.extend(
        last.iter()
            .rev()
            .enumerate()
            .map(|(index, &sample)| (f32::from(sample) * (1.0 - index as f32 / length)) as i16),
    );
    last.clone_from(filled);
}

/// Brings in the first sound after a loss, which the concealment has faded out before.
pub fn fade_in(samples: &mut [i16]) {
    /// 3 ms at the rate of a call.
    const LENGTH: usize = 48;
    for (index, sample) in samples.iter_mut().take(LENGTH).enumerate() {
        *sample = (f32::from(*sample) * index as f32 / LENGTH as f32) as i16;
    }
}

/// A 16-bit mono WAV file.
pub fn wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_size = (samples.len() * 2) as u32;
    let mut file = Vec::with_capacity(44 + samples.len() * 2);
    file.extend_from_slice(b"RIFF");
    file.extend_from_slice(&(36 + data_size).to_le_bytes());
    file.extend_from_slice(b"WAVEfmt ");
    file.extend_from_slice(&16u32.to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&rate.to_le_bytes());
    file.extend_from_slice(&(rate * 2).to_le_bytes());
    file.extend_from_slice(&2u16.to_le_bytes());
    file.extend_from_slice(&16u16.to_le_bytes());
    file.extend_from_slice(b"data");
    file.extend_from_slice(&data_size.to_le_bytes());
    for &sample in samples {
        file.extend_from_slice(&to_i16(sample).to_le_bytes());
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(from: u32, to: u32, size: usize, blocks: &[Vec<f32>]) -> Vec<Vec<i16>> {
        let mut framer = Framer::new(from, to, size);
        let mut out = Vec::new();
        for block in blocks {
            framer.push(block, |frame| out.push(frame));
        }
        out
    }

    #[test]
    fn a_second_at_any_rate_makes_the_frames_a_call_expects() {
        for rate in [48_000, 44_100, 16_000] {
            let block = vec![0.25f32; rate as usize / 20];
            let out = frames(rate, 16_000, 960, &vec![block; 20]);
            assert_eq!(out.len(), 16, "{rate}: 16,000 samples is 16 frames of 960");
            assert!(out.iter().all(|frame| frame.len() == 960));
        }
    }

    #[test]
    fn blocks_of_any_size_come_out_the_same() {
        let tone: Vec<f32> = (0..48_000)
            .map(|index| (index as f32 * 0.05).sin() * 0.5)
            .collect();
        let whole = frames(48_000, 16_000, 960, std::slice::from_ref(&tone));
        let pieces: Vec<Vec<f32>> = tone.chunks(137).map(<[f32]>::to_vec).collect();
        assert_eq!(whole, frames(48_000, 16_000, 960, &pieces));
    }

    #[test]
    fn going_down_keeps_the_level_of_what_can_be_heard() {
        // 200 Hz stays at its level at 16 kHz, 7.9 kHz above the new limit nearly goes.
        let level = |hertz: f32| {
            let tone: Vec<f32> = (0..48_000)
                .map(|index| (index as f32 / 48_000.0 * hertz * std::f32::consts::TAU).sin() * 0.5)
                .collect();
            let out: Vec<i16> = frames(48_000, 16_000, 960, &[tone])
                .into_iter()
                .flatten()
                .collect();
            let peak = out
                .iter()
                .map(|sample| sample.unsigned_abs())
                .max()
                .unwrap();
            f32::from(peak) / f32::from(i16::MAX)
        };
        assert!((level(200.0) - 0.5).abs() < 0.02, "{}", level(200.0));
        assert!(level(15_900.0) < 0.1, "{}", level(15_900.0));
    }

    #[test]
    fn samples_past_full_scale_are_held_at_it() {
        let out = frames(16_000, 16_000, 2, &[vec![2.0, -2.0]]);
        assert_eq!(out, [vec![i16::MAX, -i16::MAX]]);
    }

    #[test]
    fn packets_in_order_all_play() {
        let mut order = PacketOrder::default();
        for sequence in [65_534, 65_535, 0, 1] {
            assert_eq!(order.admit(sequence), Some(0), "{sequence} follows on");
        }
    }

    #[test]
    fn a_skipped_packet_is_counted_and_one_behind_it_is_too_late() {
        let mut order = PacketOrder::default();
        assert_eq!(order.admit(10), Some(0));
        assert_eq!(order.admit(13), Some(2), "11 and 12 did not come");
        assert_eq!(order.admit(12), None, "13 has played");
        assert_eq!(order.admit(13), None, "and a copy of it is no use");
        assert_eq!(order.admit(14), Some(0));
    }

    #[test]
    fn a_stream_that_starts_again_is_not_a_long_loss() {
        let mut order = PacketOrder::default();
        assert_eq!(order.admit(40_000), Some(0));
        assert_eq!(order.admit(5), Some(0), "far behind is a new stream");
        assert_eq!(order.admit(6), Some(0));
        assert_eq!(order.admit(30_000), Some(0), "and so is far ahead");
    }

    #[test]
    fn concealment_starts_where_the_sound_broke_off_and_fades_out() {
        let mut last: Vec<i16> = (0..960).map(|index| 1_000 + index).collect();
        let broke_off = *last.last().unwrap();
        let mut filled = Vec::new();
        conceal_into(&mut last, &mut filled);
        assert_eq!(filled.len(), 960);
        assert_eq!(filled[0], broke_off, "so nothing clicks");
        assert!(filled[959].abs() < 5, "{}", filled[959]);
        assert!(filled.windows(2).all(|pair| pair[1] <= pair[0]));
        assert_eq!(last, filled, "a second loss carries on from this one");
    }

    #[test]
    fn concealing_nothing_makes_nothing() {
        let mut filled = vec![1];
        conceal_into(&mut Vec::new(), &mut filled);
        assert!(filled.is_empty());
    }

    #[test]
    fn the_first_sound_after_a_loss_comes_in_from_nothing() {
        let mut samples = vec![10_000_i16; 96];
        fade_in(&mut samples);
        assert_eq!(samples[0], 0);
        assert!(samples.windows(2).all(|pair| pair[1] >= pair[0]));
        assert_eq!(samples[48..], [10_000; 48]);
    }

    #[test]
    fn a_wav_says_how_long_it_is() {
        let file = wav(&[0.0, 0.5], 48_000);
        assert_eq!(&file[..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes([file[40], file[41], file[42], file[43]]),
            4
        );
        assert_eq!(file.len(), 48);
    }
}
