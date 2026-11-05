//! The browser's stand-in for `ureq`, chosen by target in ZapFast's manifest.
//! A blocking request cannot run in a page, so every call fails. The paths the
//! browser build needs go through `web::http` instead.

use std::time::Duration;

#[derive(Debug)]
pub enum Error {
    StatusCode(u16),
    Unavailable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StatusCode(code) => write!(f, "http status: {code}"),
            Self::Unavailable => f.write_str("blocking requests do not run in a browser"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Debug)]
pub struct Proxy;

impl Proxy {
    pub fn new(_url: &str) -> Result<Self, Error> {
        Err(Error::Unavailable)
    }

    pub fn try_from_env() -> Option<Self> {
        None
    }
}

pub mod tls {
    use super::Error;

    pub struct Certificate;

    impl Certificate {
        pub fn from_pem(_pem: &[u8]) -> Result<Self, Error> {
            Ok(Self)
        }
    }

    pub struct RootCerts;

    impl RootCerts {
        pub fn new_with_certs(_certs: &[Certificate]) -> Self {
            Self
        }
    }

    pub struct TlsConfig;

    impl TlsConfig {
        pub fn builder() -> TlsConfigBuilder {
            TlsConfigBuilder
        }
    }

    pub struct TlsConfigBuilder;

    impl TlsConfigBuilder {
        pub fn root_certs(self, _certs: RootCerts) -> Self {
            self
        }

        pub fn build(self) -> TlsConfig {
            TlsConfig
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Agent;

impl Agent {
    pub fn config_builder() -> ConfigBuilder {
        ConfigBuilder
    }

    pub fn get(&self, _url: &str) -> RequestBuilder {
        RequestBuilder
    }
}

pub struct ConfigBuilder;

pub struct Config;

impl ConfigBuilder {
    pub fn tls_config(self, _tls: tls::TlsConfig) -> Self {
        self
    }

    pub fn proxy(self, _proxy: Option<Proxy>) -> Self {
        self
    }

    pub fn timeout_global(self, _timeout: Option<Duration>) -> Self {
        self
    }

    pub fn timeout_connect(self, _timeout: Option<Duration>) -> Self {
        self
    }

    pub fn build(self) -> Config {
        Config
    }
}

impl From<Config> for Agent {
    fn from(_config: Config) -> Self {
        Self
    }
}

pub struct RequestBuilder;

impl RequestBuilder {
    pub fn config(self) -> Self {
        self
    }

    pub fn timeout_global(self, _timeout: Option<Duration>) -> Self {
        self
    }

    pub fn build(self) -> Self {
        self
    }

    pub fn call(self) -> Result<Response, Error> {
        Err(Error::Unavailable)
    }
}

/// Never made: no request ever answers.
pub struct Response(());

impl Response {
    pub fn body_mut(&mut self) -> Body {
        Body
    }
}

pub struct Body;

impl Body {
    pub fn read_to_vec(&mut self) -> Result<Vec<u8>, Error> {
        Err(Error::Unavailable)
    }

    pub fn read_to_string(&mut self) -> Result<String, Error> {
        Err(Error::Unavailable)
    }
}
