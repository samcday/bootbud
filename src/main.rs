use crate::fastboot::webusb::{find_fastboot_interface, FastbootWebUsb};
use crate::fastboot::{FastBootError, FastBootOps, Fastboot};
use anyhow::anyhow;
use dioxus::logger::tracing;
use dioxus::prelude::*;
use futures::{AsyncRead, AsyncReadExt, StreamExt};
use gloo::events::EventListener;
use gloo::timers::future::TimeoutFuture;
use js_sys::{Date, Uint8Array};
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::time::Instant;
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

fn main() {
    launch(App);
}

enum DeviceMode {
    VendorFastboot,
    UBoot,
    LiveBooted,
}

async fn detect_device_mode(device: &UsbDevice) -> anyhow::Result<DeviceMode> {
    let mut fastboot = Fastboot::new(FastbootWebUsb::new(device.clone()).await?);
    if fastboot
        .get_var("version-bootloader")
        .await
        .is_ok_and(|v| v.contains("U-Boot"))
    {
        return Ok(DeviceMode::UBoot);
    }

    if fastboot
        .get_var("partition-type:op2")
        .await
        .is_ok_and(|v| v == "raw")
    {
        return Ok(DeviceMode::VendorFastboot);
    }

    Err(anyhow!("unknown"))
}

