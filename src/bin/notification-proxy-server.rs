use bincode::Options;
use futures_util::StreamExt;
use notification_emitter::{merge_versions, NotificationEmitter};
use notification_emitter::{
    qube_icon, MessageWriter, ReplyMessage, MAJOR_VERSION, MAX_MESSAGE_SIZE, MINOR_VERSION,
};
use std::rc::Rc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

async fn client_server(qube_name: String) {
    let default_icon = match qube_icon(qube_name.to_owned()) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("Failed to get qube {qube_name} icon: {e}");
            "".to_string()
        }
    };
    let (emitter, mut server_name_owner_changed) = NotificationEmitter::new(
        qube_name.to_owned() + ": ",
        "Qube: ".to_owned() + &*qube_name,
        default_icon,
    )
    .await
    .expect("Cannot connect to notifcation daemon");
    let (closed_stream, invoked_stream) =
        futures_util::future::join(emitter.closed(), emitter.invocations()).await;
    let emitter = Rc::new(emitter);
    let options = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_native_endian()
        .reject_trailing_bytes();
    let mut stdin = tokio::io::stdin();
    {
        let mut out = tokio::io::stdout();
        out.write_u32_le(merge_versions(MAJOR_VERSION, MINOR_VERSION).to_le())
            .await
            .expect("Cannot write version for version negotiation");
        out.flush().await.expect("flush failed");
    }
    let reply_version: u32 = stdin
        .read_u32_le()
        .await
        .expect("Cannot read reply")
        .to_le();
    let (reply_major, reply_minor) = notification_emitter::split_version(reply_version);
    if reply_major != MAJOR_VERSION ||  reply_minor > MINOR_VERSION {
        panic!(
            "Version mismatch: client supports version {reply_major}.{reply_minor} \
but this server only supports version {MAJOR_VERSION}.{MINOR_VERSION}"
        );
    }
    let stdout = MessageWriter::new();
    let emitter_ = emitter.clone();
    let mut closed_stream = closed_stream.expect("Cannot register for closed signals");
    let mut invoked_stream = invoked_stream.expect("Cannot register for invoked signals");
    let stdout_ = stdout.clone();
    let owner_changed_handle = tokio::task::spawn_local(async move {
        while let Some(item) = server_name_owner_changed.next().await {
            let item = item
                .args()
                .expect("Got invalid NameOwnerChanged message from bus daemon");
            assert_eq!(
                item.name, "org.freedesktop.Notifications",
                "Bus daemon sent message for name we didn't register for"
            );
            emitter_.clear()
        }
    });
    let emitter_ = emitter.clone();
    let closed_stream_handle = tokio::task::spawn_local(async move {
        while let Some(item) = closed_stream.next().await {
            let item = match item.args() {
                Ok(item) => item,
                Err(e) => {
                    eprintln!("Got invalid message from notification daemon: {}", e);
                    continue;
                }
            };
            let id = match emitter_.remove_host_id(item.id) {
                None => continue,
                Some(id) => id,
            };
            let data = options
                .serialize(&ReplyMessage::Dismissed {
                    id,
                    reason: item.reason,
                })
                .expect("Serialization failed?");
            stdout_.transmit(&*data).await
        }
    });
    let stdout_ = stdout.clone();
    let emitter_ = emitter.clone();
    let invoked_stream_handle = tokio::task::spawn_local(async move {
        while let Some(item) = invoked_stream.next().await {
            let item = match item.args() {
                Ok(item) => item,
                Err(e) => {
                    eprintln!("Got invalid message from notification daemon: {}", e);
                    continue;
                }
            };
            let id = match emitter_.translate_host_id(item.id) {
                None => continue,
                Some(id) => id,
            };
            let data = options
                .serialize(&ReplyMessage::ActionInvoked {
                    id,
                    action: item.action_key,
                })
                .expect("Serialization failed?");
            stdout_.transmit(&*data).await
        }
    });
    eprintln!("Entering loop");
    loop {
        let size = match stdin.read_u32_le().await {
            Ok(size) => size.to_le(),
            Err(e) => match e.kind() {
                std::io::ErrorKind::UnexpectedEof => break,
                e => panic!("Error reading from stdin: {}", e),
            },
        };
        if size > MAX_MESSAGE_SIZE {
            panic!("Message too large ({} bytes)", size)
        }
        let mut bytes = vec![0; size as _];
        match stdin.read_exact(&mut bytes[..]).await {
            Ok(bytes_read) => assert_eq!(bytes_read, size as _),
            Err(e) => match e.kind() {
                std::io::ErrorKind::UnexpectedEof => break,
                e => panic!("Error reading from stdin: {}", e),
            },
        };
        let message: notification_emitter::Message = options
            .deserialize(&bytes)
            .expect("malformed input from client");
        let sequence = message.id;
        let emitter = emitter.clone();
        let stdout = stdout.clone();
        tokio::task::spawn_local(async move {
            let out = emitter.send_notification(message.notification).await;
            let data = options
                .serialize(&match out {
                    Ok(id) => ReplyMessage::Id {
                        id: id.into(),
                        sequence,
                    },
                    Err(zbus::Error::MethodError(name, message, _)) => ReplyMessage::DBusError {
                        name: name.to_string(),
                        message,
                        sequence,
                    },
                    Err(e) => {
                        eprintln!("Serialization failed for {:?}", e);
                        ReplyMessage::UnknownError { sequence }
                    }
                })
                .expect("Serialization failed?");
            stdout.transmit(&*data).await;
        });
    }
    eprintln!("Leaving loop");
    invoked_stream_handle.abort();
    closed_stream_handle.abort();
    owner_changed_handle.abort();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let local_set = tokio::task::LocalSet::new();

    let source = std::env::var("QREXEC_REMOTE_DOMAIN").expect("No remote domain in qrexec");
    local_set.spawn_local(client_server(source));
    Ok(local_set.await)
}
