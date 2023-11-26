use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::rc::Rc;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;
use zbus::{dbus_proxy, zvariant::Type, zvariant::Value, Connection};
#[dbus_proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
pub trait Notifications {
    fn get_capabilities(&self) -> zbus::Result<(Vec<String>,)>;
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[String],
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

pub const MAX_MESSAGE_SIZE: u32 = 0x1_000_000; // max size in bytes

#[derive(Serialize, Deserialize, Debug)]
/// Messages sent by a notification server
pub enum ReplyMessage {
    /// Notification successfully sent.
    Id {
        /// ID of the created notification.
        id: u32,
        /// The sequence number of this method call
        sequence: u64,
    },
    /// D-Bus error
    DBusError {
        /// Error name
        name: String,
        /// Error message
        message: Option<String>,
        /// The sequence number of this method call
        sequence: u64,
    },
    UnknownError {
        /// The sequence number of this method call
        sequence: u64,
    },
    /// Notification was dismissed by the server.
    Dismissed {
        /// ID of the dismissed notification.
        id: u32,
        /// Reason the notification was dismissed.
        reason: u32,
    },
    /// An action was invoked.
    ActionInvoked {
        /// ID of the notification on which the action was invoked.
        id: u32,
        /// Action that was invoked
        action: String,
    },
}

#[repr(u8)]
#[derive(Serialize, Deserialize, Debug)]
pub enum Urgency {
    Low = 0,
    Normal = 1,
    Critical = 2,
}

pub const MAX_SIZE: usize = 1usize << 21; // This is 2MiB, more than enough
pub const MAX_WIDTH: i32 = 255;
pub const MAX_HEIGHT: i32 = 255;

#[derive(Serialize, Deserialize, Debug, Value, Type)]
/// Image parameters
pub struct ImageParameters {
    /// The width of the image.  Not trusted.
    pub untrusted_width: i32,
    /// The height of the image.  Not trusted.
    pub untrusted_height: i32,
    /// The rowstride of the image.  Not trusted.
    pub untrusted_rowstride: i32,
    /// Whether the image has an alpha value.
    pub untrusted_has_alpha: bool,
    /// The bits per sample of the image.  Not trusted.
    pub untrusted_bits_per_sample: i32,
    /// The number of channels of the image.  Not trusted.
    pub untrusted_channels: i32,
    /// The image data.  Not trusted.
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

    // booleans do not need to be sanitized
    let has_alpha = untrusted_has_alpha;

    // bits per sample must be 8
    if untrusted_bits_per_sample != 8 {
        return Err("Wrong number of bits per sample");
    }

    let bits_per_sample = untrusted_bits_per_sample;

    // data cannot be too long
    if untrusted_data.len() > MAX_SIZE {
        return Err("Too much data");
    }

    let data = untrusted_data;

    // compute the number of channels and check that it matches what
    // was provided
    let channels = 3i32 + has_alpha as i32;
    if untrusted_channels != channels {
        return Err("Wrong number of channels");
    }

    // image must be at least 1x1
    if untrusted_width < 1 || untrusted_height < 1 || untrusted_rowstride < 3 {
        return Err("Too small width, height, or stride");
    }

    // check that the image is not too large
    if untrusted_width > MAX_WIDTH || untrusted_height > MAX_HEIGHT {
        return Err("Width or height too large");
    }

    // check that the image fits in the buffer
    if data.len() as i32 / untrusted_height < untrusted_rowstride {
        return Err("Image too large");
    }