#[derive(Error, Debug)]
enum AppError {
    #[error(transparent)]
    JsError(#[from] gloo::utils::errors::JsError),
}

fn js_error(err: JsValue) -> AppError {
    AppError::JsError(err.try_into().unwrap())
}

/// Searches for a paired device that reports given serial.
/// If nothing is immediately found, wait until something connects that matches.
async fn device_by_serial(serial: &str) -> anyhow::Result<UsbDevice> {
    let window = web_sys::window().unwrap();
    let usb = window.navigator().usb();

    let devices: Array = JsFuture::from(usb.get_devices())
        .await
        .map_err(js_error)?
        .unchecked_into();
    for dev in devices {
        let dev: UsbDevice = dev.unchecked_into();
        if dev.serial_number().is_some_and(|v| v == serial) {
            return Ok(dev);
        }
    }

    let (tx, rx) = futures::channel::oneshot::channel();

    let serial = serial.to_owned();
    let mut tx = Some(tx);
    let listener = EventListener::new(&usb, "connect", move |event| {
        let event = event.unchecked_ref::<web_sys::UsbConnectionEvent>();
        let dev = event.device();
        if dev.serial_number().is_some_and(|v| v == serial) && tx.is_some() {
            tx.take().unwrap().send(dev).unwrap();
        }
    });

    let dev = rx.await?;
    drop(listener);
    Ok(dev)
}

async fn wait_disconnect(device: &UsbDevice) -> anyhow::Result<()> {
    let window = web_sys::window().unwrap();
    let usb = window.navigator().usb();
    let (tx, rx) = futures::channel::oneshot::channel();

    let mut found = false;
    let devices: Array = JsFuture::from(usb.get_devices())
        .await
        .map_err(js_error)?
        .unchecked_into();
    for dev in devices {
        let dev: UsbDevice = dev.unchecked_into();
        if dev.eq(device) {
            found = true;
            break;
        }
    }
    if !found {
        return Ok(());
    }

    let device = device.clone();
    let mut tx = Some(tx);
    let listener = EventListener::new(&usb, "disconnect", move |event| {
        let event = event.unchecked_ref::<web_sys::UsbConnectionEvent>();
        if event.device().eq(&device) && tx.is_some() {
            tx.take().unwrap().send(()).unwrap();
        }
    });

    rx.await?;
    drop(listener);
    Ok(())
}

async fn boot_uboot(device: UsbDevice) -> anyhow::Result<()> {
    let window = web_sys::window().unwrap();
    let path = U_BOOT.resolve();

    let mut fastboot = Fastboot::new(FastbootWebUsb::new(device).await?);

    let resp = JsFuture::from(window.fetch_with_str(path.to_str().unwrap())).await;
    let resp = resp.map_err(js_error)?.unchecked_into::<Response>();
    let size = resp
        .headers()
        .get("content-length")
        .map_err(js_error)?
        .ok_or(anyhow!("content-length missing"))?;
    let size = u32::from_str(&size)?;
    let read = wasm_streams::ReadableStream::from_raw(resp.body().ok_or(anyhow!("no body"))?);

    let info = fastboot.download(size).await?;
    tracing::debug!("Start download success: {:?}", info);
    let info = fastboot.do_download(read.into_async_read()).await?;
    tracing::debug!("Download success: {:?}", info);

    Ok(fastboot.boot().await?)
}

/// Handles booting a device all the way to kernel, passing through vendor fastboot and U-Boot
/// as needed.
async fn boot(serial: String) -> anyhow::Result<()> {
    loop {
        let device = device_by_serial(&serial).await?;

        match detect_device_mode(&device).await? {
            DeviceMode::VendorFastboot => {
                boot_uboot(device.clone()).await?;
                wait_disconnect(&device).await?;
            }
            DeviceMode::UBoot => {
                tracing::info!("made it!");
                return Ok(());
            }
            DeviceMode::LiveBooted => {
                return Ok(());
            }
        }
    }

    Ok(())
}

#[component]
fn App() -> Element {
    let mut available_devices = use_signal(|| HashMap::new());
    let mut active_device = use_signal(|| None);
    let mut boot_task = use_signal(|| None);

    // Setup WebUSB - add handlers for device connect/disconnection events and populate
    // available devices state.
    use_resource(move || async move {
        tracing::info!("doing thing");
        let mut add_device = move |device: UsbDevice| {
            let serial = device.serial_number();
            if serial.is_none() {
                tracing::warn!(
                    "Ignoring {}:{} as it has no serial number",
                    device.vendor_id(),
                    device.product_id()
                );
            }
            let serial = serial.unwrap();
            if find_fastboot_interface(&device).is_some() {
                available_devices.write().insert(serial.clone(), device);
            }
        };

        let window = web_sys::window().unwrap();
        let usb = window.navigator().usb();
        let on_connect = Closure::<dyn FnMut(_)>::new(move |event: web_sys::UsbConnectionEvent| {
            add_device(event.device());
        });
        usb.add_event_listener_with_callback("connect", on_connect.as_ref().unchecked_ref())
            .unwrap();
        on_connect.forget();

        let on_disconnect =
            Closure::<dyn FnMut(_)>::new(move |event: web_sys::UsbConnectionEvent| {
                let dev = event.device();
                available_devices.write().retain(|_, v| !v.loose_eq(&dev));
            });
        usb.add_event_listener_with_callback("disconnect", on_disconnect.as_ref().unchecked_ref())
            .unwrap();
        on_disconnect.forget();

        let devices: Array = JsFuture::from(usb.get_devices())
            .await
            .unwrap()
            .unchecked_into();
        for dev in devices {
            add_device(dev.unchecked_into::<UsbDevice>());
        }
    });

    rsx! {
        if let Some(serial) = active_device.read().as_ref() {
            Device { serial: serial }
        } else {
            SelectDevice {
                available_devices: available_devices(),
                on_select: move |serial: String| {
                    // dev_svc.send(DeviceAction::BootDevice(serial)),
                    *active_device.write() = Some(serial.clone());
                    *boot_task.write() = Some(spawn(async move {
                        if let Err(err) = boot(serial).await {
                            tracing::error!("Sad {}", err);
                        }
                    }));
                }
            },
        }
    }
}

#[component]
fn SelectDevice(
    available_devices: HashMap<String, UsbDevice>,
    on_select: EventHandler<String>,
) -> Element {
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

    rsx! {
        p { "Let's boot a device" }
        ul {
            {available_devices.iter().map(|(serial, dev)| {
                to_owned![serial];
                rsx! {
                    li {
                        "{dev.product_name().unwrap_or_default()} ({serial})"
                        " "
                        button {
                            onclick: move |_| on_select.call(serial.clone()),
                            "Boot"
                        }
                    }
                }
            })}
        },
        button {
            onclick: start_pairing,
            "Pair Device"
        }
        {pair_error}
    }
}

#[component]
fn Device(serial: String) -> Element {
    rsx! {
        "Doing boot things to {serial}"
    }
}
