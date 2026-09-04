use bitflags::bitflags;
use futures_util::TryFutureExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;
use zbus::{
    proxy,
    fdo::{DBusProxy, NameOwnerChangedStream},
    zvariant::Type,
    zvariant::Value,
    Connection,
};
mod maps;
use maps::{GuestId, HostId, Maps};
mod badge;
pub use badge::Image;
mod icon;
pub use icon::resolve_image_path;
mod hints;
pub use hints::Hints;
#[proxy(
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
    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> Result<()>;
    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> Result<()>;
    // Non-standard KDE extension
    #[zbus(signal)]
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
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b':' => {}
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
pub const MAX_WIDTH: i32 = 512;
pub const MAX_HEIGHT: i32 = 512;
pub(crate) const MAX_APP_IMAGE_SIDE: u32 = 2048;

/// Images longer than this on a side are scaled down before they are sent.
pub(crate) const MAX_ICON_SIDE: u32 = if MAX_WIDTH < MAX_HEIGHT {
    MAX_WIDTH as u32
} else {
    MAX_HEIGHT as u32
};

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

/// Tightly packed RGBA8, the one layout this crate itself produces:
/// rowstride is exactly `width * 4`.
impl From<badge::Image> for ImageParameters {
    fn from(image: badge::Image) -> Self {
        ImageParameters {
            untrusted_width: image.width as i32,
            untrusted_height: image.height as i32,
            untrusted_rowstride: image.width as i32 * 4,
            untrusted_has_alpha: true,
            untrusted_bits_per_sample: 8,
            untrusted_channels: 4,
            untrusted_data: image.data,
        }
    }
}

const MAX_LINES: usize = 500;
const MAX_CHARS_PER_LINE: usize = 1000;

fn validate_guest_image(untrusted_image: ImageParameters) -> Result<badge::Image, &'static str> {
    unpack_image(untrusted_image, MAX_WIDTH, MAX_HEIGHT, MAX_SIZE)
}

