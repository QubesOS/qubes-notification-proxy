use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use zbus::{dbus_proxy, zvariant::Value, Connection};
#[dbus_proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
pub trait Notifications {
    fn get_capabilities(&self) -> zbus::Result<(Vec<String>,)>;
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: &HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
    fn close_notification(&self, id: u32) -> zbus::Result<()>;
    fn get_server_information(&self) -> zbus::Result<(String, String, String, String)>;
    #[dbus_proxy(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> Result<()>;
    #[dbus_proxy(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> Result<()>;
}

#[repr(u8)]
pub enum Urgency {
    Low = 0,
    Normal = 1,
    Critical = 2,
}

pub const MAX_SIZE: usize = 1usize << 21; // This is 2MiB, more than enough
pub const MAX_WIDTH: i32 = 255;
pub const MAX_HEIGHT: i32 = 255;

#[derive(Serialize, Deserialize, Debug, Value)]
pub struct ImageParameters {
    pub untrusted_width: i32,
    pub untrusted_height: i32,
    pub untrusted_rowstride: i32,
    pub untrusted_has_alpha: bool,
    pub untrusted_bits_per_sample: i32,
    pub untrusted_channels: i32,
    pub untrusted_data: Vec<u8>,
}

fn serialize_image(
    ImageParameters {
        untrusted_width,
        untrusted_height,
        untrusted_rowstride,
        untrusted_has_alpha,
        untrusted_bits_per_sample,
        untrusted_channels,
        untrusted_data,
    }: ImageParameters,
) -> Result<Value<'static>, &'static str> {
    // sanitize start
    let has_alpha = untrusted_has_alpha; // no sanitization required
    if untrusted_width < 1 || untrusted_height < 1 || untrusted_rowstride < 3 {
        return Err("Too small width, height, or stride");
    }

    if untrusted_data.len() > MAX_SIZE {
        return Err("Too much data");
    }

    if untrusted_bits_per_sample != 8 {
        return Err("Wrong number of bits per sample");
    }

    let bits_per_sample = untrusted_bits_per_sample;
    let data = untrusted_data;
    let channels = 3i32 + untrusted_has_alpha as i32;

    if untrusted_channels != channels {
        return Err("Wrong number of channels");
    }

    if untrusted_width > MAX_WIDTH || untrusted_height > MAX_HEIGHT {
        return Err("Width or height too large");
    }

    if data.len() as i32 / untrusted_height < untrusted_rowstride {
        return Err("Image too large");
    }

    if untrusted_rowstride / channels < untrusted_width {
        return Err("Row stride too small");
    }

    let height = untrusted_height;
    let width = untrusted_width;
    let rowstride = untrusted_rowstride;
    // sanitize end

    return Ok(Value::from((
        width,
        height,
        rowstride,
        has_alpha,
        bits_per_sample,
        channels,
        data,
    )));
}

#[repr(transparent)]
pub struct TrustedStr(String);

impl TrustedStr {
    pub fn new(arg: String) -> Result<Self, &'static str> {
        // FIXME: validate this.  The current C API is unsuitable as it only returns
        // a boolean rather than replacing forbidden characters or even indicating
        // what those forbidden characters are.  This should be fixed on the C side
        // rather than by ugly hacks (such as character-by-character loops).
        return Ok(TrustedStr(arg));
    }

    pub fn inner(&self) -> &String {
        &self.0
    }
}

pub struct NotificationEmitter {
    proxy: NotificationsProxy<'static>,
    body_markup: bool,
    persistence: bool,
}

impl NotificationEmitter {
    pub async fn new() -> zbus::Result<Self> {
        let connection = Connection::session().await?;
        let proxy = NotificationsProxy::new(&connection).await?;
        let capabilities = proxy.get_capabilities().await?.0;
        let mut body_markup = false;
        let mut persistence = false;
        for capability in capabilities.into_iter() {
            match &*capability {
                "persistence" => persistence = true,
                "body-markup" => body_markup = true,
                _ => eprintln!("Unknown capability {} detected", capability),
            }
        }
        eprintln!(
            "Server capabilities: body markup {}, persistence {}",
            body_markup, persistence
        );
        Ok(Self {
            proxy,
            body_markup,
            persistence,
        })
    }
}

