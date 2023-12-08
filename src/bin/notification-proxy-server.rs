use bincode::Options;
use futures_util::StreamExt;
use notification_emitter::{merge_versions, Notification, NotificationEmitter};
use notification_emitter::{
    MessageWriter, ReplyMessage, MAJOR_VERSION, MAX_MESSAGE_SIZE, MINOR_VERSION,
};
use std::rc::Rc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

async fn client_server(qube_name: String) {
    let emitter = Rc::new(
        NotificationEmitter::new(
            qube_name.to_owned() + ": ",
            "Qubes VM ".to_owned() + &*qube_name,
        )
        .await
        .expect("Cannot connect to notifcation daemon"),
    );
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
    if reply_major != MAJOR_VERSION {
        panic!(
            "Version mismatch: client supports version {reply_major} \
            but the version supported by this server is {MAJOR_VERSION}"
        );
    }
    if reply_minor > MINOR_VERSION {
        panic!(
            "Version mismatch: client supports version {reply_minor} \
but this server only supports version {MINOR_VERSION}"
        );
    }
    let stdout = MessageWriter::new();
    let mut closed_stream = emitter
        .closed()
        .await
        .expect("Cannot register for closed signals");
    let mut invoked_stream = emitter
        .invocations()
        .await
        .expect("Cannot register for invoked signals");
    let stdout_ = stdout.clone();
    let _handle = tokio::task::spawn_local(async move {
        while let Some(item) = closed_stream.next().await {
            let item = match item.args() {
                Ok(item) => item,
                Err(e) => {
                    eprintln!("Got invalid message from notification daemon: {}", e);
                    continue;
                }
            };
            let data = options
                .serialize(&ReplyMessage::Dismissed {
                    id: item.id,
                    reason: item.reason,
                })
                .expect("Serialization failed?");
            stdout_.transmit(&*data).await
        }
    });
    let stdout_ = stdout.clone();
    let _handle = tokio::task::spawn_local(async move {
        while let Some(item) = invoked_stream.next().await {
            let item = match item.args() {
                Ok(item) => item,
                Err(e) => {
                    eprintln!("Got invalid message from notification daemon: {}", e);
                    continue;
                }
            };
            let data = options
                .serialize(&ReplyMessage::ActionInvoked {
                    id: item.id,
                    action: item.action_key,
                })
                .expect("Serialization failed?");
            stdout_.transmit(&*data).await
        }
    });
    eprintln!("Entering loop");
    loop {
        let size = stdin
            .read_u32_le()
            .await
            .expect("Error reading from stdin")
            .to_le();
        if size > MAX_MESSAGE_SIZE {
            panic!("Message too large ({} bytes)", size)
        }
        eprintln!("{} bytes to read!", size);
        let mut bytes = vec![0; size as _];
        let bytes_read = stdin
            .read_exact(&mut bytes[..])
            .await
            .expect("error reading from stdin");
        assert_eq!(bytes_read, size as _);
        let message: Notification = options
            .deserialize(&bytes)
            .expect("malformed input from client");
        let sequence = message.id;
        let emitter = emitter.clone();
        let stdout = stdout.clone();
        tokio::task::spawn_local(async move {
            let out = emitter.send_notification(message).await;
            let data = options
                .serialize(&match out {
                    Ok(id) => ReplyMessage::Id { id, sequence },
                    Err(zbus::Error::MethodError(name, message, _)) => ReplyMessage::DBusError {
                        name: name.to_string(),
                        message,
                        sequence,
                    },
                    Err(_) => ReplyMessage::UnknownError { sequence },
                })
                .expect("Serialization failed?");
            stdout.transmit(&*data).await
        });
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let local_set = tokio::task::LocalSet::new();

    let source = std::env::var("QREXEC_REMOTE_DOMAIN").expect("No remote domain in qrexec");
    local_set.spawn_local(client_server(source));
    Ok(local_set.await)
}
