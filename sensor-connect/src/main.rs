use esp32_nimble::{
    enums::*, utilities::BleUuid, uuid128, BLEDevice, BLEReturnCode, NimbleProperties,
};
use esp_idf_hal::task;
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsDefault};
use esp_idf_sys as _;
use futures::{channel::mpsc::channel, stream::select_all, Stream, StreamExt};
use log::{info, warn};
use random::Source;
use std::{pin::Pin, time::SystemTime};

const INITIAL_PASSKEY: u32 = 123456;
const RANDOM_BYTES: usize = 1;
const INITIAL_NAME: &str = "OpenSensor";
const NVS_NAMESPACE: &str = "sensor_connect";
const NVS_TAG_SHORT_NAME: &str = "short_name";
const NVS_TAG_PASSKEY: &str = "passkey";
const SERVICE_UUID: BleUuid = uuid128!("c5f93147-b051-4201-bb59-ff8f18db9876");
const PACKAGE_NAME_UUID: BleUuid = uuid128!("72e4028a-f727-4867-9ec4-25637a6eb834");
const VERSION_UUID: BleUuid = uuid128!("504fc887-3a39-4cd2-89f1-0fa6c9c55f22");
const HOMEPAGE_UUID: BleUuid = uuid128!("2f292fff-56e0-40b2-b8bd-cb1cc6937920");
const REPOSITORY_UUID: BleUuid = uuid128!("a2467465-8e29-436e-a0d4-6dd847193c89");
const AUTHORS_UUID: BleUuid = uuid128!("7ef914f3-9c94-45f9-ab77-26429fae3bc4");
const SHORT_NAME_UUID: BleUuid = uuid128!("ec67e1ac-cdd0-44bd-9c03-aebc64968b68");
const PASSKEY_UUID: BleUuid = uuid128!("f0650e70-58ff-4b69-ab99-5d61c6db7e75");

// 31 bytes for advertising, minus 2 for idk, minus 16 for service uuid
const SHORT_NAME_MAX_LENGTH: usize = 31 - 2 - 16;

fn main() {
    task::block_on(main_async());
}

