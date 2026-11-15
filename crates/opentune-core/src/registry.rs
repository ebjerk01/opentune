//! Runtime registry of available interface drivers, populated by the app.
use crate::interface::Interface;

/// A factory for one interface driver.
pub struct DriverEntry {
    pub id: &'static str,
    pub display_name: &'static str,
    pub make: Box<dyn Fn() -> Box<dyn Interface> + Send + Sync>,
}

/// A set of drivers the application knows how to construct.
#[derive(Default)]
pub struct Registry {
    entries: Vec<DriverEntry>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<F>(&mut self, id: &'static str, display_name: &'static str, make: F)
    where
        F: Fn() -> Box<dyn Interface> + Send + Sync + 'static,
    {
        self.entries.push(DriverEntry {
            id,
            display_name,
            make: Box::new(make),
        });
    }

    pub fn list(&self) -> &[DriverEntry] {
        &self.entries
    }

    pub fn ids(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.entries.iter().map(|e| e.id)
    }

    pub fn make(&self, id: &str) -> Option<Box<dyn Interface>> {
        self.entries.iter().find(|e| e.id == id).map(|e| (e.make)())
    }
}
