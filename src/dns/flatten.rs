// Constellation
//
// Pluggable authoritative DNS server
// Copyright: 2020, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

use std::ops::Deref;
use std::collections::HashMap;
use std::thread;
use std::sync::RwLock;
use std::time::{SystemTime, Duration, Instant};
use trust_dns_resolver::Resolver;
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::error::ResolveError;

use super::record::{RecordValues, RecordValue, RecordType};

lazy_static! {
    pub static ref DNS_BOOTSTRAP: RwLock<HashMap<DNSFlattenRegistryKey, u32>> = RwLock::new(HashMap::new());
    pub static ref DNS_FLATTEN: DNSFlatten = DNSFlattenBuilder::new();
}

struct DNSFlattenBuilder;

pub struct DNSFlatten {
    registry: RwLock<HashMap<DNSFlattenRegistryKey, DNSFlattenEntry>>,
    resolver: Resolver,
}

pub struct DNSFlattenBootstrapBuilder;
pub struct DNSFlattenBootstrap;

pub struct DNSFlattenMaintainBuilder;
pub struct DNSFlattenMaintain;

type DNSFlattenRegistryKey = (RecordValue, RecordType);

const MAINTAIN_EXPIRE_TTL_RATIO: u32 = 10;
const MAINTAIN_PERFORM_INTERVAL: Duration = Duration::from_secs(60);
const BOOTSTRAP_TICK_INTERVAL: Duration = Duration::from_millis(100);

struct DNSFlattenEntry {
    values: RecordValues,
    ttl: u32,
    refreshed_at: SystemTime,
    accessed_at: SystemTime,
}

impl DNSFlattenBuilder {
    fn new() -> DNSFlatten {
        // Acquire a resolver (prefer using system resolver)
        let resolver = if let Ok(resolver) = Resolver::from_system_conf() {
            info!("dns flatten resolver acquired from system");

            resolver
        } else {
            warn!("dns flatten resolver could not be acquired from system, using default resolver");

            Resolver::new(ResolverConfig::default(), ResolverOpts::default()).unwrap()
        };

        DNSFlatten {
            registry: RwLock::new(HashMap::new()),
            resolver: resolver,
        }
    }
}

impl DNSFlattenBootstrapBuilder {
    pub fn new() -> DNSFlattenBootstrap {
        DNSFlattenBootstrap {}
    }
}

impl DNSFlattenMaintainBuilder {
    pub fn new() -> DNSFlattenMaintain {
        // Ensure static is valid and has been built
        let _ = DNS_FLATTEN.deref();

        DNSFlattenMaintain {}
    }
}

impl DNSFlatten {
    pub fn pass(&self, record_type: RecordType, record_value: RecordValue, record_ttl: u32) -> Result<RecordValues, ()> {
        debug!("flatten registry pass on value: {:?} and type: {:?}", record_value, record_type);

        // Acquire registry key
        let registry_key = (record_value, record_type);

        // Acquire flattened value from cache (if any)
        // Notice: this is nested in a sub-block as to ensure no rw-lock dead-lock can occur due \
        //   later use of the same lock from this block level.
        let cached_value = {
            // Acquire registry write pointer
            let mut registry_write = self.registry.write().unwrap();

            if let Some(ref mut registry_record) = registry_write.get_mut(&registry_key) {
                debug!("flattening from local registry on value: {:?} and type: {:?}", registry_key.0, registry_key.1);

                // Bump last access time
                registry_record.accessed_at = SystemTime::now();

                Some(registry_record.values.to_owned())
            } else {
                None
            }
        };

        // Return cached value, or queue flatten order?
        if let Some(value) = cached_value {
            Ok(value)
        } else {
            info!("flattening from network on value: {:?} and type: {:?}", registry_key.0, registry_key.1);

            self.queue(&registry_key, record_ttl)
        }
    }

    fn queue(&self, registry_key: &DNSFlattenRegistryKey, ttl: u32) -> Result<RecordValues, ()> {
        // Acquire registry write pointer
        let mut bootstrap_write = DNS_BOOTSTRAP.write().unwrap();

        // Stack flatten order to queue (will be picked up by worker thread ASAP)
        bootstrap_write.insert(registry_key.to_owned(), ttl);

        // Send back an error, as we do not have the flat value at this point in time
        // Notice: this will propagate a 'SERVFAIL', which ensures resolvers do not cache the \
        //   empty response.
        Err(())
    }

    fn flatten(&self, registry_key: &DNSFlattenRegistryKey, ttl: u32, accessed_at: Option<SystemTime>) {
        // Convert each value type into its string representation
        let values: Result<Vec<String>, ResolveError> = match registry_key.1 {
            RecordType::A => {
                self.resolver.ipv4_lookup(&registry_key.0).map(|values| {
                    values.iter().map(|value| value.to_string()).collect()
                })
            },
            RecordType::AAAA => {
                self.resolver.ipv6_lookup(&registry_key.0).map(|values| {
                    values.iter().map(|value| value.to_string()).collect()
                })
            },
            RecordType::MX => {
                // Format as `{priority} {exchange}`, eg. `10 inbound.crisp.email`
                self.resolver.mx_lookup(&registry_key.0).map(|values| {
                    values.iter().map(|value| {
                        format!("{} {}", value.preference(), value.exchange())
                    }).collect()
                })
            },
            RecordType::TXT => {
                // Assemble all TXT data segments
                self.resolver.txt_lookup(&registry_key.0).map(|values| {
                    values.iter().map(|value| value.txt_data().join("")).collect()
                })
            },
            RecordType::PTR | RecordType::CNAME => Ok(Vec::new()),
        };

        // Return final flattened record values
        let results = if let Ok(values) = values {
            Ok(RecordValues::from_list(values.into_iter().map(|value| {
                RecordValue::from_string(value)
            }).collect()))
        } else {
            Err(())
        };

        // Acquire registry write pointer
        let mut registry_write = self.registry.write().unwrap();

        // Error was acquired, and a flattened records already exist in registry?
        // Notice: this prevents in-error refreshes to empty the registry where it previously \
        //   had records, effectively corrupting the DNS system.
        if results.is_err() && registry_write.contains_key(registry_key) {
            warn!("dns flattening in error on value: {:?} and type: {:?}, keeping old cache", registry_key.0, registry_key.1);
        } else {
            // Store flattened values to registry
            registry_write.insert(
                registry_key.to_owned(),

                DNSFlattenEntry::new(
                    results.unwrap_or(RecordValues::new()), ttl, accessed_at
                )
            );
        }
    }
}

