use std::{
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
    str::FromStr,
};

/// 环境变量读取与清洗工具结构体
pub struct Env;

impl Env {
    /// 检查并过滤非空字符串切片，去除首尾空白字符后若非空则返回
    pub fn non_empty_str(s: &str) -> Option<&str> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    /// 检查并过滤非空路径引用
    pub fn non_empty_path(p: &Path) -> Option<&Path> {
        if p.as_os_str().is_empty() {
            None
        } else {
            Some(p)
        }
    }

    /// 读取单个非空环境变量值（自动过滤空字符串与纯空白字符）
    pub fn get<K: AsRef<OsStr>>(key: K) -> Option<String> {
        env::var(key).ok().and_then(|val| {
            let trimmed = val.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
    }

    /// 依次尝试读取一组环境变量候选键，返回首个非空值（支持多级回退）
    pub fn get_first<K: AsRef<OsStr>>(keys: &[K]) -> Option<String> {
        keys.iter().find_map(Self::get)
    }

    /// 读取环境变量并解析为指定类型 T: FromStr
    pub fn parse<T: FromStr, K: AsRef<OsStr>>(key: K) -> Option<T> {
        Self::get(key).and_then(|s| s.parse().ok())
    }

    /// 依次尝试读取一组环境变量并解析为指定类型 T: FromStr
    pub fn parse_first<T: FromStr, K: AsRef<OsStr>>(keys: &[K]) -> Option<T> {
        keys.iter().find_map(Self::parse)
    }

    /// 读取环境变量并转换为 PathBuf
    pub fn get_path<K: AsRef<OsStr>>(key: K) -> Option<PathBuf> {
        Self::get(key).map(PathBuf::from)
    }

    /// 依次尝试读取一组环境变量并转换为 PathBuf
    pub fn get_path_first<K: AsRef<OsStr>>(keys: &[K]) -> Option<PathBuf> {
        keys.iter().find_map(Self::get_path)
    }

    /// 读取布尔型环境变量（支持 1/true/yes/on 与 0/false/no/off）
    pub fn get_bool<K: AsRef<OsStr>>(key: K) -> Option<bool> {
        Self::get(key).and_then(|s| match s.to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
    }

    /// 依次尝试读取一组环境变量并解析为布尔值
    pub fn get_bool_first<K: AsRef<OsStr>>(keys: &[K]) -> Option<bool> {
        keys.iter().find_map(Self::get_bool)
    }

    /// 构造针对单个变量键的链式查询对象
    pub fn key<K: AsRef<OsStr>>(key: K) -> EnvKey<K> {
        EnvKey(key)
    }

    /// 构造针对候选键切片的多级回退链式查询对象
    pub fn first<'a, K: AsRef<OsStr>>(keys: &'a [K]) -> EnvKeys<'a, K> {
        EnvKeys(keys)
    }
}

/// 单环境变量键查询器
pub struct EnvKey<K>(K);

impl<K: AsRef<OsStr>> EnvKey<K> {
    pub fn get(&self) -> Option<String> {
        Env::get(&self.0)
    }

    pub fn parse<T: FromStr>(&self) -> Option<T> {
        Env::parse(&self.0)
    }

    pub fn get_path(&self) -> Option<PathBuf> {
        Env::get_path(&self.0)
    }

    pub fn get_bool(&self) -> Option<bool> {
        Env::get_bool(&self.0)
    }
}

/// 多环境变量回退查询器
pub struct EnvKeys<'a, K>(&'a [K]);

impl<'a, K: AsRef<OsStr>> EnvKeys<'a, K> {
    pub fn get(&self) -> Option<String> {
        Env::get_first(self.0)
    }

    pub fn parse<T: FromStr>(&self) -> Option<T> {
        Env::parse_first(self.0)
    }

    pub fn get_path(&self) -> Option<PathBuf> {
        Env::get_path_first(self.0)
    }

