use js_sys::{Array, Object, Reflect};
use wasm_bindgen_futures::JsFuture;
use web_sys::UsbDeviceRequestOptions;

pub async fn start() {
    let usb = web_sys::window().unwrap().navigator().usb();

    let filter = Object::new();
    let filters = Array::of1(&filter);
    Reflect::set(&filter, &"vendorId".into(), &0xDEAD.into()).unwrap();
    Reflect::set(&filter, &"productId".into(), &0xBEEF.into()).unwrap();

    let usb_device = JsFuture::from(usb.request_device(&UsbDeviceRequestOptions::new(&filters))).await.unwrap();
    // console::log_2(&"USB device connected".into(), &usb_device);
}
