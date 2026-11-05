//! The browser's stand-in for the `opus` crate, chosen by target in ZapFast's
//! manifest. libopus needs a C library that wasm32-unknown-unknown lacks, so
//! nothing starts and a voice message reports that it cannot play.

#[derive(Debug)]
pub struct Error;

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Opus is not available in this build")
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug)]
pub enum Channels {
    Mono,
    Stereo,
}

#[derive(Clone, Copy, Debug)]
pub enum Application {
    Voip,
}

#[derive(Clone, Copy, Debug)]
pub enum Bitrate {
    Bits(i32),
}

#[derive(Debug)]
pub struct Decoder(());

impl Decoder {
    pub fn new(_rate: u32, _channels: Channels) -> Result<Self> {
        Err(Error)
    }

    pub fn decode_float(
        &mut self,
        _input: &[u8],
        _output: &mut [f32],
        _fec: bool,
    ) -> Result<usize> {
        Err(Error)
    }
}

#[derive(Debug)]
pub struct Encoder(());

impl Encoder {
    pub fn new(_rate: u32, _channels: Channels, _application: Application) -> Result<Self> {
        Err(Error)
    }

    pub fn set_bitrate(&mut self, _bitrate: Bitrate) -> Result<()> {
        Err(Error)
    }

    pub fn get_lookahead(&mut self) -> Result<i32> {
        Err(Error)
    }

    pub fn encode_float(&mut self, _input: &[f32], _output: &mut [u8]) -> Result<usize> {
        Err(Error)
    }
}
