//! System clock and UUID-v4 id generator. The default production wiring
//! for [`paperclaw_domain::Clock`] and [`paperclaw_domain::IdGenerator`].

use paperclaw_domain::{Clock, IdGenerator};
use time::OffsetDateTime;
use uuid::Uuid;

/// Reads the OS clock. The default `Clock` for the production binary.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// Mints fresh v4 UUIDs. The default `IdGenerator` for the production
/// binary.
#[derive(Debug, Clone, Copy, Default)]
pub struct UuidV4Generator;

impl IdGenerator for UuidV4Generator {
    fn new_id(&self) -> Uuid {
        Uuid::new_v4()
    }
}
