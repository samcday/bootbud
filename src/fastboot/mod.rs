mod protocol;
pub mod webusb;

use futures::AsyncRead;
use std::{collections::HashMap, fmt::Display, io::Write};
use thiserror::Error;
use tracing::{info, warn};
use tracing::{instrument, trace};

use protocol::FastBootResponse;
use protocol::{FastBootCommand, FastBootResponseParseError};

/// Fastboot communication errors
#[derive(Debug, Error)]
pub enum FastBootError {
    #[error("General error: {0}")]
    Transfer(#[from] Box<dyn std::error::Error + Send + Sync>),
    #[error("Fastboot client failure: {0}")]
    FastbootFailed(String),
    #[error("Unexpected fastboot response")]
    FastbootUnexpectedReply,
    #[error("Unknown fastboot response: {0}")]
    FastbootParseError(#[from] FastBootResponseParseError),
}

/// Errors when opening the fastboot device
#[derive(Debug, Error)]
pub enum FastBootOpenError {
    #[error("Failed to open device: {0}")]
    Device(std::io::Error),
    #[error("Failed to claim interface: {0}")]
    Interface(std::io::Error),
    #[error("Failed to find interface for fastboot")]
    MissingInterface,
    #[error("Failed to find required endpoints for fastboot")]
    MissingEndpoints,
    #[error("Unknown fastboot response: {0}")]
    FastbootParseError(#[from] FastBootResponseParseError),
}

/// Fastboot client
pub struct Fastboot<Ops> {
    ops: Ops,
    buf: Vec<u8>,
}

pub trait FastBootOps {
    async fn write_out(&mut self, buf: &mut [u8]) -> Result<usize, FastBootError>;
    async fn write_out_stream<R: AsyncRead + Unpin>(
        &mut self,
        read: R,
    ) -> Result<usize, FastBootError>;
    async fn read_in(&mut self, buf: &mut [u8]) -> Result<usize, FastBootError>;
}

impl<Ops: FastBootOps> Fastboot<Ops> {
    pub fn new(ops: Ops) -> Self {
        Self {
            ops,
            buf: Vec::with_capacity(64),
        }
    }

    async fn send_command<S: Display>(
        &mut self,
        cmd: FastBootCommand<S>,
    ) -> Result<(), FastBootError> {
        self.buf.clear();
        self.buf.write_fmt(format_args!("{}", cmd)).unwrap();
        trace!(
            "Sending command: {}",
            std::str::from_utf8(&self.buf).unwrap_or("Invalid utf-8")
        );

        self.ops.write_out(&mut self.buf).await?;
        Ok(())
    }

    #[tracing::instrument(skip_all, err)]
    async fn read_response(&mut self) -> Result<FastBootResponse, FastBootError> {
        self.buf.resize(64, 0);
        let num = self.ops.read_in(&mut self.buf).await?;
        FastBootResponse::from_bytes(&self.buf[..num]).map_err(FastBootError::FastbootParseError)
    }

    #[tracing::instrument(skip_all, err)]
    async fn handle_responses(&mut self) -> Result<String, FastBootError> {
        loop {
            let resp = self.read_response().await?;
            trace!("Response: {:?}", resp);
            match resp {
                FastBootResponse::Info(_) => (),
                FastBootResponse::Data(_) => return Err(FastBootError::FastbootUnexpectedReply),
                FastBootResponse::Okay(value) => return Ok(value),
                FastBootResponse::Fail(fail) => return Err(FastBootError::FastbootFailed(fail)),
            }
        }
    }

    #[tracing::instrument(skip_all, err)]
    async fn execute<S: Display>(
        &mut self,
        cmd: FastBootCommand<S>,
    ) -> Result<String, FastBootError> {
        self.send_command(cmd).await?;
        self.handle_responses().await
    }

    /// Get the named variable
    ///
    /// The "all" variable is special; For that [Self::get_all_vars] should be used instead
    pub async fn get_var(&mut self, var: &str) -> Result<String, FastBootError> {
        let cmd = FastBootCommand::GetVar(var);
        self.execute(cmd).await
    }

    /// Prepare a download of a given size
    pub async fn download(&mut self, size: u32) -> Result<Option<String>, FastBootError> {
        let cmd = FastBootCommand::<&str>::Download(size);
        let mut info: Option<String> = None;
        self.send_command(cmd).await?;
        loop {
            let resp = self.read_response().await?;
            match resp {
                FastBootResponse::Info(i) => {
                    if let Some(s) = info {
                        info = Some(s + &i)
                    } else {
                        info = Some(i);
                    }
                }
                FastBootResponse::Data(_) => {
                    return Ok(info);
                }
                FastBootResponse::Okay(_) => return Err(FastBootError::FastbootUnexpectedReply),
                FastBootResponse::Fail(fail) => return Err(FastBootError::FastbootFailed(fail)),
            }
        }
    }

    pub async fn do_download<R: AsyncRead + Unpin>(
        &mut self,
        reader: R,
    ) -> Result<String, FastBootError> {
        let written = self.ops.write_out_stream(reader).await?;
        tracing::debug!("Wrote {} bytes", written);
        self.handle_responses().await
    }

    /// Flash downloaded data to a given target partition
    pub async fn flash(&mut self, target: &str) -> Result<(), FastBootError> {
        let cmd = FastBootCommand::Flash(target);
        self.execute(cmd).await.map(|v| {
            trace!("Flash ok: {v}");
        })
    }

    /// Erasing the given target partition
    pub async fn erase(&mut self, target: &str) -> Result<(), FastBootError> {
        let cmd = FastBootCommand::Erase(target);
        self.execute(cmd).await.map(|v| {
            trace!("Erase ok: {v}");
        })
    }

    pub async fn boot(&mut self) -> Result<(), FastBootError> {
        let cmd = FastBootCommand::<&str>::Boot;
        self.execute(cmd).await.map(|v| {
            trace!("Boot ok: {v}");
        })
    }

    /// Reboot the device
    pub async fn reboot(&mut self) -> Result<(), FastBootError> {
        let cmd = FastBootCommand::<&str>::Reboot;
        self.execute(cmd).await.map(|v| {
            trace!("Reboot ok: {v}");
        })
    }

    /// Reboot the device to the bootloader
    pub async fn reboot_bootloader(&mut self) -> Result<(), FastBootError> {
        let cmd = FastBootCommand::<&str>::RebootBootloader;
        self.execute(cmd).await.map(|v| {
            trace!("Reboot ok: {v}");
        })
    }

    /// Retrieve all variables
    pub async fn get_all_vars(&mut self) -> Result<HashMap<String, String>, FastBootError> {
        let cmd = FastBootCommand::GetVar("all");
        self.send_command(cmd).await?;
        let mut vars = HashMap::new();
        loop {
            let resp = self.read_response().await?;
            trace!("Response: {:?}", resp);
            match resp {
                FastBootResponse::Info(i) => {
                    let Some((key, value)) = i.rsplit_once(':') else {
                        warn!("Failed to parse variable: {i}");
                        continue;
                    };
                    vars.insert(key.trim().to_string(), value.trim().to_string());
                }
                FastBootResponse::Data(_) => return Err(FastBootError::FastbootUnexpectedReply),
                FastBootResponse::Okay(_) => {
                    return Ok(vars);
                }
                FastBootResponse::Fail(fail) => return Err(FastBootError::FastbootFailed(fail)),
            }
        }
    }
}

/// Error during data download
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("Trying to complete while nothing was Queued")]
    NothingQueued,
    #[error("Incorrect data length: expected {expected}, got {actual}")]
    IncorrectDataLength { actual: u32, expected: u32 },
    #[error(transparent)]
    Nusb(#[from] FastBootError),
}