fn unpack_image(
    untrusted_image: ImageParameters,
    max_width: i32,
    max_height: i32,
    max_bytes: usize,
) -> Result<badge::Image, &'static str> {
    let ImageParameters {
        untrusted_width,
        untrusted_height,
        untrusted_rowstride,
        untrusted_has_alpha,
        untrusted_bits_per_sample,
        untrusted_channels,
        untrusted_data,
    } = untrusted_image;
    // booleans do not need to be sanitized
    let has_alpha = untrusted_has_alpha;

    // bits per sample must be 8
    if untrusted_bits_per_sample != 8 {
        return Err("Wrong number of bits per sample");
    }

    // data cannot be too long
    if untrusted_data.len() > max_bytes {
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
    if untrusted_width > max_width || untrusted_height > max_height {
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

    let mut image = badge::Image::new(width as u32, height as u32);
    for (y, row) in data.chunks_exact(rowstride as usize).take(height as usize).enumerate() {
        for (x, px) in row.chunks_exact(channels as usize).take(width as usize).enumerate() {
            let alpha = if has_alpha { px[3] } else { 0xFF };
            image.set_pixel(x as u32, y as u32, [px[0], px[1], px[2], alpha]);
        }
    }
    Ok(image)
}

pub fn fit_app_image(untrusted_image: ImageParameters) -> Option<ImageParameters> {
    let (claimed_width, claimed_height) = (
        untrusted_image.untrusted_width,
        untrusted_image.untrusted_height,
    );
    let side = MAX_APP_IMAGE_SIDE as i32;
    let bytes = (MAX_APP_IMAGE_SIDE as usize) * (MAX_APP_IMAGE_SIDE as usize) * 4;
    match unpack_image(untrusted_image, side, side, bytes) {
        Ok(image) => Some(ImageParameters::from(badge::shrink_to(image, MAX_ICON_SIDE))),
        Err(e) => {
            eprintln!(
                "Dropping unusable image from application: {e} \
                (claimed {claimed_width}x{claimed_height})"
            );
            None
        }
    }
}

#[cfg(feature = "unicode")]
#[must_use]
fn validate_code_point(code_point: u32) -> bool {
    #[link(kind = "dylib", name = ":libqubes-pure.so.0")]
    extern "C" {
        fn qubes_pure_code_point_safe_for_display(code_point: u32) -> bool;
    }
    // SAFETY: this function is not actually unsafe
    unsafe { qubes_pure_code_point_safe_for_display(code_point) }
}
#[cfg(not(feature = "unicode"))]
#[must_use]
fn validate_code_point(code_point: u32) -> bool {
    match code_point {
        0x20 ..= 0x7E => true,
        _ => false,
    }
}

fn qubesd_client(method: &str, dest: &str) -> Result<Vec<u8>, std::io::Error> {
    if Path::new("/usr/bin/qrexec-client-vm").exists() {
        let output = Command::new("/usr/bin/qrexec-client-vm")
            .arg(dest)
            .arg(method)
            .output()
            .expect("failed to execute qrexec-client-vm");
        if output.status.success() {
            return Ok(output.stdout);
        } else {
            return Err(std::io::Error::other(format!(
                "Admin API call failed: exit code {}",
                output.status.code().unwrap()
            )));
        }
    } else {
        let mut qubesd = UnixStream::connect("/run/qubesd.sock")?;
        qubesd.write_all(format!("{method} dom0 name {dest}\0").as_bytes())?;
        qubesd.shutdown(Shutdown::Write)?;
        let mut response = Vec::<u8>::new();
        qubesd.read_to_end(&mut response)?;
        return Ok(response);
    }
}

fn qubesd_value(method: &str, dest: &str) -> Result<String, std::io::Error> {
    let answer = qubesd_client(method, dest)?;
    match answer.get(0..2) {
        Some([b'0', 0]) => {
            let payload = match String::from_utf8(answer[2..].to_vec()) {
                Ok(payload) => payload,
                Err(e) => {
                    return Err(std::io::Error::other(format!(
                        "Admin API returned non-UTF-8: {e}"
                    )))
                }
            };
            Ok(payload.trim().rsplit(' ').next().unwrap().to_owned())
        }
        _ => Err(std::io::Error::other(format!(
            "Admin API call failed: {answer:?}"
        ))),
    }
}

pub fn qube_icon(name: &str) -> Result<String, std::io::Error> {
    qubesd_value("admin.vm.property.Get+icon", name)
}

pub fn qube_mark(icon_name: &str) -> Result<badge::Image, std::io::Error> {
    match icon::qube_icon_image(icon_name) {
        Some(icon) => Ok(badge::fit_mark(&icon)),
        None => Err(std::io::Error::other(format!(
            "cannot load icon {icon_name:?} or its appvm fallback from the icon theme"
        ))),
    }
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
            if validate_code_point(c.into()) || c == '\t' {
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
    #[derive(Default, Clone)]
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
    default_icon: String,
    mark: Option<badge::Image>,
    maps: std::cell::RefCell<Maps>,
}

impl NotificationEmitter {
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }
    pub async fn new(
        prefix: String,
        application_name: String,
        default_icon: String,
        mark: Option<badge::Image>,
    ) -> zbus::Result<(Self, NameOwnerChangedStream)> {
        let connection = Connection::session().await?;
        let (proxy, notification_proxy) = futures_util::future::join(
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
        let (proxy, (notification_proxy, capabilities_list)) =
            (proxy?, notification_proxy?);
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
                default_icon,
                mark,
                maps: Default::default(),
            },
            proxy,
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
    pub async fn closed(&self) -> zbus::Result<NotificationClosedStream> {
        self.notification_proxy.receive_notification_closed().await
    }
    pub async fn invocations(&self) -> zbus::Result<ActionInvokedStream> {
        self.notification_proxy.receive_action_invoked().await
    }
    pub async fn replies(&self) -> zbus::Result<NotificationRepliedStream> {
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
        HostId::new_less_safe(id)
            .and_then(|a| self.maps.borrow_mut().remove_host_id(a).map(From::from))
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
            Some(id) => self.maps.borrow().lookup_guest_id(id),
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
        // the qube icon.
        let icon = self.default_icon.clone();
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
        // Without a mark the image is dropped; the server already said so
        // at startup.
        if let (Some(untrusted_image), Some(mark)) = (image, self.mark.as_ref()) {
            let untrusted_width = untrusted_image.untrusted_width;
            let untrusted_height = untrusted_image.untrusted_height;
            match validate_guest_image(untrusted_image) {
                Ok(base) => {
                    let marked = ImageParameters::from(badge::compose(&base, mark));
                    hints.insert("image-data", Value::from(marked));
                }
                Err(e) => eprintln!(
                    "Ignoring unusable image from guest: {e} \
                    (claimed {untrusted_width}x{untrusted_height})"
                ),
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
                    &icon,
                    &*(self.prefix.clone() + &*sanitize_str(&*untrusted_summary)),
                    &*escaped_body,
                    &*actions,
                    &hints,
                    expire_timeout,
                )
                .await?,
        )
        .expect("Notification daemon sent a zero ID?");

        Ok(self.maps.borrow_mut().next_id(id, guest_id))
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

    /// Firefox sends 256x256 icons for web notifications.
    #[test]
    fn accepts_web_notification_icon_sizes() {
        let img = validate_guest_image(ImageParameters {
            untrusted_width: 256,
            untrusted_height: 256,
            untrusted_rowstride: 1024,
            untrusted_has_alpha: true,
            untrusted_bits_per_sample: 8,
            untrusted_channels: 4,
            untrusted_data: vec![0; 256 * 1024],
        })
        .unwrap();
        assert_eq!((img.width, img.height), (256, 256));
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
        let img = validate_guest_image(image.clone()).unwrap();
        // Unpacked at its own size; normalising is `badge::compose`'s job.
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(
            Value::from(ImageParameters::from(img)).value_signature(),
            "(iiibiiay)"
        );

        // A multi-row image with a padded row stride: the pad bytes must be
        // skipped so every unpacked pixel is the real one.  This is the case
        // the chunks_exact unpack exists for, and 1x1 tests never reach it.
        let padded = validate_guest_image(ImageParameters {
            untrusted_width: 2,
            untrusted_height: 2,
            untrusted_rowstride: 8, // 2px * 3ch = 6, plus 2 pad bytes
            untrusted_has_alpha: false,
            untrusted_channels: 3,
            untrusted_data: vec![
                1, 2, 3, 4, 5, 6, 99, 99, // row 0: two pixels then padding
                7, 8, 9, 10, 11, 12, 99, 99, // row 1
            ],
            ..image.clone()
        })
        .unwrap();
        assert_eq!((padded.width, padded.height), (2, 2));
        assert_eq!(padded.pixel(0, 0), [1, 2, 3, 0xFF], "first pixel, alpha filled");
        assert_eq!(padded.pixel(1, 0), [4, 5, 6, 0xFF], "second pixel, not the pad");
        assert_eq!(padded.pixel(0, 1), [7, 8, 9, 0xFF], "next row starts after pad");
        assert_eq!(padded.pixel(1, 1), [10, 11, 12, 0xFF]);

        assert_eq!(
            validate_guest_image(ImageParameters {
                untrusted_width: 0,
                ..image.clone()
            })
            .unwrap_err(),
            "Too small width, height, or stride"
        );
        assert_eq!(
            validate_guest_image(ImageParameters {
                untrusted_height: 0,
                ..image.clone()
            })
            .unwrap_err(),
            "Too small width, height, or stride"
        );
        assert_eq!(
            validate_guest_image(ImageParameters {
                untrusted_rowstride: 3,
                ..image.clone()
            })
            .unwrap_err(),
            "Too small width, height, or stride"
        );
        assert_eq!(
            validate_guest_image(ImageParameters {
                untrusted_has_alpha: false,
                ..image.clone()
            })
            .unwrap_err(),
            "Wrong number of channels"
        );
        validate_guest_image(ImageParameters {
            untrusted_has_alpha: false,
            untrusted_channels: 3,
            ..image.clone()
        })
        .unwrap();
        assert_eq!(
            validate_guest_image(ImageParameters {
                untrusted_has_alpha: false,
                untrusted_channels: 4,
                ..image.clone()
            })
            .unwrap_err(),
            "Wrong number of channels"
        );

        assert_eq!(
            validate_guest_image(ImageParameters {
                untrusted_width: MAX_WIDTH + 1,
                ..image.clone()
            })
            .unwrap_err(),
            "Width or height too large"
        );

        assert_eq!(
            validate_guest_image(ImageParameters {
                untrusted_height: MAX_HEIGHT + 1,
                ..image.clone()
            })
            .unwrap_err(),
            "Width or height too large"
        );

        assert_eq!(
            validate_guest_image(ImageParameters {
                untrusted_rowstride: 4,
                untrusted_width: 2,
                untrusted_data: vec![0; 8],
                ..image.clone()
            })
            .unwrap_err(),
            "Row stride too small"
        );

        assert_eq!(
            validate_guest_image(ImageParameters {
                untrusted_data: vec![0; 3],
                ..image.clone()
            })
            .unwrap_err(),
            "Image too large"
        );
    }
}
