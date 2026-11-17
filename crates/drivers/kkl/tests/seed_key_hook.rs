//! A registry-style `Box<dyn Interface>` can take a seed/key handler without
//! knowing the concrete driver type.
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::{Interface, Result};

struct Echo;
impl SeedKey for Echo {
    fn id(&self) -> &str {
        "echo"
    }
    fn compute(&self, seed: &[u8], _level: u8) -> Result<Vec<u8>> {
        Ok(seed.to_vec())
    }
}

#[test]
fn boxed_kkl_driver_accepts_a_seed_key() {
    let mut d: Box<dyn Interface> = Box::new(opentune_driver_kkl::KklDriver::new());
    assert!(d.set_seed_key(Box::new(Echo)));
}
