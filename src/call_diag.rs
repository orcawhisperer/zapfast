//! What a video call's pictures did, in numbers, and, on request, the pictures themselves.
//!
//! The counters go in the call's log: how many frames went out and how many of them were
//! keyframes (the largest thing a call sends), and how many of the peer's frames decoded. They are
//! counts and sizes only.
//!
//! The dump is for working on a call's video, never on by default: with `ZAPFAST_CALL_VIDEO_DUMP`
//! set to a folder, each call writes the access units it sent and the ones it received there, as
//! raw H.264 that ffmpeg and other tools read (`ffmpeg -i received.h264 out.mp4`). That is the
//! picture of both people on the call, so only set it for a call whose other side agreed.

use std::borrow::Cow;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

/// The environment variable that names the folder a call's video is dumped to.
pub const DUMP_VARIABLE: &str = "ZAPFAST_CALL_VIDEO_DUMP";

/// The NAL unit types read here (H.264 table 7-1).
const IDR: u8 = 5;
const SPS: u8 = 7;
const PPS: u8 = 8;
const DELIMITER: u8 = 9;

/// The NAL units of an Annex-B access unit, each without its start code.
pub fn nal_units(access_unit: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = access_unit;
    std::iter::from_fn(move || {
        let start = start_code_end(rest)?;
        rest = &rest[start..];
        let end = next_start_code(rest).unwrap_or(rest.len());
        let (unit, tail) = rest.split_at(end);
        rest = tail;
        // A four-byte start code leaves a zero at the end of the unit before it.
        let unit = unit.strip_suffix(&[0]).unwrap_or(unit);
        Some(unit)
    })
}

/// Where the first start code in `data` ends.
fn start_code_end(data: &[u8]) -> Option<usize> {
    data.windows(3)
        .position(|window| window == [0, 0, 1])
        .map(|at| at + 3)
}

/// Where the next start code in `data` begins.
fn next_start_code(data: &[u8]) -> Option<usize> {
    data.windows(3).position(|window| window == [0, 0, 1])
}

/// Whether an access unit holds an IDR slice: a picture a decoder can start from.
pub fn has_idr(access_unit: &[u8]) -> bool {
    nal_units(access_unit).any(|unit| unit.first().is_some_and(|header| header & 0x1f == 5))
}

/// What a sequence parameter set says of the stream: its profile, its constraint flags and its
/// level (H.264 7.3.2.1.1), the three bytes after the NAL header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamProfile {
    pub profile: u8,
    pub constraints: u8,
    pub level: u8,
}

impl StreamProfile {
    /// The profile's common name, as far as these bytes tell it.
    pub fn name(self) -> &'static str {
        match self.profile {
            66 if self.constraints & 0x40 != 0 => "Constrained Baseline",
            66 => "Baseline",
            77 => "Main",
            88 => "Extended",
            100 => "High",
            110 => "High 10",
            122 => "High 4:2:2",
            244 => "High 4:4:4",
            _ => "other",
        }
    }
}

/// The profile of the first sequence parameter set in an access unit, if it carries one.
pub fn stream_profile(access_unit: &[u8]) -> Option<StreamProfile> {
    nal_units(access_unit)
        .find(|unit| unit.first().is_some_and(|header| header & 0x1f == 7))
        .and_then(|sps| match sps {
            [_, profile, constraints, level, ..] => Some(StreamProfile {
                profile: *profile,
                constraints: *constraints,
                level: *level,
            }),
            _ => None,
        })
}