impl DNSFlattenBootstrap {
    pub fn run(&self) {
        info!("dns flattener bootstrap is now active");

        loop {
            // Hold for next tick run
            thread::sleep(BOOTSTRAP_TICK_INTERVAL);

            Self::tick();
        }
    }

    fn tick() {
        let mut bootstrap_register: Vec<(DNSFlattenRegistryKey, u32)> = Vec::new();

        // Scan for items to be bootstrapped
        {
            let bootstrap_read = DNS_BOOTSTRAP.read().unwrap();

            for (bootstrap_key, bootstrap_ttl) in bootstrap_read.iter() {
                bootstrap_register.push((bootstrap_key.to_owned(), *bootstrap_ttl));
            }
        }

        // Proceed bootstrapping items
        if bootstrap_register.is_empty() == false {
            for (bootstrap_key, bootstrap_ttl) in bootstrap_register.iter() {
                DNS_FLATTEN.flatten(bootstrap_key, *bootstrap_ttl, None);
                DNS_BOOTSTRAP.write().unwrap().remove(bootstrap_key);
            }

            debug!(
                "bootstrapped dns flattened records (count: {})",
                bootstrap_register.len()
            );
        }
    }
}

impl DNSFlattenMaintain {
    pub fn run(&self) {
        info!("dns flattener maintain is now active");

        loop {
            // Hold for next perform run
            thread::sleep(MAINTAIN_PERFORM_INTERVAL);

            debug!("running a dns flattener maintain tick...");

            let flush_start = Instant::now();

            // #1: Flush expired flattened entries
            Self::expire();

            // #2: Refresh flattened entries
            Self::refresh();

            let flush_took = flush_start.elapsed();

            info!(
                "ran dns flattener maintain tick (took {}s + {}ms)",
                flush_took.as_secs(),
                flush_took.subsec_millis()
            );
        }
    }

    fn expire() {
        debug!("flushing expired dns flattened records");

        let mut expire_register: Vec<DNSFlattenRegistryKey> = Vec::new();

        // Scan for expired registry items
        {
            let registry_read = DNS_FLATTEN.registry.read().unwrap();
            let now_time = SystemTime::now();

            for (registry_key, registry_entry) in registry_read.iter() {
                let registry_elapsed = now_time
                    .duration_since(registry_entry.accessed_at)
                    .unwrap()
                    .as_secs();

                if registry_elapsed >= (registry_entry.ttl * MAINTAIN_EXPIRE_TTL_RATIO) as u64 {
                    expire_register.push(registry_key.to_owned());
                }
            }
        }

        // Any registry item to expire?
        if expire_register.is_empty() == false {
            let mut registry_write = DNS_FLATTEN.registry.write().unwrap();

            for registry_key in &expire_register {
                registry_write.remove(registry_key);
            }
        }

        info!(
            "flushed expired dns flattened records (count: {})",
            expire_register.len()
        );
    }

    fn refresh() {
        debug!("refreshing dns flattened records");

        let mut refresh_register: Vec<(DNSFlattenRegistryKey, u32, SystemTime)> = Vec::new();

        // Scan for to-be-refreshed registry items
        {
            let registry_read = DNS_FLATTEN.registry.read().unwrap();
            let now_time = SystemTime::now();

            for (registry_key, registry_entry) in registry_read.iter() {
                let registry_elapsed = now_time
                    .duration_since(registry_entry.refreshed_at)
                    .unwrap()
                    .as_secs();

                if registry_elapsed >= registry_entry.ttl as u64 {
                    refresh_register.push((registry_key.to_owned(), registry_entry.ttl, registry_entry.accessed_at));
                }
            }
        }

        // Any registry item to refresh?
        if refresh_register.is_empty() == false {
            for (registry_key, registry_ttl, registry_accessed_at) in &refresh_register {
                // Notice: restore 'accessed_at' time, otherwise a never-accessed registry entry \
                //   would never be expired.
                DNS_FLATTEN.flatten(&registry_key, *registry_ttl, Some(*registry_accessed_at));
            }
        }

        debug!(
            "refreshed dns flattened records (count: {})",
            refresh_register.len()
        );
    }
}

impl DNSFlattenEntry {
    fn new(values: RecordValues, ttl: u32, accessed_at: Option<SystemTime>) -> DNSFlattenEntry {
        let time_now = SystemTime::now();

        DNSFlattenEntry {
            values: values,
            ttl: ttl,
            refreshed_at: time_now,
            accessed_at: accessed_at.unwrap_or(time_now),
        }
    }
}
