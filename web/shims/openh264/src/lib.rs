//! The browser's stand-in for the `openh264` crate, chosen by target in
//! ZapFast's manifest. openh264's C++ needs a C library that
//! wasm32-unknown-unknown lacks, so every decoder and encoder refuses to start
//! and video falls back the way an unsupported format does on the desktop.

/// Why nothing here works.
#[derive(Debug)]
pub struct Error;

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("H.264 is not available in this build")
    }
}

impl std::error::Error for Error {}

pub struct OpenH264API;

impl OpenH264API {
    pub fn from_source() -> Self {
        Self
    }
}

pub mod formats {
    pub trait YUVSource {
        fn dimensions(&self) -> (usize, usize);
        fn strides(&self) -> (usize, usize, usize);
        fn y(&self) -> &[u8];
        fn u(&self) -> &[u8];
        fn v(&self) -> &[u8];
    }
}

pub mod decoder {
    use super::{Error, OpenH264API, formats::YUVSource};

    #[derive(Clone, Copy, Debug)]
    pub enum Flush {
        Auto,
        Flush,
        NoFlush,
    }

    #[derive(Default)]
    pub struct DecoderConfig;

    impl DecoderConfig {
        pub fn new() -> Self {
            Self
        }

        pub fn flush_after_decode(self, _flush: Flush) -> Self {
            self
        }
    }

    pub struct Decoder(());

    impl Decoder {
        pub fn new() -> Result<Self, Error> {
            Err(Error)
        }

        pub fn with_api_config(_api: OpenH264API, _config: DecoderConfig) -> Result<Self, Error> {
            Err(Error)
        }

        pub fn decode(&mut self, _packet: &[u8]) -> Result<Option<DecodedYUV<'_>>, Error> {
            Err(Error)
        }

        pub fn flush_remaining(&'_ mut self) -> Result<Vec<DecodedYUV<'_>>, Error> {
            Err(Error)
        }
    }

    /// Never made: no decoder ever starts.
    pub struct DecodedYUV<'a>(&'a [u8]);

    impl DecodedYUV<'_> {
        pub fn write_rgb8(&self, _target: &mut [u8]) {}

        pub fn write_rgba8(&self, _target: &mut [u8]) {}
    }

    impl YUVSource for DecodedYUV<'_> {
        fn dimensions(&self) -> (usize, usize) {
            (0, 0)
        }

        fn strides(&self) -> (usize, usize, usize) {
            (0, 0, 0)
        }

        fn y(&self) -> &[u8] {
            self.0
        }

        fn u(&self) -> &[u8] {
            self.0
        }

        fn v(&self) -> &[u8] {
            self.0
        }
    }
}

pub mod encoder {
    use super::{Error, OpenH264API, formats::YUVSource};

    pub struct BitRate;

    impl BitRate {
        pub fn from_bps(_bps: u32) -> Self {
            Self
        }
    }

    pub struct FrameRate;

    impl FrameRate {
        pub fn from_hz(_hz: f32) -> Self {
            Self
        }
    }

    pub struct IntraFramePeriod;

    impl IntraFramePeriod {
        pub fn from_num_frames(_frames: u32) -> Self {
            Self
        }
    }

    pub enum Profile {
        Baseline,
    }

    pub enum RateControlMode {
        Bitrate,
    }

    #[derive(Default)]
    pub struct EncoderConfig;

    impl EncoderConfig {
        pub fn new() -> Self {
            Self
        }

        pub fn profile(self, _profile: Profile) -> Self {
            self
        }

        pub fn bitrate(self, _bitrate: BitRate) -> Self {
            self
        }

        pub fn rate_control_mode(self, _mode: RateControlMode) -> Self {
            self
        }

        pub fn max_frame_rate(self, _rate: FrameRate) -> Self {
            self
        }

        pub fn intra_frame_period(self, _period: IntraFramePeriod) -> Self {
            self
        }

        pub fn skip_frames(self, _skip: bool) -> Self {
            self
        }
    }

    pub struct Encoder(());

    impl Encoder {
        pub fn with_api_config(_api: OpenH264API, _config: EncoderConfig) -> Result<Self, Error> {
            Err(Error)
        }

        pub fn force_intra_frame(&mut self) {}

        pub fn encode<T: YUVSource>(&mut self, _yuv: &T) -> Result<EncodedBitStream, Error> {
            Err(Error)
        }
    }

    /// Never made: no encoder ever starts.
    pub struct EncodedBitStream(());

    impl EncodedBitStream {
        pub fn write_vec(&self, _dst: &mut Vec<u8>) {}
    }
}
