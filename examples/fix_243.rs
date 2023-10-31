use xcb::{x, xinput};

fn main() -> xcb::Result<()> {
    let (conn, screen_num) =
        xcb::Connection::connect_with_extensions(Some(":0"), &[xcb::Extension::Input], &[])
            .unwrap();

    println!(
        "active extensions: {:?}",
        conn.active_extensions().collect::<Vec::<_>>()
    );

    // Check if XI2 is supported
    conn.wait_for_reply(conn.send_request(&xinput::XiQueryVersion {
        major_version: 2,
        minor_version: 0,
    }))
    .expect("XI2 not supported");

    let cookie = conn.send_request(&xinput::ListInputDevices {});
    let device_list = conn.wait_for_reply(cookie)?;

    println!("{:?}", device_list);

    let device = {
        let mut device: Option<(xinput::Device, String)> = None;
        for (i, dev) in device_list.devices().iter().enumerate() {
            let name = device_list.names().nth(i).unwrap().name().to_utf8();
            if name.contains("xwayland-pointer-gestures") {
                device = Some((
                    xinput::Device::from_id(dev.device_id() as _),
                    name.to_string(),
                ));
                break;
            }
        }
        device.expect("could not find a pointer device")
    };

    println!("found pointer device \"{}\" ({:?})", device.1, device.0);

    let cookie = conn.send_request(&xinput::OpenDevice {
        device_id: device.0.id() as u8,
    });
    conn.wait_for_reply(cookie)?;

    let setup = conn.get_setup();
    let screen = setup.roots().nth(screen_num as usize).unwrap();
    let window: x::Window = conn.generate_id();

    conn.send_request(&x::CreateWindow {
        depth: x::COPY_FROM_PARENT as u8,
        wid: window,
        parent: screen.root(),
        x: 50,
        y: 50,
        width: 500,
        height: 500,
        border_width: 10,
        class: x::WindowClass::InputOutput,
        visual: screen.root_visual(),
        value_list: &[
            x::Cw::BackPixel(screen.white_pixel()),
            x::Cw::EventMask(x::EventMask::EXPOSURE | x::EventMask::KEY_PRESS),
        ],
    });

    conn.send_request(&x::MapWindow { window });

    let title = "Pointer Window";

    conn.send_request(&x::ChangeProperty {
        mode: x::PropMode::Replace,
        window,
        property: x::ATOM_WM_NAME,
        r#type: x::ATOM_STRING,
        data: title.as_bytes(),
    });

    let info = conn.wait_for_reply(conn.send_request(&xinput::XiQueryDevice { device: device.0 }));

    println!("{:#?}", info);

    conn.send_request(&xinput::XiSelectEvents {
        window,
        masks: &[xinput::EventMaskBuf::new(
            device.0,
            &[
                xinput::XiEventMask::MOTION
                | xinput::XiEventMask::BUTTON_PRESS
                | xinput::XiEventMask::BUTTON_PRESS
                | xinput::XiEventMask::RAW_MOTION
                | xinput::XiEventMask::RAW_BUTTON_PRESS
            ],
        )],
    });

    conn.flush()?;

    loop {
        let ev = conn.wait_for_event()?;
        match ev {
            xcb::Event::Input(xinput::Event::RawMotion(ev)) => {
                // TODO, query valuator infos to ensure on which axis is the pressure
                // This works for me with a Wacom One, but could be different with another device/config
                println!("received raw motion event");
                println!("{:?}", ev);
                println!();
            }
            xcb::Event::Input(xinput::Event::Motion(ev)) => {
                // TODO, query valuator infos to ensure on which axis is the pressure
                // This works for me with a Wacom One, but could be different with another device/config
                println!("received pointer motion event");
                println!("  event_x = {}", fp1616_to_f64(ev.event_x()));
                println!("  event_y = {}", fp1616_to_f64(ev.event_y()));
                println!("  pressure = {}", ev.axisvalues()[2].integral);
                println!();
            }
            xcb::Event::Input(xinput::Event::ButtonPress(_ev)) => {
                println!("received pointer press");
            }
            xcb::Event::Input(xinput::Event::ButtonRelease(_ev)) => {
                println!("received pointer release");
            }
            xcb::Event::X(x::Event::KeyPress(ev)) => {
                println!("received key press {:?}", ev);
                if ev.detail() == 24 {
                    break Ok(())
                }
            }
            ev => {
                println!("received other event {:?}", ev);
            }
        }
    }
}

fn fp1616_to_f64(val: xinput::Fp1616) -> f64 {
    (val as f64) / (u16::MAX as f64)
}
