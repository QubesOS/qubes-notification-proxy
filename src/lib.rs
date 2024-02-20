use bitflags::bitflags;
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::rc::Rc;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;
use zbus::{
    dbus_proxy,
    fdo::{DBusProxy, NameOwnerChangedStream},
    zvariant::Type,
    zvariant::Value,
    Connection,
};
mod maps;
use maps::{GuestId, HostId, Maps};
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
    // Non-standard KDE extension
    #[dbus_proxy(signal)]
    fn notification_replied(&self, id: u32, text: String) -> Result<()>;
}

pub const MAX_MESSAGE_SIZE: u32 = 0x1_000_000; // max size in bytes

fn is_valid_action_name(action: &[u8]) -> bool {
    // 255 is arbitrary but should be more than enough
    if action.is_empty() {
        return false;
    }
    if action.len() > 255 {
        return false;
    }
    match action[0] {
        b'a'..=b'z' | b'A'..=b'Z' => {}
        _ => return false,
    }
    for i in &action[1..] {
        match i {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' => {}
            _ => return false,
        }
    }
    return true;
}

#[derive(Serialize, Deserialize, Debug)]
/// Messages sent by a notification server
pub enum ReplyMessage {
    /// Notification successfully sent.  Since version 0
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
    /// Something unknown went wrong.
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
    /// Server restarted.
    ServerRestart,
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

pub const MAJOR_VERSION: u16 = 1;
pub const MINOR_VERSION: u16 = 0;

pub const fn merge_versions(major: u16, minor: u16) -> u32 {
    (major as u32) << 16 | (minor as u32)
}

pub const fn split_version(combined: u32) -> (u16, u16) {
    ((combined >> 16) as _, combined as _)
}

#[derive(Serialize, Deserialize, Debug, Value, Type, Clone)]
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

const MAX_LINES: usize = 500;
const MAX_CHARS_PER_LINE: usize = 1000;

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
    if untrusted_width < 1 || untrusted_height < 1 || untrusted_rowstride < channels {
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

/// This imposes the following restrictions:
///
/// - Characters are limited to a safe subset of Unicode.
/// - Lines are limited to 1000 characters.
/// - Text is truncated after 500 lines.
///
/// Too many lines in particular is known to make xfce4-notifyd spin and consume 100% CPU.
pub fn sanitize_str(arg: &str) -> String {
    let mut res = String::with_capacity(arg.len());
    let mut iter = arg.chars().peekable();
    let mut counter = 0;
    let mut lines = 0;
    while let Some(c) = iter.next() {
        res.push(
            // SAFETY: this function is not actually unsafe
            if unsafe { qubes_pure_code_point_safe_for_display(c.into()) } || c == '\t' {
                counter += 1;
                c
            } else if c == '\n' {
                counter = 0;
                lines += 1;
                c
            } else if c == '\r' {
                if iter.peek() == Some(&'\n') {
                    continue;
                }
                counter = 0;
                lines += 1;
                '\n'
            } else {
                // This is U+FFFD REPLACEMENT CHARACTER
                counter += 1;
                '\u{FFFD}'
            },
        );
        if counter >= MAX_CHARS_PER_LINE {
            res.push('\n');
            counter = 0;
            lines += 1;
        }
        if lines >= MAX_LINES {
            break; // notification daemon will hang if there are too many lines
        }
    }
    res
}

bitflags! {
    #[derive(Default)]
    pub struct Capabilities: u16 {
        const BODY            = 0b00000000001;
        const BODY_HYPERLINKS = 0b00000000010;
        const BODY_MARKUP     = 0b00000000100;
        const PERSISTENCE     = 0b00000001000;
        const SOUND           = 0b00000010000;
        const BODY_IMAGES     = 0b00000100000;
        const ICON_MULTI      = 0b00001000000;
        const ICON_STATIC     = 0b00010000000;
        const ACTIONS         = 0b00100000000;
        const ACTION_ICONS    = 0b01000000000;
        const INLINE_REPLY    = 0b10000000000;
   }
}

pub struct NotificationEmitter {
    notification_proxy: NotificationsProxy<'static>,
    capabilities: Capabilities,
    prefix: String,
    application_name: String,
    maps: std::cell::RefCell<Maps>,
}

impl NotificationEmitter {
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
    pub async fn new(
        prefix: String,
        application_name: String,
    ) -> zbus::Result<(Self, NameOwnerChangedStream<'static>)> {
        let connection = Connection::session().await?;
        let (dbus_proxy, notification_proxy) = futures_util::future::join(
            DBusProxy::new(&connection).and_then(move |proxy| async move {
                proxy
                    .receive_name_owner_changed_with_args(&[(0, &*"org.freedesktop.Notifications")])
                    .await
            }),
            NotificationsProxy::new(&connection).and_then(move |proxy| async move {
                let caps = proxy.get_capabilities().await?.0;
                Ok((proxy, caps))
            }),
        )
        .await;
        let (dbus_proxy, (notification_proxy, capabilities_list)) =
            (dbus_proxy?, notification_proxy?);
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
                "inline-reply" => capabilities |= Capabilities::INLINE_REPLY,
                _ => eprintln!("Unknown capability {} detected", capability_str),
            }
        }
        eprintln!(
            "Server capabilities: body markup {}, persistence {}",
            capabilities.contains(Capabilities::BODY_MARKUP),
            capabilities.contains(Capabilities::PERSISTENCE),
        );
        Ok((
            Self {
                notification_proxy,

                capabilities,
                prefix,
                application_name,
                maps: Default::default(),
            },
            dbus_proxy,
        ))
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
pub struct Message {
    pub id: u64,
    pub notification: Notification,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Notification {
    V1 {
        suppress_sound: bool,
        transient: bool,
        resident: bool,
        urgency: Option<Urgency>,
        replaces_id: u32,
        summary: String,
        // FIXME: support markup (strictly sanitized and validated) if the server
        // supports it.
        body: String,
        actions: Vec<String>,
        category: Option<String>,
        expire_timeout: i32,
        image: Option<ImageParameters>,
    },
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
        self.notification_proxy.receive_notification_closed().await
    }
    pub async fn invocations(&self) -> zbus::Result<ActionInvokedStream<'static>> {
        self.notification_proxy.receive_action_invoked().await
    }
    pub async fn replies(&self) -> zbus::Result<NotificationRepliedStream<'static>> {
        self.notification_proxy.receive_notification_replied().await
    }
    pub fn translate_host_id(&self, id: u32) -> Option<u32> {
        match HostId::new_less_safe(id) {
            None => Some(0),
            Some(a) => match self.maps.borrow().lookup_host_id(a) {
                None => {
                    eprintln!("ID {} not found!", u32::from(a));
                    None
                }
                Some(guest) => Some(guest.into()),
            },
        }
    }
    pub fn clear(&self) {
        self.maps.borrow_mut().clear()
    }
    pub fn remove_host_id(&self, id: u32) -> Option<u32> {
        match HostId::new_less_safe(id) {
            None => Some(0),
            Some(a) => match self.maps.borrow_mut().remove_host_id(a) {
                None => {
                    eprintln!("ID {} not found!", u32::from(a));
                    None
                }
                Some(guest) => Some(guest.into()),
            },
        }
    }
    pub async fn send_notification(
        &self,
        Notification::V1 {
            suppress_sound,
            transient,
            resident,
            urgency,
            replaces_id,
            summary: untrusted_summary,
            body: untrusted_body,
            actions: untrusted_actions,
            category: untrusted_category,
            expire_timeout,
            image,
        }: Notification,
    ) -> zbus::Result<GuestId> {
        let guest_id = maps::GuestId::new_less_safe(replaces_id);
        let host_id = match guest_id {
            None => None,
            Some(id) => match self.maps.borrow().lookup_guest_id(id) {
                None => {
                    return Err(zbus::Error::Failure(format!(
                        "ID {} not found in guest-to-host lookup map",
                        u32::from(id),
                    )))
                }
                Some(id) => Some(id),
            },
        };
        if expire_timeout < -1 {
            return Err(zbus::Error::Unsupported);
        }

        if untrusted_actions.len() & 1 != 0 {
            return Err(zbus::Error::Failure(format!(
                "Actions must have an even length, got {}",
                untrusted_actions.len()
            )));
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
            for (count, s) in untrusted_actions.iter().enumerate() {
                if count & 1 == 0 {
                    if !is_valid_action_name(s.as_bytes()) {
                        return Err(zbus::Error::Failure("Invalid action name".to_owned()));
                    }
                    // Sanitized by is_valid_action_name()
                    actions.push(s.to_owned())
                } else {
                    actions.push(sanitize_str(&*s))
                }
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
        if resident && self.capabilities.contains(Capabilities::PERSISTENCE) {
            hints.insert("resident", Value::from(&true));
        }
        if suppress_sound && self.capabilities.contains(Capabilities::SOUND) {
            hints.insert("suppress-sound", Value::from(&true));
        }
        if transient && self.persistence() {
            hints.insert("transient", Value::from(&true));
        }
        if let Some(ref untrusted_category) = untrusted_category {
            let category = untrusted_category.as_bytes();
            if category.len() > 64 {
                return Err(zbus::Error::MissingParameter("Invalid category"));
            }
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
        // Temporarily disabled due to lack of image processing
        if false {
            if let Some(image) = image {
                match serialize_image(image) {
                    Ok(value) => hints.insert("image-data", value),
                    Err(e) => return Err(zbus::Error::MissingParameter(e)),
                };
            }
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
        let host_id_num = match host_id {
            None => 0,
            Some(i) => i.into(),
        };
        let id = HostId::new_less_safe(
            self.notification_proxy
                .notify(
                    application_name,
                    host_id_num,
                    icon,
                    &*(self.prefix.clone() + &*sanitize_str(&*untrusted_summary)),
                    &*escaped_body,
                    &*actions,
                    &hints,
                    expire_timeout,
                )
                .await?,
        )
        .expect("Notification daemon sent a zero ID?");

        Ok(self.maps.borrow_mut().next_id(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_discriminant_serialized() {
        use bincode::Options as _;
        let options = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_native_endian()
            .reject_trailing_bytes();
        let v = options
            .serialize(&Notification::V1 {
                suppress_sound: true,
                transient: false,
                resident: false,
                urgency: None,
                replaces_id: 0,
                summary: "".to_owned(),
                body: "".to_owned(),
                actions: vec![],
                category: None,
                expire_timeout: 0,
                image: None,
            })
            .unwrap();
        assert_eq!(&v[..4], &[0, 0, 0, 0][..])
    }
    #[test]
    fn test_enum_extensibility() {
        #[derive(Serialize, Deserialize)]
        enum A {
            B { x: bool },
        }
        #[derive(Serialize, Deserialize)]
        enum D {
            B { x: bool },
            C { x: u32 },
        }
        use bincode::Options as _;
        let options = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_native_endian()
            .reject_trailing_bytes();
        let serialized = options.serialize(&A::B { x: true }).unwrap();
        let deserialized: D = options.deserialize(&serialized).unwrap();
        assert!(matches!(deserialized, D::B { x: true }));
        assert_eq!(serialized, options.serialize(&D::B { x: true }).unwrap());
    }

    #[test]
    fn test_sanitize_str_basic() {
        // The underlying C library has extensive tests,
        // including a test that it is memory safe on all possible
        // inputs.  Only do minimal tests here.
        assert_eq!(sanitize_str("&"), "&".to_owned());
        assert_eq!(sanitize_str("\n"), "\n".to_owned());
        assert_eq!(sanitize_str("\t"), "\t".to_owned());
        // \x15 isn't safe
        assert_eq!(sanitize_str("a\x15\n"), "a\u{FFFD}\n".to_owned());
    }

    #[test]
    fn test_too_many_lines() {
        let max_lines = str::repeat("a\n", 500);
        assert_eq!(&sanitize_str(&*max_lines), &max_lines, "500 lines are fine");
        assert_eq!(
            sanitize_str(&*(max_lines.clone() + &"a\n"[..])),
            max_lines,
            "501 lines are not"
        );
    }
    #[test]
    fn test_too_long_lines() {
        let really_really_long = str::repeat("a", MAX_LINES * MAX_CHARS_PER_LINE);
        let long_sanitized = sanitize_str(&*really_really_long);
        assert_eq!(long_sanitized.len(), (MAX_CHARS_PER_LINE + 1) * MAX_LINES);
        let cmp = vec![str::repeat("a", MAX_CHARS_PER_LINE); MAX_LINES].join("\n") + "\n";
        assert_eq!(long_sanitized.len(), cmp.len());
        assert_eq!(long_sanitized, cmp);
    }

    #[test]
    fn test_gigunda() {
        let really_really_long = str::repeat("a", MAX_LINES * 2 * MAX_CHARS_PER_LINE);
        let long_sanitized = sanitize_str(&*really_really_long);
        assert_eq!(long_sanitized.len(), (MAX_CHARS_PER_LINE + 1) * MAX_LINES);
        let cmp = vec![str::repeat("a", MAX_CHARS_PER_LINE); MAX_LINES].join("\n") + "\n";
        assert_eq!(long_sanitized.len(), cmp.len());
        assert_eq!(long_sanitized, cmp);
    }

    #[test]
    fn test_image_validation() {
        let image = ImageParameters {
            untrusted_width: 1,
            untrusted_height: 1,
            untrusted_rowstride: 4,
            untrusted_has_alpha: true,
            untrusted_bits_per_sample: 8,
            untrusted_channels: 4,
            untrusted_data: vec![0, 0, 0, 0],
        };
        let v = serialize_image(image.clone()).unwrap();
        assert_eq!(v.value_signature(), "(iiibiiay)");
        assert_eq!(
            v,
            Value::from((1i32, 1i32, 4i32, true, 8, 4, vec![0u8, 0, 0, 0],))
        );
        assert_eq!(
            serialize_image(ImageParameters {
                untrusted_width: 0,
                ..image.clone()
            })
            .unwrap_err(),
            "Too small width, height, or stride"
        );
        assert_eq!(
            serialize_image(ImageParameters {
                untrusted_height: 0,
                ..image.clone()
            })
            .unwrap_err(),
            "Too small width, height, or stride"
        );
        assert_eq!(
            serialize_image(ImageParameters {
                untrusted_rowstride: 3,
                ..image.clone()
            })
            .unwrap_err(),
            "Too small width, height, or stride"
        );
        assert_eq!(
            serialize_image(ImageParameters {
                untrusted_has_alpha: false,
                ..image.clone()
            })
            .unwrap_err(),
            "Wrong number of channels"
        );
        serialize_image(ImageParameters {
            untrusted_has_alpha: false,
            untrusted_channels: 3,
            ..image.clone()
        })
        .unwrap();
        assert_eq!(
            serialize_image(ImageParameters {
                untrusted_has_alpha: false,
                untrusted_channels: 4,
                ..image.clone()
            })
            .unwrap_err(),
            "Wrong number of channels"
        );

        assert_eq!(
            serialize_image(ImageParameters {
                untrusted_width: MAX_WIDTH + 1,
                ..image.clone()
            })
            .unwrap_err(),
            "Width or height too large"
        );

        assert_eq!(
            serialize_image(ImageParameters {
                untrusted_height: MAX_HEIGHT + 1,
                ..image.clone()
            })
            .unwrap_err(),
            "Width or height too large"
        );

        assert_eq!(
            serialize_image(ImageParameters {
                untrusted_rowstride: 4,
                untrusted_width: 2,
                untrusted_data: vec![0;8],
                ..image.clone()
            })
            .unwrap_err(),
            "Row stride too small"
        );

        assert_eq!(
            serialize_image(ImageParameters {
                untrusted_data: vec![0;3],
                ..image.clone()
            })
            .unwrap_err(),
            "Image too large"
        );
    }
}
