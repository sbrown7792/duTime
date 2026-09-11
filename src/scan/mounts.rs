//! Mount table awareness: deciding what the scanner must refuse to descend into.
//!
//! Restricting a walk to one filesystem needs three independent layers, and all
//! three are load-bearing. Each was confirmed necessary by measurement on a
//! stock Ubuntu 24.04 box:
//!
//! 1. **`st_dev` comparison** catches the ordinary case — 60 squashfs snap
//!    mounts and the vfat `/boot/efi` all differ from the root device.
//!
//! 2. **Bind mounts sharing a device.** `st_dev` alone is *not sufficient*:
//!    `/usr/share/hunspell` and `/var/snap/firefox/common/host-hunspell` both
//!    report `st_dev` 66306. `du -x` happily walks into both and double-counts
//!    the bytes. Only `/proc/self/mountinfo` reveals that the second is a bind
//!    of the first.
//!
//! 3. **Filesystem-type denylist**, regardless of device. `autofs` is the
//!    dangerous one: merely *traversing* an autofs mountpoint triggers a mount,
//!    so a 1.5-second local scan turns into a 30-second hang waiting on an NFS
//!    server that may not even be up.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Filesystem types never worth walking: kernel-virtual, duplicative of content
/// counted elsewhere, or actively hazardous to traverse.
pub const DENY_FSTYPES: &[&str] = &[
    "proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2", "tracefs", "debugfs",
    "securityfs", "configfs", "bpf", "pstore", "efivarfs", "fusectl", "mqueue", "hugetlbfs",
    "binfmt_misc", "autofs", "nsfs", "squashfs", "overlay", "ramfs", "fuse.portal",
    "fuse.gvfsd-fuse", "rpc_pipefs", "selinuxfs",
];

/// Fstype prefixes that are denied along with any suffix (`fuse.*`, `nfs4`, …).
const DENY_PREFIXES: &[&str] = &["fuse.", "nfs", "cifs", "smb", "ceph", "glusterfs", "afs"];

/// Filesystems where the *server* decides what you may read.
///
/// This distinction matters more than it looks. On a local filesystem the
/// kernel performs the permission check, so `CAP_DAC_READ_SEARCH` bypasses
/// it and duTime can read anything. On these, authorization happens at the
/// other end of a network connection against the numeric uid/gid the client
/// presents — NFS `sec=sys` sends exactly that and nothing else. The server
/// has no idea the client process holds a capability, so the capability buys
/// nothing at all, and `root_squash` (on by default nearly everywhere) means
/// running as root is actively worse than running as a normal user.
///
/// Getting this wrong sends someone to check capabilities that were never
/// going to help, so duTime names the filesystem type instead.
pub const SERVER_AUTHORIZED_FSTYPES: &[&str] = &["nfs", "cifs", "smb", "ceph", "afs", "glusterfs"];

/// Does this filesystem type do its permission checks on a remote server?
pub fn is_server_authorized(fstype: &str) -> bool {
    SERVER_AUTHORIZED_FSTYPES.iter().any(|p| fstype.starts_with(p))
}

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_id: i64,
    pub parent_id: i64,
    /// `major:minor` of the source device.
    pub dev: (u32, u32),
    /// Which subtree *of the source filesystem* is exposed here. `/` for a
    /// normal mount; something deeper for a bind mount of a subdirectory.
    pub root: PathBuf,
    pub mount_point: PathBuf,
    pub fstype: String,
}

impl MountEntry {
    pub fn dev_id(&self) -> u64 {
        // Matches glibc's makedev() encoding, which is what st_dev carries.
        let (maj, min) = self.dev;
        let (maj, min) = (maj as u64, min as u64);
        ((maj & 0xfff) << 8) | (min & 0xff) | ((maj & !0xfff) << 32) | ((min & !0xff) << 12)
    }

    pub fn fstype_denied(&self) -> bool {
        DENY_FSTYPES.contains(&self.fstype.as_str())
            || DENY_PREFIXES.iter().any(|p| self.fstype.starts_with(p))
    }
}

/// The parsed mount table, with the derived "do not descend here" set.
#[derive(Debug, Clone, Default)]
pub struct MountTable {
    pub entries: Vec<MountEntry>,
}

impl MountTable {
    pub fn load() -> std::io::Result<Self> {
        let raw = std::fs::read_to_string("/proc/self/mountinfo")?;
        Ok(Self::parse(&raw))
    }