impl NotificationEmitter {
    pub fn persistence(&self) -> bool {
        self.persistence
    }
    pub fn body_markup(&self) -> bool {
        false
    }
    pub async fn send_notification(
        &self,
        suppress_sound: bool,
        transient: bool,
        urgency: Option<Urgency>,
        // This is just an ID, and it can't be validated in a non-racy way anyway.
        // I assume that any decent notification daemon will handle an invalid ID
        // value correctly, but this code should probably test for this at the start
        // so that it cannot be used with a server that crashes in this case.
        replaces: u32,
        summary: TrustedStr,
        // FIXME: handle markup
        body: TrustedStr,
        actions: Vec<TrustedStr>,
        // this is santiized internally
        category: Option<String>,
        expire_timeout: i32,
        image: Option<ImageParameters>,
    ) -> zbus::Result<u32> {
        if expire_timeout < -1 {
            return Err(zbus::Error::Unsupported);
        }

        // In the future this should be a validated application name prefixed
        // by the qube name.
        let application_name = "";

        // Ideally the icon would be associated with the calling application,
        // with an image suitably processed by Qubes OS to indicate trust.
        // However, there is no good way to do that in practice, so just pass
        // an empty string to indicate "no icon".
        let icon = "";

        // this is slow but I don't care, the dbus call is orders of magnitude slower
        let actions: Vec<&str> = actions.iter().map(|x| &*x.0).collect();

        // Set up the hints
        let mut hints = HashMap::new();
        if let Some(urgency) = urgency {
            // this is a hack to appease the borrow checker
            let urgency = match urgency {
                Urgency::Low => &0,
                Urgency::Normal => &1,
                Urgency::Critical => &2,
            };
            hints.insert(
                "urgency",
                <zbus::zvariant::Value<'_> as From<&'_ u8>>::from(urgency),
            );
        }
        if suppress_sound {
            hints.insert("suppress-sound", Value::from(&true));
        }
        if transient {
            hints.insert("transient", Value::from(&true));
        }
        if let Some(ref category) = category {
            let category = category.as_bytes();
            match category.get(0) {
                Some(b'a'..=b'z') => {}
                _ => return Err(zbus::Error::MissingParameter("Invalid category")),
            }
            for i in &category[1..] {
                match i {
                    b'a'..=b'z' | b'.' => {}
                    _ => return Err(zbus::Error::MissingParameter("Invalid category")),
                }
            }
            // no underflow possible, category.get() checks for the empty slice
            if category[category.len() - 1] == b'.' {
                return Err(zbus::Error::MissingParameter("Invalid category"));
            }
            hints.insert("category", Value::from(category));
        }
        if let Some(image) = image {
            match serialize_image(image) {
                Ok(value) => hints.insert("image-data", value),
                Err(e) => return Err(zbus::Error::MissingParameter(e)),
            };
        }
        let mut escaped_body;
        if self.body_markup {
            // Body markup must be escaped.  FIXME: validate it.
            escaped_body = String::with_capacity(body.0.as_bytes().len());
            // this is slow and can easily be made much faster with
            // trivially correct `unsafe`, but the dbus call (which
            // actually renders text on screen!) will be orders of
            // magnitude slower so we do not care.
            for i in body.0.chars() {
                match i {
                    '<' => escaped_body.push_str("&lt;"),
                    '>' => escaped_body.push_str("&gt;"),
                    '&' => escaped_body.push_str("&amp;"),
                    '\'' => escaped_body.push_str("&apos;"),
                    '"' => escaped_body.push_str("&quot;"),
                    x => escaped_body.push(x),
                }
            }
        } else {
            escaped_body = body.0.clone()
        }
        self.proxy
            .notify(
                application_name,
                replaces,
                icon,
                &*summary.0,
                &*escaped_body,
                &*actions,
                &hints,
                expire_timeout,
            )
            .await
    }
}