/// A stream's parameter sets, kept for a decoder that takes whole pictures. A peer may send them in
/// an access unit of their own, ahead of the picture they belong to, which the library marks as a
/// keyframe. openh264 takes that, as it remembers them, but a browser's decoder is handed units it
/// can start from and closes on one without a picture, then again on the picture without its sets.
#[derive(Debug, Default)]
pub struct ParameterSets {
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl ParameterSets {
    /// Notes the sets in an access unit and gives back what a decoder should have of it: `None`
    /// when it has no picture, and an IDR picture with the sets it came without in front of it, so
    /// a decoder can start from any of them, however long ago the sets went by.
    pub fn complete<'a>(&mut self, access_unit: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        let (mut sps, mut pps) = (Vec::new(), Vec::new());
        let (mut picture, mut idr) = (false, false);
        for unit in nal_units(access_unit) {
            match unit.first().map(|header| header & 0x1f) {
                Some(SPS) => push_unit(&mut sps, unit),
                Some(PPS) => push_unit(&mut pps, unit),
                Some(IDR) => (picture, idr) = (true, true),
                Some(1..=4) => picture = true,
                _ => {}
            }
        }
        let (came_with_sps, came_with_pps) = (!sps.is_empty(), !pps.is_empty());
        if came_with_sps {
            self.sps = sps;
        }
        if came_with_pps {
            self.pps = pps;
        }
        if !picture {
            return None;
        }
        if !idr || (came_with_sps && came_with_pps) {
            return Some(Cow::Borrowed(access_unit));
        }
        // After any delimiter, which has to stay first.
        let mut whole = Vec::with_capacity(access_unit.len() + self.sps.len() + self.pps.len());
        let mut placed = false;
        for unit in nal_units(access_unit) {
            if !placed
                && unit
                    .first()
                    .is_some_and(|header| header & 0x1f != DELIMITER)
            {
                placed = true;
                if !came_with_sps {
                    whole.extend_from_slice(&self.sps);
                }
                if !came_with_pps {
                    whole.extend_from_slice(&self.pps);
                }
            }
            push_unit(&mut whole, unit);
        }
        Some(Cow::Owned(whole))
    }

    /// What the newest sequence parameter set says of the stream.
    pub fn profile(&self) -> Option<StreamProfile> {
        stream_profile(&self.sps)
    }
}

/// Adds a NAL unit to an Annex-B buffer.
fn push_unit(to: &mut Vec<u8>, unit: &[u8]) {
    to.extend_from_slice(&[0, 0, 0, 1]);
    to.extend_from_slice(unit);
}

/// Counts of one call's pictures, both ways, read and reset by the call's heartbeat.
#[derive(Debug, Default)]
pub struct VideoCounters {
    sent_frames: AtomicU32,
    sent_keyframes: AtomicU32,
    largest_keyframe: AtomicU32,
    decoded: AtomicU32,
    undecodable: AtomicU32,
}

/// What [`VideoCounters::take`] read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoCounts {
    pub sent_frames: u32,
    pub sent_keyframes: u32,
    /// The largest keyframe sent, in bytes.
    pub largest_keyframe: u32,
    /// The peer's frames that decoded, and the ones the decoder refused.
    pub decoded: u32,
    pub undecodable: u32,
}

impl VideoCounters {
    /// One access unit went to the engine.
    pub fn sent(&self, access_unit: &[u8]) {
        self.sent_frames.fetch_add(1, Ordering::Relaxed);
        if has_idr(access_unit) {
            self.sent_keyframes.fetch_add(1, Ordering::Relaxed);
            let bytes = u32::try_from(access_unit.len()).unwrap_or(u32::MAX);
            self.largest_keyframe.fetch_max(bytes, Ordering::Relaxed);
        }
    }

    pub fn decoded(&self) {
        self.decoded.fetch_add(1, Ordering::Relaxed);
    }

    pub fn undecodable(&self) {
        self.undecodable.fetch_add(1, Ordering::Relaxed);
    }

    /// The counts since the last time this was asked.
    pub fn take(&self) -> VideoCounts {
        VideoCounts {
            sent_frames: self.sent_frames.swap(0, Ordering::Relaxed),
            sent_keyframes: self.sent_keyframes.swap(0, Ordering::Relaxed),
            largest_keyframe: self.largest_keyframe.swap(0, Ordering::Relaxed),
            decoded: self.decoded.swap(0, Ordering::Relaxed),
            undecodable: self.undecodable.swap(0, Ordering::Relaxed),
        }
    }
}

/// Everything one call keeps about its pictures: the counts, and the dumps of both directions.
pub struct CallVideo {
    pub counters: VideoCounters,
    pub sent: Dump,
    pub received: Dump,
}

impl CallVideo {
    pub fn new(call_id: &str) -> Self {
        Self {
            counters: VideoCounters::default(),
            sent: Dump::open(call_id, "sent"),
            received: Dump::open(call_id, "received"),
        }
    }
}

/// One direction of a call's video, written as raw H.264, when [`DUMP_VARIABLE`] asks for it.
pub struct Dump {
    file: Mutex<Option<std::io::BufWriter<crate::platform::sfs::File>>>,
}

