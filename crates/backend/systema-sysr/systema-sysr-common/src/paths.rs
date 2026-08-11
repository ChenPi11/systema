//! cgroup path mapping and value conversions.
//!
//! All functions here are pure and platform-neutral so they can be unit
//! tested without a cgroup filesystem.  They implement the naming rules
//! systemd uses for slices: a slice unit name encodes its position in the
//! hierarchy, with each `-` separating a parent/child level.

use std::fmt::Write;

/// Mount point of the unified cgroup v2 hierarchy.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The root slice unit name (systemd's `-.slice`).
pub const ROOT_SLICE_NAME: &str = "-.slice";

/// Split a slice unit name into its hierarchy components.
///
/// `"system.slice"` → `["system"]`, `"foo-bar.slice"` → `["foo", "bar"]`,
/// `"-.slice"` → `[]` (the root slice).
pub fn slice_name_components(name: &str) -> Option<Vec<String>> {
    let stem = name.strip_suffix(".slice")?;
    if stem.is_empty() || stem == "-" {
        return Some(vec![]);
    }
    Some(stem.split('-').map(|s| s.to_string()).collect())
}

/// Resolve the cgroup filesystem path of a slice unit's own cgroup.
///
/// The leaf directory of a nested slice is named after the *last* component
/// only: `foo-bar.slice` lives in `.../foo.slice/bar.slice`.
pub fn slice_cgroup_path(slice_name: &str) -> String {
    if slice_name == ROOT_SLICE_NAME {
        return CGROUP_ROOT.to_string();
    }
    let mut path = CGROUP_ROOT.to_string();
    if let Some(stem) = slice_name.strip_suffix(".slice") {
        for comp in stem.split('-') {
            path.push('/');
            path.push_str(comp);
            path.push_str(".slice");
        }
    } else {
        path.push('/');
        path.push_str(slice_name);
    }
    path
}

/// Resolve the cgroup filesystem path of a non-slice unit (service, scope)
/// placed inside the slice named `slice_name`.
///
/// The unit's cgroup is a child of its parent slice's cgroup and is named
/// after the full unit name (e.g. `system.slice/sshd.service`).
pub fn unit_cgroup_path(slice_name: &str, unit_name: &str) -> String {
    format!("{}/{}", slice_cgroup_path(slice_name), unit_name)
}

/// Parse a `CPUQuota=` value into a percentage as `f64`.
///
/// Accepts `"50"`, `"50%"` and decimal quotas such as `"50.5%"`.
/// `infinity` (no quota) and unparseable input yield `None`.
pub fn parse_cpu_quota_percent(value: &str) -> Option<f64> {
    let s = value.trim();
    if s.is_empty() {
        return None;
    }
    let s = s.strip_suffix('%').unwrap_or(s).trim();
    if s.eq_ignore_ascii_case("infinity") || s.eq_ignore_ascii_case("inf") {
        return None;
    }
    let pct: f64 = s.parse().ok()?;
    if !(pct.is_finite()) || pct <= 0.0 {
        return None;
    }
    Some(pct)
}

/// Convert a `CPUQuota=` value into the cgroup v2 `cpu.max` payload
/// (`"quota_us period_us"`), with the period fixed at 100ms.
pub fn cpu_quota_to_cpu_max(value: &str) -> Option<String> {
    let pct = parse_cpu_quota_percent(value)?;
    // A percentage is a fraction of one full period: 50% → 50ms quota on a
    // 100ms period.
    let quota_us = (pct / 100.0 * super::CPU_MAX_PERIOD_US as f64) as u64;
    if quota_us == 0 {
        return None;
    }
    Some(format!("{quota_us} {}", super::CPU_MAX_PERIOD_US))
}

/// Parse a memory size string (systemd `parse_size`) into bytes.
///
/// Supports plain byte counts and the `K`/`M`/`G`/`T` suffixes with decimal
/// prefixes, e.g. `"512M"`, `"1.5G"`, `"1048576"`.
pub fn parse_memory_size(value: &str) -> Option<u64> {
    let s = value.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.as_bytes().last() {
        Some(c) if c.is_ascii_alphabetic() => {
            let (num, suffix) = s.split_at(s.len() - 1);
            let mult = match suffix.to_ascii_uppercase().as_str() {
                "K" => 1u64 << 10,
                "M" => 1u64 << 20,
                "G" => 1u64 << 30,
                "T" => 1u64 << 40,
                _ => return None,
            };
            (num, mult)
        }
        _ => (s, 1),
    };
    let n: f64 = num.trim().parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    Some((n * mult as f64) as u64)
}

/// Render a memory byte count back to a decimal `cpu.max`-style string
/// (used for the memory limits on the cgroup v2 files, which are in bytes).
pub fn bytes_to_string(bytes: u64) -> String {
    let mut buf = String::new();
    let _ = write!(buf, "{bytes}");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_components() {
        assert_eq!(slice_name_components("system.slice"), Some(vec!["system".to_string()]));
        assert_eq!(
            slice_name_components("foo-bar.slice"),
            Some(vec!["foo".to_string(), "bar".to_string()])
        );
        assert_eq!(slice_name_components("-.slice"), Some(vec![]));
        assert_eq!(slice_name_components("nope"), None);
    }

    #[test]
    fn slice_paths() {
        assert_eq!(slice_cgroup_path("system.slice"), "/sys/fs/cgroup/system.slice");
        assert_eq!(
            slice_cgroup_path("foo-bar.slice"),
            "/sys/fs/cgroup/foo.slice/bar.slice"
        );
        assert_eq!(slice_cgroup_path("-.slice"), "/sys/fs/cgroup");
        assert_eq!(
            unit_cgroup_path("system.slice", "sshd.service"),
            "/sys/fs/cgroup/system.slice/sshd.service"
        );
        assert_eq!(
            unit_cgroup_path("system-foo.slice", "sshd.service"),
            "/sys/fs/cgroup/system.slice/foo.slice/sshd.service"
        );
    }

    #[test]
    fn quota_parsing() {
        assert_eq!(parse_cpu_quota_percent("50%"), Some(50.0));
        assert_eq!(parse_cpu_quota_percent("50"), Some(50.0));
        assert_eq!(parse_cpu_quota_percent("50.5%"), Some(50.5));
        assert_eq!(parse_cpu_quota_percent("infinity"), None);
        assert_eq!(parse_cpu_quota_percent(""), None);
        assert_eq!(parse_cpu_quota_percent("junk"), None);
    }

    #[test]
    fn quota_to_cpu_max() {
        assert_eq!(cpu_quota_to_cpu_max("50%"), Some("50000 100000".to_string()));
        assert_eq!(cpu_quota_to_cpu_max("100%"), Some("100000 100000".to_string()));
        assert_eq!(cpu_quota_to_cpu_max("10.5%"), Some("10500 100000".to_string()));
        assert_eq!(cpu_quota_to_cpu_max("infinity"), None);
    }

    #[test]
    fn memory_sizes() {
        assert_eq!(parse_memory_size("512M"), Some(512 * 1024 * 1024));
        assert_eq!(parse_memory_size("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_memory_size("1.5G"), Some(1_610_612_736));
        assert_eq!(parse_memory_size("1048576"), Some(1_048_576));
        assert_eq!(parse_memory_size("2T"), Some(2u64 << 40));
        assert_eq!(parse_memory_size(""), None);
        assert_eq!(parse_memory_size("x"), None);
        assert_eq!(parse_memory_size("-5M"), None);
    }

    #[test]
    fn bytes_to_string_roundtrip() {
        assert_eq!(bytes_to_string(1_048_576), "1048576");
    }
}
