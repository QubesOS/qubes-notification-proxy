use bincode::Options;
use notification_emitter::{Notification, NotificationEmitter};
use notification_emitter::{ReplyMessage, MAX_MESSAGE_SIZE};
use std::rc::Rc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

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
    let stdout = Rc::new(Mutex::new(tokio::io::stdout()));

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
        let message: Notification = options
            .deserialize(&bytes)
            .expect("malformed input from client");
        let emitter = emitter.clone();
        let stdout = stdout.clone();
        tokio::task::spawn_local(async move {
            let out = emitter.send_notification(message).await;
            let data = options
                .serialize(&match out {
                    Ok(id) => ReplyMessage::Id { id },
                    Err(zbus::Error::MethodError(name, message, _)) => ReplyMessage::DBusError {
                        name: name.to_string(),
                        message,
                    },
                    Err(_) => ReplyMessage::UnknownError,
                })
                .expect("Serialization failed?");
            let len: u32 = data.len().try_into().unwrap();
            let mut guard = stdout.lock().await;
            guard
                .write_u32_le(len.to_le())
                .await
                .expect("error writing to stdout");
            guard
                .write_all(&*data)
                .await
                .expect("error writing to stdout");
        });
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let local_set = tokio::task::LocalSet::new();

    local_set.spawn_local(client_server());
    Ok(local_set.await)
}
