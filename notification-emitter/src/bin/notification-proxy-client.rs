use bincode::Options;
use notification_emitter::{ImageParameters, ReplyMessage, MAX_MESSAGE_SIZE};
use notification_emitter::{Notification, NotificationEmitter, Urgency};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use zbus::zvariant::{DeserializeDict, SerializeDict, Type, Value};

struct Server(Arc<Mutex<tokio::io::Stdout>>);

#[derive(SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
struct Hints {
    #[zvariant(rename = "action-icons")]
    action_icons: Option<bool>,
    category: Option<String>,
    #[zvariant(rename = "desktop-entry")]
    desktop_entry: Option<String>,
    #[zvariant(rename = "image-data")]
    image_data: Option<ImageParameters>,
    #[zvariant(rename = "image_data")]
    image_data_deprecated1: Option<ImageParameters>,
}

#[zbus::dbus_interface(name = "org.freedesktop.Notifications")]
impl Server {
    async fn get_server_information(&self) -> zbus::fdo::Result<(String, String, String, String)> {
        Ok((
            "Qubes OS Notification Proxy".to_owned(),
            "Qubes OS".to_owned(),
            "0.0.1".to_owned(),
            "1.2".to_owned(),
        ))
    }
    async fn notify(
        &self,
        // Ignored.  We pass an empty string.
        _app_name: &str,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::fdo::Result<u32> {
        let options = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_native_endian()
            .reject_trailing_bytes();
        let mut image: Option<ImageParameters> = None;
        let mut suppress_sound = false;
        let mut transient = false;
        let mut urgency = None;
        let mut category = None;
        for (i, j) in hints.into_iter() {
            match &*i {
                "action-icons" => {}
                "category" => {
                    category = Some(
                        j.try_into()
                            .map_err(|f: zbus::zvariant::Error| zbus::fdo::Error::ZBus(f.into()))?,
                    )
                }
                // There is no way to trust this.  Ignore it.
                "desktop-entry" => {}
                // Deprecated, not yet implemented
                "image_data" | "icon_data" => {}
                // Also deprecated, and also NYI
                "image_path" => {}
                // This requires processing FreeDesktop icon themes.
                // This is also needed for SNI so it needs to be
                // implemented.
                "image-path" => eprintln!("Not yet implemented: Image paths"),
                "image-data" => {
                    let (
                        untrusted_width,
                        untrusted_height,
                        untrusted_rowstride,
                        untrusted_has_alpha,
                        untrusted_bits_per_sample,
                        untrusted_channels,
                        untrusted_data,
                    ) = j
                        .try_into()
                        .map_err(|f: zbus::zvariant::Error| zbus::fdo::Error::ZBus(f.into()))?;
                    image = Some(ImageParameters {
                        untrusted_width,
                        untrusted_height,
                        untrusted_rowstride,
                        untrusted_has_alpha,
                        untrusted_bits_per_sample,
                        untrusted_channels,
                        untrusted_data,
                    })
                }
                "sound-file" => {
                    eprintln!("Not yet implemented: Sound files (got {:?})", j)
                }
                "sound-name" => eprintln!(
                    "Not yet implemented: Sound files specified by name (got {:?})",
                    j
                ),
                "suppress-sound" => suppress_sound = true,
                "transient" => transient = true,
                "x" | "y" => eprintln!("Ignoring coordinate hint {} {:?}", i, j),
                "urgency" => match j {
                    Value::U8(0) => urgency = Some(Urgency::Low),
                    Value::U8(1) => urgency = Some(Urgency::Normal),
                    Value::U8(2) => urgency = Some(Urgency::Critical),
                    _ => eprintln!("Ignoring unknown urgency value {:?}", j),
                },
                _ => {
                    eprintln!("Unknown hint {:?}, ignoring", &*i);
                }
            }
        }
        let notification = Notification {
            suppress_sound,
            transient,
            urgency,
            replaces_id,
            summary,
            body,
            actions,
            category,
            expire_timeout,
            image,
        };

        let data = options
            .serialize(&notification)
            .expect("Cannot serialize object?");

        let len = data.len().try_into().unwrap();
        let mut guard = self.0.lock().await;
        guard
            .write_u32_le(len.to_le())
            .await
            .expect("error writing to stdout");
        guard
            .write_all(&*data)
            .await
            .expect("error writing to stdout");
        return Ok(0);
    }
}
async fn client_server() {
    let _connection = zbus::ConnectionBuilder::session()
        .expect("cannot create session bus")
        .name("org.freedesktop.Notifications")
        .expect("cannot acquire name")
        .serve_at(
            "/org/freedesktop/Notifications",
            Server(Arc::new(Mutex::new(tokio::io::stdout()))),
        )
        .expect("cannot serve")
        .build()
        .await
        .expect("error");
    let mut stdin = tokio::io::stdin();
    loop {
        let size = stdin
            .read_u32_le()
            .await
            .expect("Error reading from stdin")
            .to_le();
        if size > MAX_MESSAGE_SIZE {
            panic!("Message too large ({} bytes)", size)
        }

        let mut bytes = vec![0; size as _];
        let bytes_read = stdin
            .read_exact(&mut bytes[..])
            .await
            .expect("error reading from stdin");
        assert_eq!(bytes_read, size as _);
        eprintln!("{} bytes read!", bytes_read);

        let options = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_native_endian()
            .reject_trailing_bytes();
        match options
            .deserialize(&bytes)
            .expect("malformed input from client")
        {
            ReplyMessage::Id { id } => todo!(),
            ReplyMessage::DBusError { name, message } => todo!(),
            ReplyMessage::Dismissed { id } => todo!(),
            ReplyMessage::ActionInvoked { id } => todo!(),
            ReplyMessage::UnknownError => todo!(),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let local_set = tokio::task::LocalSet::new();

    local_set.spawn_local(client_server());
    Ok(local_set.await)
}
