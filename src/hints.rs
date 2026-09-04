use crate::ImageParameters;
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use std::collections::HashMap;
use zbus::zvariant::{DeserializeValue, Signature, Type, Value};

const IMAGE_DATA: &str = "image-data";

pub struct Hints<'a> {
    pub image: Option<ImageParameters>,
    pub other: HashMap<String, Value<'a>>,
}

impl Type for Hints<'_> {
    const SIGNATURE: &'static Signature = <HashMap<String, Value<'static>> as Type>::SIGNATURE;
}

impl<'de> Deserialize<'de> for Hints<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(HintsVisitor)
    }
}

struct HintsVisitor;

impl<'de> Visitor<'de> for HintsVisitor {
    type Value = Hints<'de>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a dictionary of notification hints")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Hints<'de>, M::Error> {
        let mut hints = Hints {
            image: None,
            other: HashMap::new(),
        };
        while let Some(key) = map.next_key::<String>()? {
            if key == IMAGE_DATA {
                let image = map.next_value::<DeserializeValue<'de, ImageParameters>>()?.0;
                hints.image = Some(image);
            } else {
                let value = map.next_value::<Value<'de>>()?;
                hints.other.insert(key, value);
            }
        }
        Ok(hints)
    }
}
