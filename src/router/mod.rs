mod traefik;

use serde_json::Value;

use crate::compose::Rewriter;
use crate::config::{Descriptor, RouterKind};

/// How the services of a bay are reached from outside, without colliding with the main stack.
pub trait Router {
    /// Public hosts a service answers on.
    fn hosts(&self, service: &Value) -> Vec<String>;

    /// Host of the bay replacing a host of the main stack.
    fn bay_host(&self, host: &str, bay: &str) -> String;

    /// Rewrites the routing of a duplicated service so that it does not take over the main one. Returns
    /// warnings about what could not be rewritten, which the bay then does not expose.
    fn rewrite(&self, service: &mut Value, bay: &str, rewriter: &Rewriter) -> Vec<String>;
}

/// Hosts a service answers on, read from its Traefik labels, with no descriptor to tell the domain.
pub fn traefik_hosts(service: &Value) -> Vec<String> {
    traefik::Traefik::new("").hosts(service)
}

pub fn from_descriptor(descriptor: &Descriptor) -> Box<dyn Router> {
    match descriptor.router.kind {
        RouterKind::Traefik => Box::new(traefik::Traefik::new(&descriptor.router.domain)),
    }
}
