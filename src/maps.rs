use core::num::NonZeroU32;
use std::collections::BTreeMap;

#[derive(Copy, Clone)]
#[repr(transparent)]
pub struct HostId(NonZeroU32);
#[repr(transparent)]
#[derive(Copy, Clone)]
pub struct GuestId(NonZeroU32);

impl HostId {
    pub fn new_less_safe(v: u32) -> Option<Self> {
        NonZeroU32::new(v).map(Self)
    }
}

impl GuestId {
    pub fn new_less_safe(v: u32) -> Option<Self> {
        NonZeroU32::new(v).map(Self)
    }
}

impl From<GuestId> for u32 {
    fn from(value: GuestId) -> Self {
        value.0.into()
    }
}

impl From<HostId> for u32 {
    fn from(value: HostId) -> Self {
        value.0.into()
    }
}

pub(super) struct Maps {
    guest_to_host_map: BTreeMap<NonZeroU32, NonZeroU32>,
    host_to_guest_map: BTreeMap<NonZeroU32, NonZeroU32>,
    last_id: NonZeroU32,
}

impl Default for Maps {
    fn default() -> Self {
        Self {
            guest_to_host_map: Default::default(),
            host_to_guest_map: Default::default(),
            last_id: 1.try_into().expect("constant value"),
        }
    }
}

fn next(t: NonZeroU32) -> NonZeroU32 {
    match t.into() {
        u32::MAX => 1,
        e => e + 1,
    }
    .try_into()
    .expect("always produces nonzero value")
}

impl Maps {
    pub(super) fn next_id(&mut self, id: HostId, guest_id: Option<GuestId>) -> GuestId {
        if let Some(guest_id) = guest_id {
            self.guest_to_host_map.insert(guest_id.0, id.0);
            self.host_to_guest_map.insert(id.0, guest_id.0);
            return guest_id;
        }
        self.last_id = next(self.last_id);
        while self.guest_to_host_map.contains_key(&self.last_id) {
            self.last_id = next(self.last_id);
        }
        let last_id = self.last_id;
        eprintln!("Next ID is {last_id}, mapping to host ID {}", id.0);
        assert!(self
            .guest_to_host_map
            .insert(last_id, id.0.into())
            .is_none());
        assert!(
            self.host_to_guest_map
                .insert(id.0.into(), last_id)
                .is_none(),
            "notification daemon reused an ID without telling us"
        );
        GuestId(
            last_id
                .try_into()
                .expect("last ID set to a nonzero value above"),
        )
    }

    pub(super) fn lookup_guest_id(&self, id: GuestId) -> Option<HostId> {
        self.guest_to_host_map.get(&id.0.into()).map(|&e| HostId(e))
    }

    pub(super) fn lookup_host_id(&self, id: HostId) -> Option<GuestId> {
        self.host_to_guest_map
            .get(&id.0.into())
            .map(|&e| GuestId(e))
    }

    pub(super) fn remove_host_id(&mut self, id: HostId) -> Option<GuestId> {
        self.host_to_guest_map.remove(&id.0.into()).map(|g| {
            assert_eq!(self.guest_to_host_map.remove(&g.into()), id.0.into());
            GuestId(g)
        })
    }

    pub(super) fn clear(&mut self) {
        self.guest_to_host_map.clear();
        self.host_to_guest_map.clear();
    }
}
