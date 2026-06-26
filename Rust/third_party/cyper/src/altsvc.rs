//! Vendored and modified from `altsvc` crate.

use std::{collections::HashMap, time::Instant};

use thiserror::Error;

use crate::sync::{mutex_blocking::Mutex, shared::Shared};

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("invalid parameter: {0}")]
    Parameter(String),
    #[error("invalid value of 'ma': {0}")]
    MaValue(String),
    #[error("invalid value of 'persist': {0}")]
    PersistValue(String),
    #[error("invalid value of 'alt-authority': {0}")]
    AltAuthorityValue(String),
    #[error("invalid port number: {0}")]
    PortNumber(String),
}

#[derive(Debug)]
pub enum AltService {
    Clear,
    Services(Vec<Service>),
}

#[derive(Debug, Default)]
pub struct Service {
    id: String,
    authority: AltAuthority,
    max_age: Option<u64>,
    persist: bool,
}

#[derive(Debug, Default)]
pub struct AltAuthority {
    pub host: String,
    pub port: u16,
}

pub fn parse(s: &str) -> Result<AltService, ParseError> {
    if s == "clear" {
        return Ok(AltService::Clear);
    }

    let mut ret = Vec::new();
    let services = s.split(",");
    for svc_string in services {
        let svc_string = svc_string.trim();
        if svc_string.is_empty() {
            continue;
        }
        let mut svc = Service::default();
        let params = svc_string.split(";");
        for kv in params {
            let raw_kv = kv.trim();
            if raw_kv.is_empty() {
                continue;
            }
            let kv_split: Vec<&str> = raw_kv.split("=").collect();
            if kv_split.len() != 2 {
                return Err(ParseError::Parameter(raw_kv.to_string()));
            }
            let k = kv_split[0];
            let v = kv_split[1];
            match k {
                "ma" => {
                    let ma = v.parse().map_err(|_| ParseError::MaValue(v.to_string()))?;
                    svc.max_age = Some(ma);
                }
                "persist" => {
                    let persist = v
                        .parse::<i32>()
                        .map_err(|_| ParseError::PersistValue(v.to_string()))?;
                    if persist != 1 {
                        continue;
                    }
                    svc.persist = true;
                }
                _ => {
                    let raw_value = v.trim_matches('"');
                    let alt_auth_split: Vec<&str> = raw_value.split(":").collect();
                    if alt_auth_split.len() != 2 {
                        return Err(ParseError::AltAuthorityValue(raw_value.to_string()));
                    }
                    svc.id = k.to_string();
                    let host = alt_auth_split[0].to_string();
                    let port = alt_auth_split[1].to_string();
                    let port = port
                        .parse()
                        .map_err(|_| ParseError::PortNumber(port.to_string()))?;
                    svc.authority = AltAuthority { host, port };
                }
            }
        }
        ret.push(svc);
    }
    Ok(AltService::Services(ret))
}

#[derive(Debug)]
struct AltHostEntry {
    insert_time: Instant,
    max_age: u64,
}

#[derive(Debug, Clone)]
pub struct KnownHosts {
    map: Shared<Mutex<HashMap<String, AltHostEntry>>>,
    /// Vast patch (h3→h2 fallback): hosts whose h3 transport FAILED, negative-cached until the stored
    /// `Instant`. While quarantined, `try_insert` is a no-op for the host — the very next h2 response's
    /// `Alt-Svc: h3` header must not instantly re-arm a broken (e.g. UDP-blocked) h3 path, which would
    /// otherwise make every other request pay a failed multi-second QUIC handshake.
    quarantine: Shared<Mutex<HashMap<String, Instant>>>,
}

impl Default for KnownHosts {
    fn default() -> Self {
        Self {
            map: Shared::new(Mutex::new(HashMap::new())),
            quarantine: Shared::new(Mutex::new(HashMap::new())),
        }
    }
}

impl KnownHosts {
    pub fn try_insert(&self, host: &str, srv: &crate::altsvc::Service) -> bool {
        if self.quarantined(host) {
            // Vast patch: a quarantined host's advertisement is REJECTED (return true so the caller stops
            // scanning alternatives — the advertisement was understood, just deliberately not honored).
            return srv.id == "h3";
        }
        if srv.id == "h3"
            && (srv.authority.host.is_empty() || srv.authority.host == host)
            && srv.authority.port == 443
        {
            self.map.lock().insert(
                host.to_string(),
                AltHostEntry {
                    insert_time: Instant::now(),
                    max_age: srv.max_age.unwrap_or(86400), // 24 hours
                },
            );
            return true;
        }
        false
    }

    pub fn find(&self, host: &str) -> bool {
        let mut map = self.map.lock();
        if let Some((host, entry)) = map.remove_entry(host)
            && (Instant::now() - entry.insert_time).as_secs() <= entry.max_age
        {
            map.insert(host, entry);
            return true;
        }
        false
    }

    pub fn clear(&self, host: &str) {
        self.map.lock().remove(host);
    }

    /// Vast patch (h3→h2 fallback): drop the host's h3 entry AND negative-cache it until `until`, so the
    /// h3 path is not re-armed by routine `Alt-Svc` headers until the quarantine lapses (one re-probe per
    /// window instead of one failed QUIC handshake per request on a UDP-blocked network).
    pub fn quarantine_host(&self, host: &str, until: Instant) {
        self.map.lock().remove(host);
        self.quarantine.lock().insert(host.to_string(), until);
    }

    /// True while `host` is inside its failure quarantine; expired entries are reclaimed on probe.
    fn quarantined(&self, host: &str) -> bool {
        let mut q = self.quarantine.lock();
        match q.get(host) {
            Some(&until) if Instant::now() < until => true,
            Some(_) => {
                q.remove(host);
                false
            }
            None => false,
        }
    }
}