    // check that the rows fit in the stride
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

#[link(kind = "dylib", name = "qubes-pure")]
extern "C" {
    fn qubes_pure_code_point_safe_for_display(code_point: u32) -> bool;
}

pub fn sanitize_str(arg: &str) -> String {
    arg.chars()
        .map(|c| {
            // SAFETY: this function is not actually unsafe
            if unsafe { qubes_pure_code_point_safe_for_display(c.into()) } {
                c
            } else {
                // This is U+FFFD REPLACEMENT CHARACTER
                '\u{FFFD}'
            }
        })
        .collect()
}

bitflags! {
    #[derive(Default)]
    pub struct Capabilities: u16 {
        const BODY            = 0b0000000001;
        const BODY_HYPERLINKS = 0b0000000010;
        const BODY_MARKUP     = 0b0000000100;
        const PERSISTENCE     = 0b0000001000;
        const SOUND           = 0b0000010000;
        const BODY_IMAGES     = 0b0000100000;
        const ICON_MULTI      = 0b0001000000;
        const ICON_STATIC     = 0b0010000000;
        const ACTIONS         = 0b0100000000;
        const ACTION_ICONS    = 0b1000000000;
   }
}

pub struct NotificationEmitter {
    proxy: NotificationsProxy<'static>,
    capabilities: Capabilities,
    prefix: String,
    application_name: String,
}

impl NotificationEmitter {
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
    pub async fn new(prefix: String, application_name: String) -> zbus::Result<Self> {
        let connection = Connection::session().await?;
        let proxy = NotificationsProxy::new(&connection).await?;
        let capabilities_list = proxy.get_capabilities().await?.0;
        let mut capabilities = Capabilities::default();
        for capability_str in capabilities_list.into_iter() {
            match &*capability_str {
                "action-icons" => capabilities |= Capabilities::ACTION_ICONS,
                "persistence" => capabilities |= Capabilities::PERSISTENCE,
                "body-markup" => capabilities |= Capabilities::BODY_MARKUP,
                "sound" => capabilities |= Capabilities::SOUND,
                "body" => capabilities |= Capabilities::BODY,
                "body-hyperlinks" => capabilities |= Capabilities::BODY_HYPERLINKS,
                "body-images" => capabilities |= Capabilities::BODY_IMAGES,
                "icon-static" => capabilities |= Capabilities::ICON_STATIC,
                "actions" => capabilities |= Capabilities::ACTIONS,
                "icon-multi" => capabilities |= Capabilities::ICON_MULTI,
                _ => eprintln!("Unknown capability {} detected", capability_str),
            }
        }
        eprintln!(
            "Server capabilities: body markup {}, persistence {}",
            capabilities.contains(Capabilities::BODY_MARKUP),
            capabilities.contains(Capabilities::PERSISTENCE),
        );
        Ok(Self {
            proxy,
            capabilities,
            prefix,
            application_name,
        })
    }
}

#[derive(Debug, Clone)]
pub struct MessageWriter(Rc<Mutex<tokio::io::Stdout>>);

impl MessageWriter {
    pub fn new() -> Self {
        Self(Rc::new(Mutex::new(tokio::io::stdout())))
    }
    pub async fn transmit(&self, data: &[u8]) {
        let len: u32 = data.len().try_into().unwrap();
        let mut guard = self.0.lock().await;
        guard
            .write_u32_le(len.to_le())
            .await
            .expect("error writing to stdout");
        guard
            .write_all(&*data)
            .await
            .expect("error writing to stdout");
        guard.flush().await.expect("error writing to stdout");
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Notification {
    pub id: u64,
    pub suppress_sound: bool,
    pub transient: bool,
    pub urgency: Option<Urgency>,
    // This is just an ID, and it can't be validated in a non-racy way anyway.
    // I assume that any decent notification daemon will handle an invalid ID
    // value correctly, but this code should probably test for this at the start
    // so that it cannot be used with a server that crashes in this case.
    pub replaces_id: u32,
    pub summary: String,
    // FIXME: support markup (strictly sanitized and validated) if the server
    // supports it.
    pub body: String,
    pub actions: Vec<String>,
    pub category: Option<String>,
    pub expire_timeout: i32,
    pub image: Option<ImageParameters>,
}

impl NotificationEmitter {
    #[inline]
    /// Whether the server supports persistence
    pub fn persistence(&self) -> bool {
        self.capabilities.contains(Capabilities::PERSISTENCE)
    }
    #[inline]
    /// Whether the server supports sound
    pub fn sound(&self) -> bool {
        self.capabilities.contains(Capabilities::SOUND)
    }
    #[inline]
    /// Whether the server supports actions
    pub fn actions(&self) -> bool {
        self.capabilities.contains(Capabilities::ACTIONS)
    }
    #[inline]
    /// Whether the server supports body markup
    pub fn body_markup(&self) -> bool {
        self.capabilities.contains(Capabilities::BODY_MARKUP)
    }
    #[inline]
    /// Whether the server supports notification bodies
    pub fn body(&self) -> bool {
        self.capabilities.contains(Capabilities::BODY)
    }
    pub async fn closed(&self) -> zbus::Result<NotificationClosedStream<'static>> {
        self.proxy.receive_notification_closed().await
    }
    pub async fn invocations(&self) -> zbus::Result<ActionInvokedStream<'static>> {
        self.proxy.receive_action_invoked().await
    }
    pub async fn send_notification(
        &self,
        Notification {
            id: _,
            suppress_sound,
            transient,
            urgency,
            replaces_id,
            summary: untrusted_summary,
            body: untrusted_body,
            actions: untrusted_actions,
            category: untrusted_category,
            expire_timeout,
            image,
        }: Notification,
    ) -> zbus::Result<u32> {
        if expire_timeout < -1 {
            return Err(zbus::Error::Unsupported);
        }

        // In the future this should be a validated application name prefixed
        // by the qube name.
        let application_name = self.application_name.clone();

        // Ideally the icon would be associated with the calling application,
        // with an image suitably processed by Qubes OS to indicate trust.
        // However, there is no good way to do that in practice, so just pass
        // an empty string to indicate "no icon".
        let icon = "";
        let actions = if self.actions() {
            let mut actions = Vec::with_capacity(untrusted_actions.len());
            for i in untrusted_actions {
                actions.push(sanitize_str(&*i))
            }
            actions
        } else {
            vec![]
        };

        // this is slow but I don't care, the D-Bus call is orders of magnitude slower

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
        if suppress_sound && self.capabilities.contains(Capabilities::SOUND) {
            hints.insert("suppress-sound", Value::from(&true));
        }
        if transient && self.persistence() {
            hints.insert("transient", Value::from(&true));
        }
        if let Some(ref untrusted_category) = untrusted_category {
            let category = untrusted_category.as_bytes();
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
            // sanitize end
            hints.insert("category", Value::from(category));
        }
        if let Some(image) = image {
            match serialize_image(image) {
                Ok(value) => hints.insert("image-data", value),
                Err(e) => return Err(zbus::Error::MissingParameter(e)),
            };
        }
        let mut escaped_body;
        if self.body_markup() {
            let body = sanitize_str(&*untrusted_body);
            // Body markup must be escaped.  FIXME: validate it instead.
            escaped_body = String::with_capacity(body.as_bytes().len());
            // this is slow and can easily be made much faster with
            // trivially correct `unsafe`, but the D-Bus call (which
            // actually renders text on screen!) will be orders of
            // magnitude slower so we do not care.
            for i in body.chars() {
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
            escaped_body = sanitize_str(&*untrusted_body)
        }
        self.proxy
            .notify(
                application_name,
                replaces_id,
                icon,
                &*(self.prefix.clone() + &*sanitize_str(&*untrusted_summary)),
                &*escaped_body,
                &*actions,
                &hints,
                expire_timeout,
            )
            .await
    }
}