    /// Parse mountinfo. Format (see `proc(5)`):
    ///
    /// ```text
    /// 36 35 98:0 /mnt1 /mnt2 rw,noatime - ext3 /dev/sda1 rw,errors=continue
    /// |  |  |    |     |     |            |  |
    /// 0  1  2    3     4     5..       sep  fstype
    /// ```
    ///
    /// Fields 6..n are optional and variable in number, terminated by a literal
    /// `-`, so the fstype must be located relative to that separator rather
    /// than by a fixed index.
    pub fn parse(raw: &str) -> Self {
        let mut entries = Vec::new();
        for line in raw.lines() {
            let Some(sep) = line.split_whitespace().position(|f| f == "-") else {
                continue;
            };
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < sep + 2 || sep < 6 {
                continue;
            }
            let Some((maj, min)) = f[2].split_once(':') else {
                continue;
            };
            let (Ok(maj), Ok(min)) = (maj.parse::<u32>(), min.parse::<u32>()) else {
                continue;
            };
            let (Ok(mount_id), Ok(parent_id)) = (f[0].parse::<i64>(), f[1].parse::<i64>()) else {
                continue;
            };
            entries.push(MountEntry {
                mount_id,
                parent_id,
                dev: (maj, min),
                root: PathBuf::from(unescape(f[3])),
                mount_point: PathBuf::from(unescape(f[4])),
                fstype: unescape(f[sep + 1]),
            });
        }
        Self { entries }
    }

    /// Mount points the scanner must never descend into, given a root device.
    ///
    /// Covers both hazardous/virtual filesystems and same-device bind mounts
    /// that would otherwise be counted twice.
    pub fn skip_set(&self, root_dev: u64, root_path: &Path) -> HashSet<PathBuf> {
        let mut skip = HashSet::new();

        for e in &self.entries {
            if e.fstype_denied() {
                skip.insert(e.mount_point.clone());
            }
        }

        // Same-device bind mounts. Within one device, if two mount points
        // expose overlapping source subtrees, everything after the first is a
        // duplicate view of bytes already counted.
        //
        // Ordering by mount point depth then path makes the choice of "first"
        // deterministic across runs — otherwise which copy gets counted would
        // depend on mountinfo ordering and the totals would flap between scans.
        let mut same_dev: Vec<&MountEntry> = self
            .entries
            .iter()
            .filter(|e| e.dev_id() == root_dev && !e.fstype_denied())
            .collect();
        same_dev.sort_by(|a, b| {
            a.mount_point
                .components()
                .count()
                .cmp(&b.mount_point.components().count())
                .then_with(|| a.mount_point.cmp(&b.mount_point))
        });

        let mut claimed: Vec<PathBuf> = Vec::new();
        for e in same_dev {
            // Is this mount's source subtree already reachable through an
            // earlier mount point we are going to walk?
            let dup = claimed.iter().any(|c| e.root.starts_with(c));
            if dup && !e.mount_point.starts_with(root_path.join("__never__")) {
                // Only a duplicate if we'd actually reach it during this walk.
                if e.mount_point != *root_path {
                    skip.insert(e.mount_point.clone());
                    continue;
                }
            }
            claimed.push(e.root.clone());
        }

        skip.remove(root_path);
        // Only mounts we could actually reach from this root matter. Reporting
        // all 87 system mount points when scanning $HOME is noise that makes a
        // genuinely surprising skip impossible to notice.
        skip.retain(|p| p.starts_with(root_path));
        skip
    }

    pub fn find_mount_for(&self, path: &Path) -> Option<&MountEntry> {
        self.entries
            .iter()
            .filter(|e| path.starts_with(&e.mount_point))
            .max_by_key(|e| e.mount_point.components().count())
    }
}

/// mountinfo octal-escapes space, tab, newline and backslash.
fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let oct = &s[i + 1..i + 4];
            if let Ok(v) = u8::from_str_radix(oct, 8) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
