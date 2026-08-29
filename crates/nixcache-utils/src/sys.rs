/// 获取当前环境进程 ID 或伪随机标识
///
/// 在原生操作系统平台下返回 `std::process::id()`；
/// 在 wasm32 等沙箱无进程环境下返回 0 作为抖动基数。
pub fn get_process_id() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        0u64
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        std::process::id() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::get_process_id;

    #[test]
    fn test_get_process_id_consistency() {
        let pid1 = get_process_id();
        let pid2 = get_process_id();
        assert_eq!(pid1, pid2);
    }
}
