use clap::Args;
use nixcache_utils::Env;
use std::net::{AddrParseError, SocketAddr};

pub const DEFAULT_SERVER_LISTEN: &str = "127.0.0.1";
pub const DEFAULT_SERVER_PORT: u16 = 37515;

/// 网络监听与绑定参数组
#[derive(Args, Debug, Clone, Default)]
pub struct ServerBindArgs {
    #[arg(long, help = "Address to listen on [env: NIXCACHE_LISTEN]")]
    pub listen: Option<String>,

    #[arg(long, help = "Port to listen on [env: NIXCACHE_PORT]")]
    pub port: Option<u16>,
}

impl ServerBindArgs {
    /// 解析监听地址（默认 127.0.0.1）
    pub fn resolve_listen(&self) -> String {
        self.listen
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_LISTEN"))
            .unwrap_or_else(|| DEFAULT_SERVER_LISTEN.to_string())
    }

    /// 解析监听端口（默认 37515）
    pub fn resolve_port(&self) -> u16 {
        self.port
            .or_else(|| Env::parse("NIXCACHE_PORT"))
            .unwrap_or(DEFAULT_SERVER_PORT)
    }

    /// 同时解析 (listen, port)
    pub fn resolve(&self, default_listen: &str, default_port: u16) -> (String, u16) {
        let listen = self
            .listen
            .as_deref()
            .and_then(Env::non_empty_str)
            .map(|s| s.to_string())
            .or_else(|| Env::get("NIXCACHE_LISTEN"))
            .unwrap_or_else(|| default_listen.to_string());

        let port = self
            .port
            .or_else(|| Env::parse("NIXCACHE_PORT"))
            .unwrap_or(default_port);

        (listen, port)
    }

    /// 解析为 SocketAddr
    pub fn socket_addr(&self) -> Result<SocketAddr, AddrParseError> {
        let (listen, port) = self.resolve(DEFAULT_SERVER_LISTEN, DEFAULT_SERVER_PORT);
        format!("{}:{}", listen, port).parse()
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SERVER_LISTEN, DEFAULT_SERVER_PORT, ServerBindArgs};
    use std::env;

    #[test]
    fn test_server_bind_resolution() {
        let empty = ServerBindArgs::default();
        let (l, p) = empty.resolve(DEFAULT_SERVER_LISTEN, DEFAULT_SERVER_PORT);
        assert_eq!(l, DEFAULT_SERVER_LISTEN);
        assert_eq!(p, DEFAULT_SERVER_PORT);

        let explicit = ServerBindArgs {
            listen: Some("0.0.0.0".to_string()),
            port: Some(8080),
        };
        let (l, p) = explicit.resolve(DEFAULT_SERVER_LISTEN, DEFAULT_SERVER_PORT);
        assert_eq!(l, "0.0.0.0");
        assert_eq!(p, 8080);

        unsafe {
            env::set_var("NIXCACHE_LISTEN", "192.168.1.100");
            env::set_var("NIXCACHE_PORT", "9999");
        }
        let env_res = empty.resolve(DEFAULT_SERVER_LISTEN, DEFAULT_SERVER_PORT);
        assert_eq!(env_res.0, "192.168.1.100");
        assert_eq!(env_res.1, 9999);

        unsafe {
            env::remove_var("NIXCACHE_LISTEN");
            env::remove_var("NIXCACHE_PORT");
        }
    }
}