25 30 0:23 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
26 30 0:24 / /sys rw,nosuid,nodev,noexec,relatime shared:2 - sysfs sysfs rw
30 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw,errors=remount-ro
48 30 259:1 / /boot/efi rw,relatime - vfat /dev/nvme0n1p1 rw
99 30 0:57 / /snap/core22/2045 ro,nodev,relatime shared:55 - squashfs /dev/loop3 ro
120 30 259:2 /usr/share/hunspell /var/snap/firefox/common/host-hunspell ro,nosuid,nodev - ext4 /dev/nvme0n1p2 rw
140 30 0:99 / /mnt/nas rw,relatime - nfs4 10.0.0.5:/export rw
150 30 0:52 / /net rw,relatime - autofs systemd-1 rw";

    #[test]
    fn parses_variable_optional_fields() {
        let mt = MountTable::parse(SAMPLE);
        assert_eq!(mt.entries.len(), 8);
        // Line 25 has one optional field (shared:12); line 48 has none.
        assert_eq!(mt.entries[0].fstype, "proc");
        assert_eq!(mt.entries[3].fstype, "vfat");
        assert_eq!(mt.entries[3].mount_point, PathBuf::from("/boot/efi"));
    }

    #[test]
    fn dev_id_matches_makedev_encoding() {
        let mt = MountTable::parse(SAMPLE);
        let root = mt.entries.iter().find(|e| e.mount_point == Path::new("/")).unwrap();
        // 259:2 -> (259 & 0xfff) << 8 | 2 == 66306, the value measured via stat.
        assert_eq!(root.dev_id(), 66306);
    }

    #[test]
    fn denies_hazardous_and_virtual_fstypes() {
        let mt = MountTable::parse(SAMPLE);
        let skip = mt.skip_set(66306, Path::new("/"));
        for p in ["/proc", "/sys", "/snap/core22/2045", "/mnt/nas", "/net"] {
            assert!(skip.contains(Path::new(p)), "{p} should be skipped");
        }
    }

    #[test]
    fn detects_same_device_bind_mount() {
        // The measured trap: both are ext4 on 259:2, so st_dev alone does not
        // distinguish them and `du -x` double-counts the hunspell dictionaries.
        let mt = MountTable::parse(SAMPLE);
        let skip = mt.skip_set(66306, Path::new("/"));
        assert!(
            skip.contains(Path::new("/var/snap/firefox/common/host-hunspell")),
            "same-device bind mount must be skipped to avoid double counting"
        );
        assert!(!skip.contains(Path::new("/")));
    }

    #[test]
    fn unescapes_octal_sequences() {
        assert_eq!(unescape("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape("/plain/path"), "/plain/path");
    }
}

#[cfg(test)]
mod nfs_tests {
    use super::*;

    /// `nfs4` is on the denylist so that a scan of `/` does not wander onto a
    /// NAS and hang. But a root the operator configured *is* the NAS, and an
    /// explicit request must win over a blanket rule — otherwise duTime
    /// silently records the mount point and nothing under it.
    #[test]
    fn an_nfs_root_is_still_walked_when_it_is_the_root() {
        let raw = "\
36 25 0:52 / /media/nextcloud rw,relatime shared:1 - nfs4 192.168.0.250:/volume1/nextcloud rw
25 1 259:2 / / rw,relatime shared:2 - ext4 /dev/nvme0n1p2 rw";
        let mt = MountTable::parse(raw);
        let nfs = mt.entries.iter().find(|e| e.fstype == "nfs4").unwrap();
        assert!(nfs.fstype_denied(), "nfs4 should be denied in general");

        let root = Path::new("/media/nextcloud");
        let skip = mt.skip_set(nfs.dev_id(), root);
        assert!(
            !skip.contains(root),
            "the configured root was skipped because of its own filesystem type: {skip:?}"
        );
        assert!(skip.is_empty(), "nothing under this root should be skipped: {skip:?}");
    }

    /// The same mount reached while scanning `/` must still be skipped: that
    /// is a walk across the network nobody asked for.
    #[test]
    fn the_same_nfs_mount_is_skipped_when_scanning_slash() {
        let raw = "\
36 25 0:52 / /media/nextcloud rw,relatime shared:1 - nfs4 192.168.0.250:/volume1/nextcloud rw
25 1 259:2 / / rw,relatime shared:2 - ext4 /dev/nvme0n1p2 rw";
        let mt = MountTable::parse(raw);
        let ext4 = mt.entries.iter().find(|e| e.fstype == "ext4").unwrap();
        let skip = mt.skip_set(ext4.dev_id(), Path::new("/"));
        assert!(skip.contains(Path::new("/media/nextcloud")), "{skip:?}");
    }
}