impl Dump {
    /// A dump of one direction (`sent` or `received`) of the call `call_id`, when the variable
    /// names a folder; a dump that writes nothing otherwise.
    pub fn open(call_id: &str, direction: &str) -> Self {
        let file = std::env::var_os(DUMP_VARIABLE)
            .filter(|folder| !folder.is_empty())
            .and_then(|folder| Self::create(Path::new(&folder), call_id, direction));
        Self {
            file: Mutex::new(file),
        }
    }

    fn create(
        folder: &Path,
        call_id: &str,
        direction: &str,
    ) -> Option<std::io::BufWriter<crate::platform::sfs::File>> {
        let started = crate::platform::SystemTime::now()
            .duration_since(crate::platform::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let path = dump_path(folder, started, call_id, direction);
        let created = crate::platform::sfs::create_dir_all(folder)
            .and_then(|()| crate::platform::sfs::File::create(&path));
        match created {
            Ok(file) => {
                log::info!("[CALL] writing the {direction} video to {}", path.display());
                Some(std::io::BufWriter::new(file))
            }
            Err(error) => {
                log::warn!("[CALL] the {direction} video cannot be dumped: {error}");
                None
            }
        }
    }

    /// Appends one access unit. A failed write stops the dump rather than the call.
    pub fn write(&self, access_unit: &[u8]) {
        let mut file = self.file.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(writer) = file.as_mut()
            && let Err(error) = writer.write_all(access_unit)
        {
            log::warn!("[CALL] the video dump stopped: {error}");
            *file = None;
        }
    }
}

/// The file one direction of a call's video is dumped to: named by when the video started and by
/// the call (an outgoing call has no id yet when its camera starts), so two calls never write to
/// one file, and by the direction.
fn dump_path(folder: &Path, started: u64, call_id: &str, direction: &str) -> PathBuf {
    let call: String = call_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(32)
        .collect();
    let name = if call.is_empty() {
        format!("{started}-{direction}.h264")
    } else {
        format!("{started}-{call}-{direction}.h264")
    };
    folder.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An access unit as the encoder writes one: an AUD, an SPS, a PPS and an IDR slice, with both
    /// lengths of start code.
    fn keyframe() -> Vec<u8> {
        let mut unit = vec![0, 0, 0, 1, 0x09, 0xf0];
        unit.extend([0, 0, 0, 1, 0x67, 66, 0xc0, 31, 0xaa]);
        unit.extend([0, 0, 1, 0x68, 0xce]);
        unit.extend([0, 0, 1, 0x65, 0x88, 0x84]);
        unit
    }

    #[test]
    fn an_access_unit_splits_into_its_nal_units() {
        let unit = keyframe();
        let headers: Vec<u8> = nal_units(&unit).map(|nal| nal[0] & 0x1f).collect();
        assert_eq!(headers, [9, 7, 8, 5]);
        assert_eq!(
            nal_units(&unit).nth(1),
            Some(&[0x67, 66, 0xc0, 31, 0xaa][..])
        );
    }

    #[test]
    fn a_keyframe_is_told_from_a_delta_frame() {
        assert!(has_idr(&keyframe()));
        assert!(!has_idr(&[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x41, 0x9a]));
    }

    #[test]
    fn the_stream_profile_is_read_from_its_parameter_set() {
        let profile = stream_profile(&keyframe()).expect("an SPS");
        assert_eq!(
            profile,
            StreamProfile {
                profile: 66,
                constraints: 0xc0,
                level: 31
            }
        );
        assert_eq!(profile.name(), "Constrained Baseline");
        assert_eq!(stream_profile(&[0, 0, 1, 0x41, 0x9a]), None);
    }

    /// The types of an access unit's NAL units.
    fn types(unit: &[u8]) -> Vec<u8> {
        nal_units(unit).map(|nal| nal[0] & 0x1f).collect()
    }

    const SETTINGS_ALONE: &[u8] = &[0, 0, 0, 1, 0x67, 66, 0xc0, 31, 0xaa, 0, 0, 1, 0x68, 0xce];
    const BARE_IDR: &[u8] = &[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x65, 0x88];
    const DELTA: &[u8] = &[0, 0, 1, 0x41, 0x9a];

    #[test]
    fn settings_alone_wait_for_the_picture_they_belong_to() {
        let mut sets = ParameterSets::default();
        assert_eq!(sets.complete(SETTINGS_ALONE), None, "nothing to decode yet");
        let whole = sets.complete(BARE_IDR).expect("a picture");
        assert_eq!(types(&whole), [9, 7, 8, 5], "the delimiter stays first");
        assert_eq!(stream_profile(&whole).map(|seen| seen.level), Some(31));
    }

    #[test]
    fn a_decoder_can_restart_from_a_later_idr_without_settings() {
        let mut sets = ParameterSets::default();
        sets.complete(SETTINGS_ALONE);
        sets.complete(BARE_IDR);
        sets.complete(DELTA);
        assert_eq!(types(&sets.complete(BARE_IDR).unwrap()), [9, 7, 8, 5]);
        assert_eq!(sets.profile().map(|seen| seen.profile), Some(66));
    }

    #[test]
    fn a_keyframe_with_its_settings_and_a_delta_frame_pass_untouched() {
        let mut sets = ParameterSets::default();
        let keyframe = keyframe();
        assert!(matches!(sets.complete(&keyframe), Some(Cow::Borrowed(_))));
        assert!(matches!(sets.complete(DELTA), Some(Cow::Borrowed(_))));
    }

    #[test]
    fn settings_ahead_of_a_delta_frame_stay_and_do_not_make_it_a_keyframe() {
        let mut sets = ParameterSets::default();
        let mut unit = SETTINGS_ALONE.to_vec();
        unit.extend_from_slice(DELTA);
        let passed = sets.complete(&unit).expect("a picture");
        assert_eq!(&*passed, &unit[..]);
        assert!(!has_idr(&passed));
        assert_eq!(types(&sets.complete(BARE_IDR).unwrap()), [9, 7, 8, 5]);
    }

    #[test]
    fn newer_settings_replace_older_ones_and_only_the_missing_are_added() {
        let mut sets = ParameterSets::default();
        sets.complete(SETTINGS_ALONE);
        // A new SPS alongside the picture: the PPS is the one thing it lacks.
        let unit = [&[0, 0, 0, 1, 0x67, 100, 0, 40][..], &[0, 0, 1, 0x65, 0x88]].concat();
        let whole = sets.complete(&unit).expect("a picture");
        assert_eq!(types(&whole), [8, 7, 5]);
        assert_eq!(sets.profile().map(|seen| seen.profile), Some(100));
    }

    #[test]
    fn an_idr_with_no_settings_ever_seen_is_left_for_the_decoder_to_refuse() {
        let mut sets = ParameterSets::default();
        assert_eq!(types(&sets.complete(BARE_IDR).unwrap()), [9, 5]);
        assert_eq!(sets.profile(), None);
    }

    #[test]
    fn counters_count_keyframes_and_reset_when_read() {
        let counters = VideoCounters::default();
        counters.sent(&keyframe());
        counters.sent(&[0, 0, 1, 0x41, 0x9a]);
        counters.decoded();
        counters.undecodable();
        let counts = counters.take();
        assert_eq!(counts.sent_frames, 2);
        assert_eq!(counts.sent_keyframes, 1);
        assert_eq!(counts.largest_keyframe, keyframe().len() as u32);
        assert_eq!((counts.decoded, counts.undecodable), (1, 1));
        assert_eq!(counters.take(), VideoCounts::default());
    }

    #[test]
    fn a_dump_file_is_named_by_the_call_and_the_direction() {
        let path = dump_path(
            Path::new("/dumps"),
            1_760_000_000,
            "00CA6C61/../x",
            "received",
        );
        assert_eq!(path, Path::new("/dumps/1760000000-00CA6C61x-received.h264"));
        let path = dump_path(Path::new("/dumps"), 1_760_000_000, "", "sent");
        assert_eq!(path, Path::new("/dumps/1760000000-sent.h264"));
    }

    #[test]
    fn a_dump_writes_what_it_is_given() {
        let folder = tempfile::tempdir().expect("a folder");
        let file = Dump::create(folder.path(), "call", "sent").expect("created");
        let dump = Dump {
            file: Mutex::new(Some(file)),
        };
        dump.write(&keyframe());
        drop(dump);
        let written = crate::platform::sfs::read_dir(folder.path())
            .expect("listed")
            .flatten()
            .map(|entry| crate::platform::sfs::read(entry.path()).expect("read"))
            .next()
            .expect("written");
        assert_eq!(written, keyframe());
    }
}