async fn main_async() {
    // It is necessary to call this function once,
    // or else some patches to the runtime implemented by esp-idf-sys might not link properly.
    esp_idf_sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let nvs_default_partition = EspNvsPartition::<NvsDefault>::take().unwrap();
    let mut nvs = EspNvs::new(nvs_default_partition, NVS_NAMESPACE, true).unwrap();
    let name = {
        // Add 1 cuz it needs an extra character for \0 (which we will trim)
        let mut buf = [0u8; SHORT_NAME_MAX_LENGTH + 1];
        let stored_name = nvs.get_str(NVS_TAG_SHORT_NAME, &mut buf).unwrap();
        match stored_name {
            Some(stored_name) => stored_name.trim_end_matches(char::from(0)).to_owned(),
            None => {
                let seed = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64;
                let mut source = random::default(seed);
                let bytes =
                    hex::encode_upper(source.iter().take(RANDOM_BYTES).collect::<Vec<u8>>());
                let name = format!("{} {}", INITIAL_NAME, bytes);
                nvs.set_str(NVS_TAG_SHORT_NAME, name.as_str()).unwrap();
                name
            }
        }
    };
    let passkey = {
        match nvs.get_u32(NVS_TAG_PASSKEY).unwrap() {
            Some(stored_passkey) => stored_passkey,
            None => {
                nvs.set_u32(NVS_TAG_PASSKEY, INITIAL_PASSKEY).unwrap();
                INITIAL_PASSKEY
            }
        }
    };
    info!("Passkey is: {:0>6}", passkey);

    let device = BLEDevice::take();
    device
        .security()
        .set_auth(AuthReq::all())
        .set_passkey(passkey)
        .set_io_cap(SecurityIOCap::DisplayOnly);

    let server = device.get_server();
    let (mut advertise_tx, advertise_rx) = channel::<()>(0);
    server.on_connect(move |server, desc| {
        ::log::info!("Client connected: {:?}", desc);

        if server.connected_count() < (esp_idf_sys::CONFIG_BT_NIMBLE_MAX_CONNECTIONS as _) {
            ::log::info!("Multi-connect support: start advertising");
            advertise_tx.try_send(()).unwrap();
        }
    });
    server.on_disconnect(|_desc, reason| {
        ::log::info!("Client disconnected ({:?})", BLEReturnCode(reason as _));
    });

    let service = server.create_service(SERVICE_UUID);

    struct ConstCharacteristic {
        uuid: BleUuid,
        value: &'static str,
    }
    let const_characteristics = vec![
        ConstCharacteristic {
            uuid: PACKAGE_NAME_UUID,
            value: env!("CARGO_PKG_NAME"),
        },
        ConstCharacteristic {
            uuid: VERSION_UUID,
            value: env!("CARGO_PKG_VERSION"),
        },
        ConstCharacteristic {
            uuid: HOMEPAGE_UUID,
            value: env!("CARGO_PKG_HOMEPAGE"),
        },
        ConstCharacteristic {
            uuid: REPOSITORY_UUID,
            value: env!("CARGO_PKG_REPOSITORY"),
        },
        ConstCharacteristic {
            uuid: AUTHORS_UUID,
            value: env!("CARGO_PKG_AUTHORS"),
        },
    ];
    for const_characteristic in const_characteristics {
        service
            .lock()
            .create_characteristic(const_characteristic.uuid, NimbleProperties::READ)
            .lock()
            .set_value(const_characteristic.value.as_bytes());
    }

    let (mut short_name_tx, short_name_rx) = channel::<String>(0);
    let short_name_characteristic = service.lock().create_characteristic(
        SHORT_NAME_UUID,
        NimbleProperties::READ
            | NimbleProperties::WRITE
            | NimbleProperties::WRITE_ENC
            | NimbleProperties::WRITE_AUTHEN
            | NimbleProperties::NOTIFY,
    );
    short_name_characteristic
        .lock()
        .set_value(name.as_bytes())
        .on_write(
            move |args| match String::from_utf8(args.recv_data.to_vec()) {
                Ok(short_name) => {
                    if short_name.len() <= SHORT_NAME_MAX_LENGTH {
                        short_name_tx.try_send(short_name).unwrap()
                    } else {
                        args.reject();
                        warn!(
                            "New short name too long: {:#?}. Not changing short name.",
                            short_name
                        );
                    }
                }
                Err(e) => {
                    args.reject();
                    warn!("Invalid short_name. Error: {:#?}", e);
                }
            },
        );

    let (mut passkey_tx, passkey_rx) = channel::<u32>(0);
    let passkey_characteristic = service.lock().create_characteristic(
        PASSKEY_UUID,
        NimbleProperties::READ
            | NimbleProperties::READ_ENC
            | NimbleProperties::READ_AUTHEN
            | NimbleProperties::WRITE
            | NimbleProperties::WRITE_ENC
            | NimbleProperties::WRITE_AUTHEN
            | NimbleProperties::NOTIFY,
    );
    passkey_characteristic
        .lock()
        .set_value(&INITIAL_PASSKEY.to_be_bytes())
        .on_write(
            move |args| match <&[u8] as TryInto<[u8; 4]>>::try_into(args.recv_data) {
                Ok(new_passkey) => {
                    let new_passkey = u32::from_be_bytes(new_passkey);
                    passkey_tx.try_send(new_passkey).unwrap();
                }
                Err(e) => {
                    warn!(
                        "Pass key was not changed because it had an invalid length. Error: {:#?}",
                        e
                    );
                }
            },
        );

    let ble_advertising = device.get_advertising();
    ble_advertising
        .name(name.as_str())
        .add_service_uuid(SERVICE_UUID)
        .start()
        .unwrap();

    ::log::info!("bonded_addresses: {:?}", device.bonded_addresses().unwrap());

    enum Event {
        Advertise,
        ShortName(String),
        Passkey(u32),
    }

    let event_streams: Vec<Pin<Box<dyn Stream<Item = Event>>>> = vec![
        Box::pin(advertise_rx.map(|_| Event::Advertise)),
        Box::pin(short_name_rx.map(|short_name| Event::ShortName(short_name))),
        Box::pin(passkey_rx.map(|passkey| Event::Passkey(passkey))),
    ];
    let mut event_stream = select_all(event_streams);

    loop {
        let event = event_stream.next().await.unwrap();
        match event {
            Event::Advertise => {
                device.get_advertising().start().unwrap();
            }
            Event::ShortName(new_name) => {
                nvs.set_str(NVS_TAG_SHORT_NAME, &new_name).unwrap();
                ble_advertising.reset().unwrap();
                ble_advertising
                    .name(new_name.as_str())
                    .add_service_uuid(SERVICE_UUID)
                    .start()
                    .unwrap();
                short_name_characteristic.lock().notify();
            }
            Event::Passkey(passkey) => {
                device.security().set_passkey(passkey);
                nvs.set_u32(NVS_TAG_PASSKEY, passkey).unwrap();
                passkey_characteristic.lock().notify();
            }
        }
    }
}
