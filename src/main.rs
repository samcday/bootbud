use crate::fastboot::webusb::{find_fastboot_interface, FastbootWebUsb};
use crate::fastboot::{FastBootError, FastBootOps, Fastboot};
use anyhow::anyhow;
use dioxus::logger::tracing;
use dioxus::prelude::*;
use futures::{AsyncRead, AsyncReadExt, StreamExt};
use js_sys::Uint8Array;
use std::collections::VecDeque;
use std::str::FromStr;
use thiserror::Error;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::js_sys::Array;
use web_sys::{
    console, DomException, Response, UsbConfiguration, UsbDevice, UsbDeviceFilter,
    UsbDeviceRequestOptions, UsbDirection, UsbEndpoint, UsbEndpointType, UsbInTransferResult,
    UsbInterface, UsbOutTransferResult,
};

mod fastboot;

static U_BOOT: Asset = asset!("/assets/u-boot.img");

static AVAILABLE_DEVICES: GlobalSignal<Vec<UsbDevice>> = Signal::global(|| vec![]);
static DEVICE: GlobalSignal<Option<UsbDevice>> = Signal::global(|| None);

fn main() {
    launch(App);
}

#[derive(Error, Debug)]
enum AppError {
    #[error(transparent)]
    JsError(#[from] gloo_utils::errors::JsError),
}

fn js_error(err: JsValue) -> AppError {
    AppError::JsError(err.try_into().unwrap())
}

enum DeviceAction {
    BootDevice(UsbDevice),
}

#[component]
fn App() -> Element {
    let mut pair_error = use_signal(|| "".to_string());
    let start_pairing = move |_| async move {
        let window = web_sys::window().unwrap();
        let usb = window.navigator().usb();

        pair_error.set(String::new());

        // OP6(T)
        let filter = UsbDeviceFilter::new();
        filter.set_vendor_id(0x18d1);
        filter.set_product_id(0xd00d);

        let filters = Array::of1(&filter);
        if let Err(err) =
            JsFuture::from(usb.request_device(&UsbDeviceRequestOptions::new(&filters))).await
        {
            if let Some(err) = err.dyn_ref::<DomException>() {
                pair_error.set(err.message());
            }
        }
    };

    let window = web_sys::window().unwrap();
    let usb = window.navigator().usb();
    let on_connect = Closure::<dyn FnMut(_)>::new(move |event: web_sys::UsbConnectionEvent| {
        if find_fastboot_interface(&event.device()).is_some() {
            if !AVAILABLE_DEVICES.read().contains(&event.device()) {
                AVAILABLE_DEVICES.write().push(event.device());
            }
        }
    });
    usb.add_event_listener_with_callback("connect", on_connect.as_ref().unchecked_ref())
        .unwrap();
    on_connect.forget();

    let on_disconnect = Closure::<dyn FnMut(_)>::new(move |event: web_sys::UsbConnectionEvent| {
        let dev = event.device();
        AVAILABLE_DEVICES.write().retain(|v| !v.eq(&dev));
    });
    usb.add_event_listener_with_callback("disconnect", on_disconnect.as_ref().unchecked_ref())
        .unwrap();
    on_disconnect.forget();

    let dev_svc: Coroutine<DeviceAction> = use_coroutine(move |mut rx| async move {
        let window = web_sys::window().unwrap();
        let usb = window.navigator().usb();

        let devices: Array = JsFuture::from(usb.get_devices())
            .await
            .unwrap()
            .unchecked_into();
        for dev in devices {
            let dev = dev.unchecked_into::<UsbDevice>();
            if let Some(_) = find_fastboot_interface(&dev) {
                if !AVAILABLE_DEVICES.read().contains(&dev) {
                    AVAILABLE_DEVICES.write().push(dev);
                }
            }
        }

        // Wait for command to boot a device.
        while let Some(msg) = rx.next().await {
            match msg {
                DeviceAction::BootDevice(dev) => {
                    *DEVICE.write() = Some(dev.clone());
                    break;
                }
            }
        }

        let path = U_BOOT.resolve();

        let mut fastboot =
            Fastboot::new(FastbootWebUsb::new(DEVICE.read().clone().unwrap()).await.unwrap());
        let resp = JsFuture::from(window.fetch_with_str(path.to_str().unwrap())).await;
        let resp = resp.unwrap().unchecked_into::<Response>();

        let size = resp.headers().get("content-length").unwrap().unwrap();
        let size = u32::from_str(&size).unwrap();

        let read = wasm_streams::ReadableStream::from_raw(resp.body().unwrap());

        let info = fastboot.download(size).await.unwrap();
        tracing::debug!("Start download success: {:?}", info);
        let info = fastboot.do_download(read.into_async_read()).await.unwrap();
        tracing::debug!("Download success: {:?}", info);

        fastboot.boot().await.unwrap();
    });

    rsx! {
        if let Some(dev) = DEVICE.read().clone() {
            "Doing boot things to {dev.serial_number().unwrap()}"
        } else {
            p { "Let's boot a device" }
            if !AVAILABLE_DEVICES.read().is_empty() {
                ul {
                    for dev in AVAILABLE_DEVICES.read().iter().cloned() {
                        li {
                            "{dev.product_name().unwrap_or_default()}",
                            if let Some(serial) = dev.serial_number() {
                                " ({dev.serial_number().unwrap()})"
                            }
                            " "
                            button {
                                onclick: move |_| dev_svc.send(DeviceAction::BootDevice(dev.clone())),
                                "Boot"
                            }
                        }
                    }
                }
            },
            button {
                onclick: start_pairing,
                "Pair Device"
            }
            {pair_error}
        }
    }
}
