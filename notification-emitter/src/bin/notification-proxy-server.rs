use bincode::Options;
use futures_util::StreamExt;
use notification_emitter::{MessageWriter, ReplyMessage, MAX_MESSAGE_SIZE};
use notification_emitter::{Notification, NotificationEmitter};
use std::rc::Rc;
use tokio::io::AsyncReadExt;

async fn client_server() {
    let emitter = Rc::new(
        NotificationEmitter::new()
            .await
            .expect("Cannot connect to notifcation daemon"),
    );
    let options = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_native_endian()
        .reject_trailing_bytes();
    let mut stdin = tokio::io::stdin();
    let stdout = MessageWriter::new();
    let mut closed_stream = emitter
        .closed()
        .await
        .expect("Cannot register for closed signals");
    let mut _invoked_stream = emitter
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
        while let Some(item) = _invoked_stream.next().await {
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

    local_set.spawn_local(client_server());
    Ok(local_set.await)
}