    pub fn get_bool(&self) -> Option<bool> {
        Env::get_bool_first(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Env;
    use std::{
        env,
        path::{Path, PathBuf},
    };

    #[test]
    fn test_non_empty_str_and_path() {
        assert_eq!(Env::non_empty_str(""), None);
        assert_eq!(Env::non_empty_str("   "), None);
        assert_eq!(Env::non_empty_str("  hello  "), Some("hello"));

        assert_eq!(Env::non_empty_path(Path::new("")), None);
        assert_eq!(
            Env::non_empty_path(Path::new("/tmp/test")),
            Some(Path::new("/tmp/test"))
        );
    }

    #[test]
    fn test_get_env_non_empty() {
        let key = "TEST_NIXCACHE_ENV_NON_EMPTY_KEY";
        unsafe {
            env::set_var(key, "  value123  ");
        }
        assert_eq!(Env::get(key), Some("value123".to_string()));
        assert_eq!(Env::key(key).get(), Some("value123".to_string()));

        unsafe {
            env::set_var(key, "   ");
        }
        assert_eq!(Env::get(key), None);
        assert_eq!(Env::key(key).get(), None);

        unsafe {
            env::remove_var(key);
        }
        assert_eq!(Env::get(key), None);
    }

    #[test]
    fn test_get_env_first_non_empty() {
        let k1 = "TEST_MULTI_K1";
        let k2 = "TEST_MULTI_K2";
        let k3 = "TEST_MULTI_K3";

        unsafe {
            env::set_var(k1, "");
            env::set_var(k2, "val2");
            env::set_var(k3, "val3");
        }
        assert_eq!(Env::get_first(&[k1, k2, k3]), Some("val2".to_string()));
        assert_eq!(Env::first(&[k1, k2, k3]).get(), Some("val2".to_string()));

        unsafe {
            env::set_var(k1, "val1");
        }
        assert_eq!(Env::get_first(&[k1, k2, k3]), Some("val1".to_string()));
        assert_eq!(Env::first(&[k1, k2, k3]).get(), Some("val1".to_string()));

        unsafe {
            env::remove_var(k1);
            env::remove_var(k2);
            env::remove_var(k3);
        }
    }

    #[test]
    fn test_parse_env_var_and_first() {
        let k1 = "TEST_PARSE_U64_1";
        let k2 = "TEST_PARSE_U64_2";

        unsafe {
            env::set_var(k1, "invalid");
            env::set_var(k2, "12345");
        }
        assert_eq!(Env::parse::<u64, _>(k1), None);
        assert_eq!(Env::key(k1).parse::<u64>(), None);
        assert_eq!(Env::parse::<u64, _>(k2), Some(12345u64));
        assert_eq!(Env::key(k2).parse::<u64>(), Some(12345u64));
        assert_eq!(Env::parse_first::<u64, _>(&[k1, k2]), Some(12345u64));
        assert_eq!(Env::first(&[k1, k2]).parse::<u64>(), Some(12345u64));

        unsafe {
            env::remove_var(k1);
            env::remove_var(k2);
        }
    }

    #[test]
    fn test_get_env_bool() {
        let k = "TEST_BOOL_ENV";

        for true_val in &["1", "true", "TRUE", "yes", "YES", "on", "On"] {
            unsafe {
                env::set_var(k, true_val);
            }
            assert_eq!(Env::get_bool(k), Some(true), "failed on {}", true_val);
            assert_eq!(Env::key(k).get_bool(), Some(true));
        }

        for false_val in &["0", "false", "FALSE", "no", "NO", "off", "Off"] {
            unsafe {
                env::set_var(k, false_val);
            }
            assert_eq!(Env::get_bool(k), Some(false), "failed on {}", false_val);
            assert_eq!(Env::key(k).get_bool(), Some(false));
        }

        unsafe {
            env::set_var(k, "invalid");
        }
        assert_eq!(Env::get_bool(k), None);

        let k2 = "TEST_BOOL_ENV_2";
        unsafe {
            env::set_var(k2, "1");
        }
        assert_eq!(Env::get_bool_first(&[k, k2]), Some(true));
        assert_eq!(Env::first(&[k, k2]).get_bool(), Some(true));

        unsafe {
            env::remove_var(k);
            env::remove_var(k2);
        }
    }

    #[test]
    fn test_get_env_path() {
        let k1 = "TEST_PATH_ENV_1";
        let k2 = "TEST_PATH_ENV_2";
        unsafe {
            env::set_var(k1, "");
            env::set_var(k2, "/tmp/foo/bar");
        }
        assert_eq!(Env::get_path(k2), Some(PathBuf::from("/tmp/foo/bar")));
        assert_eq!(Env::key(k2).get_path(), Some(PathBuf::from("/tmp/foo/bar")));
        assert_eq!(
            Env::get_path_first(&[k1, k2]),
            Some(PathBuf::from("/tmp/foo/bar"))
        );
        assert_eq!(
            Env::first(&[k1, k2]).get_path(),
            Some(PathBuf::from("/tmp/foo/bar"))
        );
        unsafe {
            env::remove_var(k1);
            env::remove_var(k2);
        }
    }
}
