//! Immutable environment snapshot handed to a carrier runtime.
//!
//! Runtimes never read the ambient process environment directly: the assembly
//! passes a snapshot so tests and the daemon share one code path, and so a
//! carrier cannot inherit a credential it was not explicitly granted.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeEnv {
    vars: BTreeMap<String, OsString>,
}

impl RuntimeEnv {
    pub fn from_process() -> Self {
        Self::from_pairs(std::env::vars_os())
    }

    pub fn from_strs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<OsString>,
    {
        Self::from_pairs(pairs.into_iter().map(|(k, v)| (OsString::from(k.into()), v.into())))
    }

    pub fn from_pairs<I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        Self {
            vars: pairs
                .into_iter()
                .map(|(key, value)| (key.to_string_lossy().into_owned(), value))
                .collect(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&OsStr> {
        self.vars.get(key).map(OsString::as_os_str)
    }

    /// UTF-8 view used where the 2.1 source compares plain strings.
    pub fn get_str(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|value| value.to_str()).map(str::to_owned)
    }

    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<OsString>) {
        self.vars.insert(key.into(), value.into());
    }

    /// Copy only the listed keys, preserving the caller's whitelist order.
    pub fn pick(&self, keys: &[&str]) -> Self {
        let mut picked = BTreeMap::new();
        for key in keys {
            if let Some(value) = self.vars.get(*key) {
                picked.insert((*key).to_string(), value.clone());
            }
        }
        Self { vars: picked }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &OsString)> {
        self.vars.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Flattened `PATH` entry for `find_executable`.
    pub fn path_map(&self) -> BTreeMap<String, String> {
        self.vars
            .iter()
            .filter_map(|(key, value)| {
                value
                    .to_str()
                    .map(|text| (key.clone(), text.to_string()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeEnv;

    #[test]
    fn pick_keeps_only_whitelisted_keys_and_insert_overrides() {
        let env = RuntimeEnv::from_strs([
            ("PATH", "/bin"),
            ("LANG", "C.UTF-8"),
            ("ANTHROPIC_API_KEY", "must-not-leak"),
        ]);
        let picked = env.pick(&["LANG", "PATH", "MISSING"]);
        assert_eq!(picked.get_str("LANG").as_deref(), Some("C.UTF-8"));
        assert_eq!(picked.get_str("PATH").as_deref(), Some("/bin"));
        assert_eq!(picked.get_str("ANTHROPIC_API_KEY"), None);
        assert_eq!(picked.iter().count(), 2);

        let mut mutable = picked.clone();
        mutable.insert("HOME", "/private/home");
        assert_eq!(mutable.get_str("HOME").as_deref(), Some("/private/home"));
        assert_eq!(mutable.get_str("LANG").as_deref(), Some("C.UTF-8"));
    }
}
