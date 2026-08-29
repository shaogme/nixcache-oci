use std::future::Future;

/// 同步配置解析转换 Trait
pub trait Resolve {
    type Output;
    type Error;

    fn resolve(self) -> Result<Self::Output, Self::Error>;
}

/// 异步配置解析转换 Trait（适用于需要异步 Token 探测或环境探测的场景）
pub trait AsyncResolve {
    type Output;
    type Error;

    fn resolve(self) -> impl Future<Output = Result<Self::Output, Self::Error>> + Send;
}
