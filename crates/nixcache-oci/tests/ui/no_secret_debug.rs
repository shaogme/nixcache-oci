use nixcache_oci::{GitHubPackagesClient, MockRouterTransport, OciClient, TokenManager};

fn requires_debug<T: std::fmt::Debug>() {}

fn main() {
    requires_debug::<TokenManager>();
    requires_debug::<OciClient<MockRouterTransport>>();
    requires_debug::<GitHubPackagesClient<MockRouterTransport>>();
}
