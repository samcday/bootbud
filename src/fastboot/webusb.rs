use crate::fastboot::{FastBootError, FastBootOps};
use crate::js_error;
use anyhow::anyhow;
use futures::{AsyncRead, AsyncReadExt};
use js_sys::Uint8Array;
use std::collections::VecDeque;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    UsbConfiguration, UsbDevice, UsbDirection, UsbEndpoint, UsbEndpointType, UsbInTransferResult,
    UsbInterface, UsbOutTransferResult,
};

pub struct FastbootWebUsb {
    dev: UsbDevice,
    input_ep: u8,
    output_ep: u8,
    output_size: usize,
}

pub fn find_fastboot_interface(device: &UsbDevice) -> Option<(UsbConfiguration, UsbInterface)> {
    for config in device.configurations() {
        let config = config.unchecked_into::<UsbConfiguration>();
        for iface in config.interfaces() {
            let iface = iface.unchecked_into::<UsbInterface>();
            let alternate = iface.alternate();
            if alternate.interface_class() == 0xFF
                && alternate.interface_subclass() == 0x42
                && alternate.interface_protocol() == 0x3
            {
                return Some((config, iface));
            }
        }
    }

    None
}

impl FastbootWebUsb {
    pub async fn new(dev: UsbDevice) -> anyhow::Result<Self> {
        let (config, iface) =
            find_fastboot_interface(&dev).ok_or(anyhow!("No fastboot interface found"))?;

        JsFuture::from(dev.open()).await.map_err(js_error)?;
        let iface_num = iface.interface_number();
        JsFuture::from(dev.claim_interface(iface_num))
            .await
            .map_err(js_error)?;
        JsFuture::from(dev.select_configuration(config.configuration_value()))
            .await
            .map_err(js_error)?;

        let mut in_ep = None;
        let mut out_ep = None;

        let alternate = iface.alternate();
        for ep in alternate.endpoints() {
            let ep = UsbEndpoint::unchecked_from_js(ep);
            if let UsbEndpointType::Bulk = ep.type_() {
                match ep.direction() {
                    UsbDirection::In => in_ep = Some(ep),
                    UsbDirection::Out => out_ep = Some(ep),
                    _ => {}
                }
            }
        }

        if in_ep.is_none() || out_ep.is_none() {
            return Err(anyhow!("Fastboot interface lacking endpoints"));
        }

        let out_ep = out_ep.unwrap();
        let in_ep = in_ep.unwrap();

        Ok(Self {
            input_ep: in_ep.endpoint_number(),
            output_ep: out_ep.endpoint_number(),
            output_size: out_ep.packet_size() as _,
            dev,
        })
    }
}

impl FastBootOps for FastbootWebUsb {
    async fn write_out(&mut self, buf: &mut [u8]) -> Result<usize, FastBootError> {
        JsFuture::from(
            self.dev
                .transfer_out_with_u8_slice(self.output_ep, buf)
                .map_err(|err| {
                    let err: gloo::utils::errors::JsError = err.try_into().unwrap();
                    FastBootError::Transfer(err.into())
                })?,
        )
        .await
        .map_err(|err| {
            let err: gloo::utils::errors::JsError = err.try_into().unwrap();
            FastBootError::Transfer(err.into())
        })
        .map(|res| {
            let res: UsbOutTransferResult = res.unchecked_into();
            res.bytes_written() as usize
        })
    }

    async fn write_out_stream<R: AsyncRead + Unpin>(
        &mut self,
        mut read: R,
    ) -> Result<usize, FastBootError> {
        let mut buf = vec![];
        let mut total = 0;
        let mut queued: VecDeque<JsFuture> = VecDeque::new();
        buf.resize(self.output_size, 0);

        loop {
            let sz = read
                .read(&mut buf)
                .await
                .map_err(|err| FastBootError::Transfer(err.into()))?;
            if sz == 0 {
                break;
            }

            if queued.len() > 3 {
                total += queued
                    .pop_back()
                    .unwrap()
                    .await
                    .map_err(|err| {
                        let err: gloo::utils::errors::JsError = err.try_into().unwrap();
                        FastBootError::Transfer(err.into())
                    })
                    .map(|res| {
                        let res: UsbOutTransferResult = res.unchecked_into();
                        res.bytes_written() as usize
                    })?;
            }

            queued.push_front(JsFuture::from(
                self.dev
                    .transfer_out_with_u8_slice(self.output_ep, &mut buf[..sz])
                    .map_err(|err| {
                        let err: gloo::utils::errors::JsError = err.try_into().unwrap();
                        FastBootError::Transfer(err.into())
                    })?,
            ));
        }

        while !queued.is_empty() {
            total += queued
                .pop_back()
                .unwrap()
                .await
                .map_err(|err| {
                    let err: gloo::utils::errors::JsError = err.try_into().unwrap();
                    FastBootError::Transfer(err.into())
                })
                .map(|res| {
                    let res: UsbOutTransferResult = res.unchecked_into();
                    res.bytes_written() as usize
                })?;
        }

        Ok(total)
    }

    async fn read_in(&mut self, buf: &mut [u8]) -> Result<usize, FastBootError> {
        JsFuture::from(self.dev.transfer_in(self.input_ep, buf.len() as _))
            .await
            .map_err(|err| {
                let err: gloo::utils::errors::JsError = err.try_into().unwrap();
                FastBootError::Transfer(err.into())
            })
            .map(|res| {
                let res = UsbInTransferResult::unchecked_from_js(res);
                // TODO: res.status
                let data = res.data();
                if data.is_none() {
                    return 0;
                }
                let data = data.unwrap();
                let u8arr = Uint8Array::new(&data.buffer());
                u8arr.copy_to(&mut buf[0..data.byte_length()]);
                data.byte_length()
            })
    }
}
