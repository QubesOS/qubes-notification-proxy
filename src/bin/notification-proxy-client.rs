use bincode::Options;
use futures_channel::oneshot::Sender;
use notification_emitter::{ImageParameters, ReplyMessage, MAX_MESSAGE_SIZE};
use notification_emitter::{Notification, Urgency};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use zbus::zvariant::{DeserializeDict, SerializeDict, Type, Value};

#[derive(Debug)]
struct ServerInner {
    out: tokio::io::Stdout,
    map: HashMap<u64, Sender<Result<u32, (String, Option<String>)>>>,
}

struct Server(Arc<Mutex<ServerInner>>, core::sync::atomic::AtomicU64);

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
    async fn get_capabilities(&self) -> zbus::fdo::Result<(Vec<String>,)> {
        Ok((vec!["persistence".to_owned(), "actions".to_owned()],))
    }
    #[dbus_interface(signal)]
    async fn notification_closed(
        &self,
        signal_context: &zbus::SignalContext<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
    #[dbus_interface(signal)]
    async fn action_invoked(
        &self,
        signal_context: &zbus::SignalContext<'_>,
        id: u32,
        action_key: String,
    ) -> zbus::Result<()>;
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
        let id = self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let notification = Notification {
            id,
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

        let len: u32 = data.len().try_into().unwrap();
        let mut guard = self.0.lock().await;
        guard
            .out
            .write_u32_le(len.to_le())
            .await
            .expect("error writing to stdout");
        guard
            .out
            .write_all(&*data)
            .await
            .expect("error writing to stdout");
        guard.out.flush().await.expect("Error writing to stdout");
        let (sender, receiver) = futures_channel::oneshot::channel();
        guard.map.insert(id, sender);
        drop(guard);
        eprintln!("Message sent to server");

        receiver
            .await
            .expect("sender crashed")
            .map_err(|(_a, b)| zbus::fdo::Error::Failed(b.unwrap_or("failed".to_owned())))
    }
}
async fn client_server() {
    let server = Arc::new(Mutex::new(ServerInner {
        out: tokio::io::stdout(),
        map: HashMap::new(),
    }));
    let connection = zbus::ConnectionBuilder::session()
        .expect("cannot create session bus")
        .name("org.freedesktop.Notifications")
        .expect("cannot acquire name")
        .serve_at(
            "/org/freedesktop/Notifications",
            Server(server.clone(), 0u64.into()),
        )
        .expect("cannot serve")
        .build()
        .await
        .expect("error");
    let interface_ref = connection
        .object_server()
        .interface::<_, Server>("/org/freedesktop/Notifications")
        .await
        .expect("something went wrong");
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
            ReplyMessage::Id { id, sequence } => server
                .lock()
                .await
                .map
                .remove(&sequence)
                .expect("server violated the protocol")
                .send(Ok(id))
                .expect("task died"),
            ReplyMessage::DBusError {
                name,
                message,
                sequence,
            } => server
                .lock()
                .await
                .map
                .remove(&sequence)
                .expect("server violated the protocol")
                .send(Err((name, message)))
                .expect("task died"),
            ReplyMessage::Dismissed { id, reason } => {
                let x = interface_ref.get().await;
                x.notification_closed(interface_ref.signal_context(), id, reason)
                    .await
                    .expect("cannot emit signal");
            }
            ReplyMessage::ActionInvoked { id, action } => {
                let x = interface_ref.get().await;
                x.action_invoked(interface_ref.signal_context(), id, action)
                    .await
                    .expect("cannot emit signal");
            }
            ReplyMessage::UnknownError { sequence: _ } => todo!(),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let local_set = tokio::task::LocalSet::new();

    local_set.spawn_local(client_server());
    Ok(local_set.await)
}
